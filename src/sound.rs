//! The Mac's system sound to the client on RDPSND (design.md "声音"). The source is a
//! CoreAudio process tap that mutes the Mac while the client listens (tap.rs), or, when that
//! cannot be had, ScreenCaptureKit's audio output of the same SCStream that carries the picture
//! (capture.rs: `capturesAudio`, 48 kHz stereo Float32, usually planar); here it becomes
//! 16-bit interleaved PCM in 20 ms waves, keel-rdp's shape (keelOS docs/71 §11), sent
//! through the server's event channel. Silence inside a sound (pauses in speech or music) is
//! sent like any wave; only after HANGOVER of unbroken silence do waves stop until the next
//! sound. Skipping every silent wave (as before) left holes the client took for a starved
//! network: "ran dry" every few seconds and its jitter buffer grown to 400 ms (user 2026-11-09,
//! 32-39 skipped waves per 10 s of a video ≈ the client's 4 × 150 ms "gaps").
//!
//! Opus (user 2026-11-09: "1.5M太多了"): offered first, so a client that takes it (kRDPclient,
//! IronRDP's player: one Opus packet per wave, 48 kHz stereo) gets each 20 ms wave as one
//! Opus packet, about 96 kbit/s instead of PCM's 1536. A client without Opus (mstsc) settles
//! on PCM as before. MACRDP_OPUS=0 offers PCM only; MACRDP_OPUS_KBPS sets the bit rate.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

pub const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const WAVE_MS: usize = 20;
/// samples (both channels) per wave
const WAVE_SAMPLES: usize = RATE as usize * CHANNELS * WAVE_MS / 1000;
/// |sample| at or below this (of 32767) counts as silence
const SILENCE: i16 = 2;
/// Unbroken silence after which waves stop (the sound has ended): 2 s of 20 ms waves.
const HANGOVER: u32 = 100;
/// the same, as Float32 (tap_silent)
const LOUD: f32 = 2.0 / 32767.0;
/// Opus bit rate when MACRDP_OPUS_KBPS does not say: transparent for most listeners at 48 kHz
/// stereo, a sixteenth of PCM.
const OPUS_KBPS: i32 = 96;
/// The RDPSND wave PDU before version 8 carries the first 4 bytes apart: a shorter Opus packet
/// (only near-silence) is dropped rather than padded.
const MIN_WAVE: usize = 4;

/// The formats' descriptions must match the client's byte for byte (IronRDP compares channels,
/// rate, bits and, for a codec, the average rate and block align too): kRDPclient's Opus entry
/// says 48 kHz, 2 channels, 16 bits, 192000 bytes/s, align 4, as its PCM one does.
fn format(format: WaveFormat) -> AudioFormat {
    AudioFormat {
        format,
        n_channels: CHANNELS as u16,
        n_samples_per_sec: RATE,
        n_avg_bytes_per_sec: RATE * 4,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    }
}

fn opus_offered() -> bool {
    std::env::var("MACRDP_OPUS").as_deref() != Ok("0")
}

fn opus_encoder() -> Result<opus2::Encoder, opus2::Error> {
    let kbps = std::env::var("MACRDP_OPUS_KBPS").ok().and_then(|v| v.parse::<i32>().ok()).filter(|k| (6..=510).contains(k));
    let mut e = opus2::Encoder::new(RATE, opus2::Channels::Stereo, opus2::Application::Audio)?;
    e.set_bitrate(opus2::Bitrate::Bits(kbps.unwrap_or(OPUS_KBPS) * 1000))?;
    e.set_vbr(true)?;
    Ok(e)
}

struct Inner {
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// the client negotiated a format and wants waves (start .. stop)
    on: AtomicBool,
    t0: Instant,
    /// interleaved i16 not yet sent (less than one wave)
    pending: Mutex<Vec<i16>>,
    /// Some while the negotiated format is Opus: each wave is encoded with it
    opus: Mutex<Option<opus2::Encoder>>,
    stats: Mutex<Stats>,
    /// tap.rs feeds the waves: ScreenCaptureKit's samples are only watched, not sent
    tapped: AtomicBool,
    /// kRDPclient's sound switch (SOUND on keelOS::Video, video.rs): off = no waves, RDPSND
    /// stays up so switching back on needs no renegotiation. On until the client says otherwise.
    client_on: AtomicBool,
    /// silent waves in a row (wave(): sent up to HANGOVER, then skipped)
    silent_run: std::sync::atomic::AtomicU32,
    /// ms since t0 + 1 of the last audible callback from each source (0: none yet); a tap
    /// that stays silent while ScreenCaptureKit hears sound was refused (tap_silent)
    capture_loud: AtomicU64,
    tap_loud: AtomicU64,
    tap_since: AtomicU64,
}

/// What happened since the last line in the log (every 10 s while sound runs): to tell a
/// bursty source (few, big callbacks), dropped waves, and clipping apart (user 2026-11-09:
/// "看视频有点破音").
#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    callbacks: u32,
    frames: u64,
    frames_max: usize,
    waves: u32,
    silent: u32,
    clipped: u64,
    /// bytes of the waves sent (PCM or Opus), for the bit rate in the log
    bytes: u64,
}

/// One connection's sound. Cloned between the RDPSND handler and the capture's audio callback.
#[derive(Clone)]
pub struct Sound(Arc<Inner>);

pub struct Factory(Sound);

impl Sound {
    pub fn new() -> (Self, Factory) {
        let s = Sound(Arc::new(Inner {
            sender: Mutex::new(None),
            on: AtomicBool::new(false),
            t0: Instant::now(),
            pending: Mutex::new(Vec::with_capacity(WAVE_SAMPLES * 2)),
            opus: Mutex::new(None),
            stats: Mutex::new(Stats::default()),
            tapped: AtomicBool::new(false),
            client_on: AtomicBool::new(true),
            silent_run: std::sync::atomic::AtomicU32::new(0),
            capture_loud: AtomicU64::new(0),
            tap_loud: AtomicU64::new(0),
            tap_since: AtomicU64::new(0),
        }));
        (s.clone(), Factory(s))
    }

    /// The client takes sound now: RDPSND started and its switch (if any) is on.
    pub fn on(&self) -> bool {
        self.0.on.load(Ordering::Relaxed) && self.0.client_on.load(Ordering::Relaxed)
    }

    /// kRDPclient's sound switch (video.rs SOUND).
    pub fn set_client(&self, on: bool) {
        if self.0.client_on.swap(on, Ordering::Relaxed) != on {
            info!(on, "sound: the client's switch");
            self.0.pending.lock().unwrap().clear();
        }
    }

    fn now_ms(&self) -> u64 {
        self.0.t0.elapsed().as_millis() as u64 + 1
    }

    /// The tap (tap.rs) takes over from ScreenCaptureKit, or gives back (false).
    pub fn set_tapped(&self, on: bool) {
        if on {
            self.0.tap_since.store(self.now_ms(), Ordering::Relaxed);
        }
        self.0.tapped.store(on, Ordering::Relaxed);
        self.0.pending.lock().unwrap().clear();
    }

    /// The running tap delivers nothing audible for 3 s while ScreenCaptureKit hears sound
    /// right now: the "System Audio Recording" permission was refused (the HAL then hands the
    /// tap silence, no error) and the Mac was never muted either.
    pub fn tap_silent(&self) -> bool {
        let now = self.now_ms();
        let cap = self.0.capture_loud.load(Ordering::Relaxed);
        let heard = self.0.tap_loud.load(Ordering::Relaxed).max(self.0.tap_since.load(Ordering::Relaxed));
        self.0.tapped.load(Ordering::Relaxed) && cap != 0 && now - cap < 1000 && now - heard > 3000
    }

    /// ScreenCaptureKit's samples (capture.rs): sent unless the tap runs.
    pub fn push_capture(&self, data: &[f32], frames: usize, channels: usize, planar: bool) {
        if data.iter().any(|x| x.abs() > LOUD) {
            self.0.capture_loud.store(self.now_ms(), Ordering::Relaxed);
        }
        if !self.0.tapped.load(Ordering::Relaxed) {
            self.push_f32(data, frames, channels, planar);
        }
    }

    /// The tap's samples (tap.rs): interleaved stereo at RATE.
    pub fn push_tap(&self, data: &[f32], frames: usize) {
        if data.iter().any(|x| x.abs() > LOUD) {
            self.0.tap_loud.store(self.now_ms(), Ordering::Relaxed);
        }
        if self.0.tapped.load(Ordering::Relaxed) {
            self.push_f32(data, frames, CHANNELS, false);
        }
    }

    /// A VM's samples (vmsound.rs), interleaved stereo at RATE: this connection shows the VM,
    /// so neither the tap nor ScreenCaptureKit runs for it.
    pub fn push_vm(&self, data: &[f32]) {
        self.push_f32(data, data.len() / CHANNELS, CHANNELS, false);
    }

    /// Float32 samples from the capture, `channels` wide, planar (channel after channel) or
    /// interleaved; `frames` samples per channel. Converted, cut into waves, sent.
    fn push_f32(&self, data: &[f32], frames: usize, channels: usize, planar: bool) {
        if !self.on() || frames == 0 || channels == 0 {
            return;
        }
        let mut clipped = 0u64;
        let mut p = self.0.pending.lock().unwrap();
        let s16 = |x: f32| (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        for i in 0..frames {
            for ch in 0..CHANNELS {
                let c = ch.min(channels - 1); // mono source: both ears
                let x = if planar { data[c * frames + i] } else { data[i * channels + c] };
                if !(-1.0..=1.0).contains(&x) {
                    clipped += 1;
                }
                p.push(s16(x));
            }
        }
        while p.len() >= WAVE_SAMPLES {
            let wave: Vec<i16> = p.drain(..WAVE_SAMPLES).collect();
            drop(p);
            self.wave(&wave);
            p = self.0.pending.lock().unwrap();
        }
        drop(p);
        // taken before the stats lock: wave() takes the encoder's lock first, then the stats'
        let codec = if self.0.opus.lock().unwrap().is_some() { "opus" } else { "pcm" };
        let mut st = self.0.stats.lock().unwrap();
        st.callbacks += 1;
        st.frames += frames as u64;
        st.frames_max = st.frames_max.max(frames);
        st.clipped += clipped;
        let since = *st.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= Duration::from_secs(10) {
            let ms_per_cb = frames as f64 * 1000.0 / RATE as f64;
            let kbps = st.bytes as f64 * 8.0 / since.elapsed().as_secs_f64() / 1000.0;
            info!(
                codec,
                kbps = format!("{kbps:.0}"),
                callbacks = st.callbacks,
                frames_per_cb_avg = st.frames / st.callbacks.max(1) as u64,
                frames_per_cb_max = st.frames_max,
                ms_last_cb = format!("{ms_per_cb:.1}"),
                waves_sent = st.waves,
                waves_silent = st.silent,
                clipped_samples = st.clipped,
                "RDPSND: 10 s"
            );
            *st = Stats { since: Some(Instant::now()), ..Default::default() };
        }
    }

    fn wave(&self, w: &[i16]) {
        if w.iter().all(|s| s.abs() <= SILENCE) {
            let run = self.0.silent_run.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            if run > HANGOVER {
                self.0.stats.lock().unwrap().silent += 1;
                return;
            }
        } else {
            self.0.silent_run.store(0, Ordering::Relaxed);
        }
        let data = match &mut *self.0.opus.lock().unwrap() {
            Some(enc) => {
                let mut out = vec![0u8; 4000];
                match enc.encode(w, &mut out) {
                    Ok(n) if n >= MIN_WAVE => {
                        out.truncate(n);
                        out
                    }
                    Ok(n) if n > 0 => {
                        // near-silence encodes to under MIN_WAVE bytes: pad it (Opus padding,
                        // decodes the same) rather than leave a hole
                        match opus2::packet::pad(&mut out[..MIN_WAVE], n) {
                            Ok(_) => {
                                out.truncate(MIN_WAVE);
                                out
                            }
                            Err(e) => {
                                warn!("Opus pad: {e}");
                                return;
                            }
                        }
                    }
                    Ok(_) => {
                        self.0.stats.lock().unwrap().silent += 1;
                        return;
                    }
                    Err(e) => {
                        warn!("Opus encode: {e}");
                        return;
                    }
                }
            }
            None => {
                let mut data = Vec::with_capacity(w.len() * 2);
                for s in w {
                    data.extend_from_slice(&s.to_le_bytes());
                }
                data
            }
        };
        {
            let mut st = self.0.stats.lock().unwrap();
            st.waves += 1;
            st.bytes += data.len() as u64;
        }
        let ts = self.0.t0.elapsed().as_millis() as u32;
        if let Some(tx) = &*self.0.sender.lock().unwrap() {
            let _ = tx.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, ts)));
        }
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0 .0.sender.lock().unwrap() = Some(sender);
    }
}

impl SoundServerFactory for Factory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        let mut formats = Vec::new();
        if opus_offered() {
            formats.push(format(WaveFormat::OPUS));
        }
        formats.push(format(WaveFormat::PCM));
        Box::new(Backend { sound: self.0.clone(), formats })
    }
}

struct Backend {
    sound: Sound,
    formats: Vec<AudioFormat>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").field("on", &self.sound.on()).finish()
    }
}

impl RdpsndServerHandler for Backend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(&mut self, common: &'a [NegotiatedFormat]) -> Option<&'a NegotiatedFormat> {
        common
            .iter()
            .find(|f| f.format().format == WaveFormat::OPUS)
            .or_else(|| common.iter().find(|f| f.format().format == WaveFormat::PCM && f.format().n_samples_per_sec == RATE))
            .or_else(|| common.first())
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let f = format.format();
        // an encoder that cannot be made leaves the format declined, the crate logs why
        let opus = if f.format == WaveFormat::OPUS {
            Some(opus_encoder().map_err(|e| Box::new(OpusError(e.to_string())) as Box<dyn RdpsndError>)?)
        } else {
            None
        };
        info!(rate = f.n_samples_per_sec, codec = if opus.is_some() { "opus" } else { "pcm" }, "RDPSND: the client takes sound");
        self.sound.0.pending.lock().unwrap().clear();
        *self.sound.0.opus.lock().unwrap() = opus;
        self.sound.0.on.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn stop(&mut self) {
        debug!("RDPSND: stopped");
        self.sound.0.on.store(false, Ordering::Relaxed);
        *self.sound.0.opus.lock().unwrap() = None;
    }
}

#[derive(Debug)]
struct OpusError(String);

impl std::fmt::Display for OpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Opus encoder: {}", self.0)
    }
}

impl std::error::Error for OpusError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 440 Hz tone, 20 ms waves through the encoder and kRDPclient's decoder settings back:
    /// the packets are small, the tone comes back.
    #[test]
    fn opus_round_trip() {
        let mut enc = opus_encoder().unwrap();
        let mut dec = opus2::Decoder::new(RATE, opus2::Channels::Stereo).unwrap();
        let mut bytes = 0;
        let mut energy = 0f64;
        for k in 0..50 {
            let wave: Vec<i16> = (0..WAVE_SAMPLES / 2)
                .flat_map(|i| {
                    let t = (k * WAVE_SAMPLES / 2 + i) as f64 / RATE as f64;
                    let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                    [s, s]
                })
                .collect();
            let mut out = vec![0u8; 4000];
            let n = enc.encode(&wave, &mut out).unwrap();
            assert!(n >= MIN_WAVE);
            bytes += n;
            let mut pcm = vec![0i16; WAVE_SAMPLES];
            let got = dec.decode(&out[..n], &mut pcm, false).unwrap();
            assert_eq!(got, WAVE_SAMPLES / 2);
            if k > 5 {
                energy += pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>();
            }
        }
        let kbps = bytes as f64 * 8.0 / 1.0 / 1000.0; // 50 waves = 1 s
        assert!(kbps < 200.0, "{kbps} kbit/s");
        let rms = (energy / (44 * WAVE_SAMPLES) as f64).sqrt();
        assert!(rms > 3000.0, "rms {rms}");
    }
}
