//! The Mac's screen through ScreenCaptureKit: full BGRA frames, capped at 30 fps for M0,
//! handed to the connection as DisplayUpdate::Bitmap (IronRDP's encoder does the diffing).
//!
//! M0 captures at the display's point size (the "looks like" resolution), not the Retina
//! pixel size: half the bandwidth, and 1:1 with what the client window shows. Physical
//! pixels are a later item (docs/design.md "画面").
//!
//! NOTE 2026-11-09: written on the Windows machine against the objc2 0.6 / frameworks 0.3
//! API from documentation; it has NOT been compiled yet. The first build on the Mac will
//! likely need mechanical fixes (method vs C-function spellings in objc2-core-media /
//! objc2-core-video, block signatures). The structure is the deliverable.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use block2::RcBlock;
use bytes::BytesMut;
use dispatch2::DispatchQueue;
use ironrdp_server::{BitmapUpdate, DisplayUpdate, PixelFormat};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};
use tracing::{debug, info, warn};

/// Frame rate ceiling asked of ScreenCaptureKit (it only delivers frames that changed, so a
/// still screen is far below this). Default 30: over a WAN the jitter (tens of ms) is as
/// long as a 60 fps frame interval and shows as judder, and the bandwidth halves (user
/// 2026-11-09, from the office: "+20ms, 估计跑不了60"). MACRDP_FPS=60 on a LAN — the
/// media engine keeps up either way. Making this adapt to the ACK round trip is a TODO.
pub fn fps() -> i32 {
    std::env::var("MACRDP_FPS").ok().and_then(|s| s.parse().ok()).filter(|f| (1..=120).contains(f)).unwrap_or(30)
}
/// Frames in flight to a connection: a slow one drops frames instead of queueing them.
const DEPTH: usize = 2;

/// The primary display as plain data (usable across threads; the SCDisplay object itself
/// is fetched again by the capture thread).
#[derive(Clone, Copy, Debug)]
pub struct Screen {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

/// Retained ObjC objects over a thread boundary. ScreenCaptureKit's objects are
/// internally thread safe; objc2 just has no way to know that, so Retained<SCDisplay>
/// is not Send on its own.
struct SendCell<T>(T);
unsafe impl<T> Send for SendCell<T> {}

/// Fetch the shareable content and reduce it inside the completion handler (which runs
/// on a ScreenCaptureKit queue); only the reduced value crosses back.
fn with_content<T: Send + 'static>(
    f: impl Fn(&SCShareableContent) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<T>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let r = if content.is_null() {
            let why = unsafe { error.as_ref() }
                .map(|e| unsafe { e.localizedDescription() }.to_string())
                .unwrap_or_else(|| "no shareable content".into());
            Err(anyhow!(why))
        } else {
            f(unsafe { &*content })
        };
        let _ = tx.send(r);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv_timeout(Duration::from_secs(10))
        .context("ScreenCaptureKit did not answer (Screen Recording permission dialog pending?)")?
}

/// The first (primary) display.
pub fn primary() -> anyhow::Result<Screen> {
    with_content(|c| {
        let displays = unsafe { c.displays() };
        let d = displays.iter().next().context("no display")?;
        Ok(Screen {
            id: unsafe { d.displayID() },
            width: unsafe { d.width() } as u32,
            height: unsafe { d.height() } as u32,
        })
    })
}

/// Where a capture's thread is while it calls into the system, for the log: on 2026-11-11 the
/// Mac's desktop stayed without a picture after a restart of the service (three logins, no
/// "capturing", no error, no timeout) and nothing said which call never came back. Each call
/// that leaves this process is a step; a watchdog thread says so when one lasts, and once has
/// the system's `sample` write this process's stacks to <state>/logs.
#[derive(Clone)]
struct Steps(Arc<Mutex<StepState>>);

struct StepState {
    /// the call in progress and since when; None: not in one
    now: Option<(&'static str, Instant)>,
    /// the capture's thread has ended
    over: bool,
    /// the steps done and what each took, ms (said when one was slow)
    took: Vec<(&'static str, u128)>,
}

/// A step lasting this long is said in the log (and again at 3x, 9x, ...).
const STEP_SLOW: Duration = Duration::from_secs(5);

impl Steps {
    fn new() -> Self {
        let s = Steps(Arc::new(Mutex::new(StepState { now: None, over: false, took: Vec::new() })));
        let w = s.clone();
        let _ = std::thread::Builder::new().name("capture-watch".into()).spawn(move || {
            let mut said = (None::<&'static str>, STEP_SLOW, false);
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let st = w.0.lock().unwrap();
                if st.over {
                    return;
                }
                let Some((name, since)) = st.now else {
                    said = (None, STEP_SLOW, said.2);
                    continue;
                };
                drop(st);
                if said.0 != Some(name) {
                    said = (Some(name), STEP_SLOW, said.2);
                }
                if since.elapsed() < said.1 {
                    continue;
                }
                said.1 *= 3;
                warn!(step = name, secs = since.elapsed().as_secs(), "capture: this call has not come back");
                if !said.2 {
                    said.2 = true;
                    sample_self(name);
                }
            }
        });
        s
    }

    /// Run `f` as the step `name`.
    fn step<T>(&self, name: &'static str, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        self.0.lock().unwrap().now = Some((name, t));
        let r = f();
        let mut st = self.0.lock().unwrap();
        st.now = None;
        st.took.push((name, t.elapsed().as_millis()));
        r
    }

    /// The steps so far, if any was slow: one line.
    fn report(&self) {
        let mut st = self.0.lock().unwrap();
        if st.took.iter().any(|(_, ms)| *ms >= 1000) {
            let all: Vec<String> = st.took.iter().map(|(n, ms)| format!("{n} {ms} ms")).collect();
            warn!(steps = all.join(", "), "capture: slow to start");
        }
        st.took.clear();
    }
}

impl Drop for StepsEnd {
    fn drop(&mut self) {
        self.0 .0.lock().unwrap().over = true;
    }
}

/// Ends the watchdog when the capture's thread ends, however it does.
struct StepsEnd(Steps);

/// This process's stacks, by the system's `sample`, into <state>/logs/capture-stall-<time>.txt:
/// what a person would run by hand, done while the call is still stuck.
fn sample_self(step: &str) {
    let dir = crate::vms::root().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let file = dir.join(format!("capture-stall-{secs}.txt"));
    match std::process::Command::new("/usr/bin/sample")
        .arg(std::process::id().to_string())
        .args(["2", "-mayDie", "-file"])
        .arg(&file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => warn!(step, file = %file.display(), "capture: stacks of this process are being written"),
        Err(e) => warn!(step, "capture: could not run sample: {e}"),
    }
}

/// Start capturing `screen`. Frames arrive on the returned channel from ScreenCaptureKit's
/// own queue; dropping the receiver (and with it `Stop`, held by the same owner) ends the
/// capture. The capture runs on its own thread so no ObjC object crosses into the
/// connection's runtime.
pub fn start(
    screen: Screen,
    video: Option<crate::video::Video>,
    sound: Option<crate::sound::Sound>,
    mute: crate::tap::MuteSwitch,
) -> anyhow::Result<tokio::sync::mpsc::Receiver<DisplayUpdate>> {
    let (tx, rx) = tokio::sync::mpsc::channel(DEPTH);
    std::thread::Builder::new().name("capture".into()).spawn(move || {
        if let Err(e) = run(screen, tx, video, sound, mute) {
            warn!("capture: {e:#}");
        }
    })?;
    Ok(rx)
}

fn run(
    screen: Screen,
    tx: tokio::sync::mpsc::Sender<DisplayUpdate>,
    video: Option<crate::video::Video>,
    sound: Option<crate::sound::Sound>,
    mute: crate::tap::MuteSwitch,
) -> anyhow::Result<()> {
    let steps = Steps::new();
    let _end = StepsEnd(steps.clone());
    // The SCDisplay for this capture, fetched here so the Retained object can be moved
    // to this thread in one piece.
    let display: Retained<SCDisplay> = steps.step("shareable content", || with_content(move |c| {
        let displays = unsafe { c.displays() };
        let d = displays
            .iter()
            .find(|d| unsafe { d.displayID() } == screen.id)
            .context("display gone")?;
        Ok(SendCell(d))
    }))
    .map(|c| c.0)?;

    let filter = steps.step("content filter", || unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new())
    });
    let config = unsafe { SCStreamConfiguration::new() };
    steps.step("stream configuration", || unsafe {
        config.setWidth(screen.width as usize);
        config.setHeight(screen.height as usize);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        // (60 for a client on the local network, video.rs fps(); bitmaps stay at fps(), frame())
        let rate = video.as_ref().map_or(fps(), |v| v.fps());
        config.setMinimumFrameInterval(CMTime { value: 1, timescale: rate, flags: CMTimeFlags::Valid, epoch: 0 });
        config.setQueueDepth(3);
        // the client draws the pointer itself (cursor.rs)
        config.setShowsCursor(false);
        if sound.is_some() {
            // the system mix as the second output of the same stream (sound.rs); the Screen
            // Recording grant covers "screen and system audio"
            config.setCapturesAudio(true);
            config.setSampleRate(crate::sound::RATE as isize);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
        }
    });
    let with_sound = sound.is_some();
    let output = Output::new(tx.clone(), video, sound);
    let queue = DispatchQueue::new("hypermac.capture", None);
    let stream = steps.step("stream init", || unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    });
    steps.step("add screen output", || unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|e| anyhow!("addStreamOutput: {}", e.localizedDescription()))
    })?;
    if with_sound {
        steps.step("add audio output", || unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("addStreamOutput(audio): {}", e.localizedDescription()))
        })?;
    }
    let (start_tx, start_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let block = RcBlock::new(move |error: *mut NSError| {
        let _ = start_tx.send(match unsafe { error.as_ref() } {
            Some(e) => Err(unsafe { e.localizedDescription() }.to_string()),
            None => Ok(()),
        });
    });
    steps.step("startCapture (the call)", || unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) });
    let started = steps.step("startCapture (its answer)", || start_rx.recv_timeout(Duration::from_secs(10)));
    steps.report();
    started.context("startCapture did not answer")?.map_err(|e| anyhow!("startCapture: {e}"))?;
    info!(id = screen.id, w = screen.width, h = screen.height, "capturing");

    // Hold the stream until the connection is done with it: the frame callback notices
    // the receiver is gone (send fails) and flips `dead`.
    // Meanwhile the CoreAudio tap (tap.rs) mutes the Mac for as long as the session lasts
    // and the app's switch says so (read again every second, so flipping it acts at once),
    // whether or not the client takes sound; ScreenCaptureKit's audio above is the fallback.
    let sound = output.ivars().sound.clone();
    let mut tap: Option<crate::tap::Tap> = None;
    let mut tap_ok = true;
    let mut mute_on = mute.on();
    let mut tick = 0u32;
    while !output.ivars().dead.load(std::sync::atomic::Ordering::Relaxed) {
        if tx.is_closed() {
            break;
        }
        tick = tick.wrapping_add(1);
        if tick % 5 == 0 {
            mute_on = mute.on();
        }
        let want = tap_ok && mute_on;
        if want && tap.is_none() {
            match steps.step("CoreAudio tap", || crate::tap::Tap::start(sound.clone())) {
                Ok(t) => {
                    if let Some(s) = &sound {
                        s.set_tapped(true);
                    }
                    tap = Some(t);
                }
                Err(e) => {
                    warn!("sound: no CoreAudio tap ({e:#}); the Mac keeps playing (ScreenCaptureKit's copy to the client)");
                    tap_ok = false;
                }
            }
        } else if !want && tap.is_some() {
            if let Some(s) = &sound {
                s.set_tapped(false);
            }
            tap = None;
        } else if tap.is_some() && sound.as_ref().is_some_and(|s| s.tap_silent()) {
            warn!(
                "sound: the tap stays silent while the Mac plays: \"System Audio Recording\" not allowed for hypermac? \
                 (System Settings → Privacy & Security → Screen & System Audio Recording). ScreenCaptureKit's copy instead"
            );
            if let Some(s) = &sound {
                s.set_tapped(false);
            }
            tap = None;
            tap_ok = false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(s) = &sound {
        s.set_tapped(false);
    }
    steps.step("CoreAudio tap (closing)", || drop(tap));
    let block = RcBlock::new(move |error: *mut NSError| {
        if let Some(e) = unsafe { error.as_ref() } {
            debug!("stopCapture: {}", unsafe { e.localizedDescription() });
        }
    });
    steps.step("stopCapture (the call)", || unsafe { stream.stopCaptureWithCompletionHandler(Some(&block)) });
    steps.report();
    info!(id = screen.id, "capture stopped");
    Ok(())
}

struct Ivars {
    tx: tokio::sync::mpsc::Sender<DisplayUpdate>,
    dead: std::sync::atomic::AtomicBool,
    /// keelOS::Video of this connection: frames go to VideoToolbox instead of as bitmaps
    /// while the client runs a stream (video.rs). None: bitmaps only (mstsc).
    video: Option<crate::video::Video>,
    /// The H.264 encoder of the current stream, (encoder, stream id); made on the first
    /// frame of a stream, dropped when the stream changes or ends.
    encoder: Mutex<Option<(crate::encoder::Encoder, u32)>>,
    /// RDPSND of this connection: the stream's audio samples go here (sound.rs).
    sound: Option<crate::sound::Sound>,
    /// when the last bitmap went out (frame(): their rate is fps() whatever the capture's is)
    bitmap_at: Mutex<Option<std::time::Instant>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "HyperMacStreamOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    // macOS ends the stream itself when the user picks "Stop Sharing" in its purple screen
    // recording indicator (user 2026-11-10: it doubles as Disconnect while the menu bar icon
    // is hidden, menubar.rs). The picture would freeze with the connection still up: end
    // every connection instead (the indicator stops all of hyperMAC's streams anyway).
    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            warn!(error = %error.localizedDescription(), "the system stopped the capture (Stop Sharing?): disconnecting");
            self.ivars().dead.store(true, std::sync::atomic::Ordering::Relaxed);
            crate::shutdown::disconnect_all();
        }
    }

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(&self, _stream: &SCStream, sample_buffer: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                if let Err(e) = self.frame(sample_buffer) {
                    debug!("frame: {e:#}");
                }
            } else if kind == SCStreamOutputType::Audio {
                if let Err(e) = self.audio(sample_buffer) {
                    debug!("audio: {e:#}");
                }
            }
        }
    }
);

impl Output {
    fn new(
        tx: tokio::sync::mpsc::Sender<DisplayUpdate>,
        video: Option<crate::video::Video>,
        sound: Option<crate::sound::Sound>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            tx,
            dead: std::sync::atomic::AtomicBool::new(false),
            video,
            encoder: Mutex::new(None),
            sound,
            bitmap_at: Mutex::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// One audio sample buffer (ScreenCaptureKit: Float32, 48 kHz, 2 channels, planar by
    /// default): straight to sound.rs, which cuts it into RDPSND waves.
    fn audio(&self, sb: &CMSampleBuffer) -> anyhow::Result<()> {
        let Some(sound) = &self.ivars().sound else { return Ok(()) };
        if !sound.on() {
            return Ok(()); // the client took no sound (yet): do not even copy
        }
        let fmt = unsafe { sb.format_description() }.context("audio: no format description")?;
        // CMAudioFormatDescription is CMFormatDescription (CF type with an audio media type)
        let asbd = unsafe {
            objc2_core_media::CMAudioFormatDescriptionGetStreamBasicDescription(
                &*(std::ptr::from_ref(&*fmt) as *const objc2_core_media::CMAudioFormatDescription),
            )
        };
        let asbd = unsafe { asbd.as_ref() }.context("audio: no stream description")?;
        let flags = asbd.mFormatFlags;
        if flags & objc2_core_audio_types::kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 {
            bail!("audio: not Float32 (flags {flags:#x}, {} bits)", asbd.mBitsPerChannel);
        }
        let planar = flags & objc2_core_audio_types::kAudioFormatFlagIsNonInterleaved != 0;
        let channels = asbd.mChannelsPerFrame as usize;
        let frames = unsafe { sb.num_samples() } as usize;
        let bb = unsafe { sb.data_buffer() }.context("audio: no data buffer")?;
        let len = unsafe { bb.data_length() };
        if len < frames * channels * 4 {
            bail!("audio: {len} bytes for {frames}x{channels} floats");
        }
        let mut raw = vec![0u8; len];
        let st = unsafe { bb.copy_data_bytes(0, len, std::ptr::NonNull::new(raw.as_mut_ptr() as *mut _).unwrap()) };
        if st != 0 {
            bail!("audio: CMBlockBufferCopyDataBytes {st}");
        }
        let floats: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        sound.push_capture(&floats, frames, channels, planar);
        Ok(())
    }

    /// One frame: to VideoToolbox when the client runs a keelOS::Video stream (the
    /// CVPixelBuffer as it is, zero copy), else a full-screen BitmapUpdate, tightly
    /// packed. Frames without an image buffer (status-only) are skipped; a full channel
    /// drops the frame (the client is behind, a newer one follows).
    fn frame(&self, sb: &CMSampleBuffer) -> anyhow::Result<()> {
        let Some(pb) = (unsafe { sb.image_buffer() }) else {
            return Ok(()); // status-only sample (started/stopped/idle)
        };
        if let Some(v) = &self.ivars().video {
            let (w, h) = unsafe { (CVPixelBufferGetWidth(&pb) as u32, CVPixelBufferGetHeight(&pb) as u32) };
            match v.begin(w, h) {
                crate::video::Go::Encode { stream, key } => {
                    let mut enc = self.ivars().encoder.lock().unwrap();
                    if enc.as_ref().map(|(_, s)| *s) != Some(stream) {
                        *enc = None; // the old stream's encoder, and its drain thread, go
                        // 6 Mbps: 1080p30 over a home uplink (10 burst to 14 and queued, 200 ms
                        // spikes); 12 at 60 a second on the local network (video.rs kbps())
                        match crate::encoder::Encoder::new(w, h, v.fps() as u32, v.kbps()) {
                            Ok((e, rx)) => {
                                let v = v.clone();
                                // encoded frames come out on VideoToolbox's thread into rx;
                                // this thread turns them into FRAME messages, in order
                                std::thread::Builder::new().name("h264-out".into()).spawn(move || {
                                    while let Ok(f) = rx.recv() {
                                        v.frame_out(stream, f.key, &f.data, f.pts_us);
                                    }
                                })?;
                                *enc = Some((e, stream));
                            }
                            Err(e) => {
                                warn!("H.264 encoder: {e:#} — this connection stays on bitmaps");
                                v.abort(stream);
                                v.disable();
                                return Ok(());
                            }
                        }
                    }
                    if let Some((e, _)) = enc.as_ref() {
                        if let Err(err) = e.encode(&pb, crate::video::now_us(), key) {
                            warn!("H.264 encode: {err:#}");
                            v.abort(stream);
                        }
                    }
                    return Ok(());
                }
                crate::video::Go::Busy => return Ok(()), // the client has enough in flight
                crate::video::Go::NoVideo => {
                    // bitmaps: drop a leftover encoder from a stream that ended
                    if let Ok(mut enc) = self.ivars().encoder.try_lock() {
                        *enc = None;
                    }
                    // the capture may run at the local network's 60: bitmaps stay at fps()
                    let mut last = self.ivars().bitmap_at.lock().unwrap();
                    if last.is_some_and(|t| t.elapsed() < Duration::from_micros(900_000 / fps() as u64)) {
                        return Ok(());
                    }
                    *last = Some(std::time::Instant::now());
                }
            }
        }
        unsafe {
            if CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
                bail!("CVPixelBufferLockBaseAddress");
            }
        }
        let r = (|| {
            let (w, h) = unsafe { (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb)) };
            let stride = unsafe { CVPixelBufferGetBytesPerRow(&pb) };
            let base = unsafe { CVPixelBufferGetBaseAddress(&pb) } as *const u8;
            if base.is_null() || w == 0 || h == 0 {
                bail!("empty pixel buffer");
            }
            let (w16, h16) = (
                NonZeroU16::new(u16::try_from(w)?).context("width")?,
                NonZeroU16::new(u16::try_from(h)?).context("height")?,
            );
            let mut data = BytesMut::with_capacity(w * h * 4);
            for y in 0..h {
                let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
                data.extend_from_slice(row);
            }
            Ok(DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0,
                y: 0,
                width: w16,
                height: h16,
                // X, not A: what the compositor leaves in the alpha byte is nobody's
                // business (keel-rdp sends BgrX32 too)
                format: PixelFormat::BgrX32,
                data: data.freeze(),
                stride: NonZeroUsize::new(w * 4).context("stride")?,
            }))
        })();
        unsafe {
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly);
        }
        match self.ivars().tx.try_send(r?) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.ivars().dead.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod step_tests {
    use super::*;

    /// A step gives back what its call gave and is no longer "in progress" afterwards; the
    /// watchdog's thread ends with the capture's.
    #[test]
    fn steps_are_recorded() {
        let steps = Steps::new();
        let end = StepsEnd(steps.clone());
        assert_eq!(steps.step("a", || 7), 7);
        steps.step("b", || std::thread::sleep(Duration::from_millis(20)));
        {
            let st = steps.0.lock().unwrap();
            assert!(st.now.is_none());
            assert_eq!(st.took.iter().map(|(n, _)| *n).collect::<Vec<_>>(), ["a", "b"]);
            assert!(st.took[1].1 >= 20);
        }
        drop(end);
        assert!(steps.0.lock().unwrap().over);
    }

    /// The real thing, by hand (takes 12 s, writes a file into the state folder's logs):
    /// `cargo test --release -- --ignored stalled_step_is_said_and_sampled --nocapture`.
    /// A step that does not come back is said after STEP_SLOW and this process's stacks are
    /// written, with this test's own frame in them.
    #[test]
    #[ignore]
    fn stalled_step_is_said_and_sampled() {
        let dir = crate::vms::root().join("logs");
        let before: std::collections::HashSet<_> =
            std::fs::read_dir(&dir).map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
        let steps = Steps::new();
        let _end = StepsEnd(steps.clone());
        steps.step("a call that hangs (test)", || std::thread::sleep(STEP_SLOW + Duration::from_secs(7)));
        let new: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| !before.contains(p) && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("capture-stall-")))
            .collect();
        assert_eq!(new.len(), 1, "{new:?}");
        let text = std::fs::read_to_string(&new[0]).unwrap();
        assert!(text.contains("stalled_step_is_said_and_sampled"), "the sample does not show this test's thread");
        let _ = std::fs::remove_file(&new[0]);
    }
}
