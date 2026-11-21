//! The `keelOS::Video` dynamic channel (keelOS docs/71 §4): kRDPclient gets the picture as
//! H.264 instead of RemoteFX. mstsc refuses the channel and stays on EGFX. Protocol copied from
//! keel-rdp (keelOS/rdp/src/video.rs); the encoder is VideoToolbox H.264 (encoder.rs), not libvpx.
//!
//! Protocol v1: every message = u16 type, u16 flags (0), u32 body length, body; little-endian.
//! Client → server: CAPS, FRAME_ACK, KEYFRAME_REQUEST. Server → client: START, FRAME, STOP.
//!
//! Flow (§4.5): only the newest picture is ever encoded, nothing is queued. While the frames in
//! flight (sent, not acknowledged) reach the client's max_in_flight, nothing is encoded; the
//! next captured frame replaces the held one. Frames are dropped before encoding, never after.
//!
//! Stage 1 (2026-11-09): channel + CAPS handshake only; feed_frame logs. Stage 2 adds the
//! VideoToolbox encoder, stage 3 wires capture frames in. docs/design.md "编码".

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ironrdp_core::{Encode, EncodeResult, WriteCursor, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor, encode_dvc_messages};
use ironrdp_pdu::PduResult;
use ironrdp_server::{DvcServerFactory, ServerEvent, ServerEventSender};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

pub const CHANNEL_NAME: &str = "keelOS::Video";

pub const CAPS: u16 = 0x0001;
pub const FRAME_ACK: u16 = 0x0002;
pub const KEYFRAME_REQUEST: u16 = 0x0003;
/// v2 (docs/71 §4.8.1): the client has the second connection up; u32 session. The draft said
/// 0x0004, which SOUND took meanwhile — 0x0005 here, kRDPclient must match (TODO-hypermac.md).
pub const TRANSPORT_READY: u16 = 0x0005;
/// The client's sound switch (keelOS docs/73 §5, kRDPclient proto.rs): u8 on + 3 reserved.
/// Sound itself stays on RDPSND; this only says whether to send it (sound.rs set_client).
pub const SOUND: u16 = 0x0004;
/// v2, client → server (user 2026-11-09: "让 kRDPclient 来请求第二条通道，有请求再发"): the
/// client asks for a second connection; only then does the server send TRANSPORT_OFFER. CAPS
/// with version 2 alone offers nothing — the client decides (e.g. only when UDP did not come
/// up). Empty body.
pub const TRANSPORT_REQUEST: u16 = 0x0006;
/// LIMIT (kRDPclient 2026-11-20, a phone on mobile data): u16 `max_fps`, u16 reserved, u32
/// `max_kbps`; 0 leaves that one to the server. May come at any time after CAPS; the running
/// stream is ended and the next frame starts one within the limit.
pub const LIMIT: u16 = 0x0007;
/// SIZE (kRDPclient 2026-11-20): u16 `max_width`, u16 `max_height`: how large the client
/// shows the whole desktop, in pixels (a phone: 1206 wide for 1920×1080); 0 0 = as it is.
/// An H.264 stream is then encoded at about that size, in the desktop's shape (never
/// larger than the desktop), and its START has the desktop's own size after `fps_hint`
/// (u16, u16) so the client keeps the pointer in the desktop's pixels. May come at any
/// time; the next frame starts a stream of the new size. Lossless streams stay as they are.
pub const SIZE: u16 = 0x0008;
pub const START: u16 = 0x0101;
pub const FRAME: u16 = 0x0102;
pub const STOP: u16 = 0x0103;
/// v2: u32 session, 16-byte one-time token (10 s), u16 port (0 = this connection's port).
pub const TRANSPORT_OFFER: u16 = 0x0104;

/// The second connection announces itself with these 8 bytes after TLS (then u32 session,
/// 16-byte token); the server answers u32 0 (OK) or closes.
pub const VIDEO_MAGIC: &[u8; 8] = b"KEELVID2";
/// A TRANSPORT_OFFER's token is good this long.
pub const OFFER_TTL: std::time::Duration = std::time::Duration::from_secs(10);
/// The window full and no FRAME_ACK for this long: the ACKs are lost, not late (begin()).
const STALL: std::time::Duration = std::time::Duration::from_secs(2);

pub const VERSION: u32 = 1;
pub const CODEC_VP9: u8 = 1;
pub const CODEC_H264: u8 = 3;
/// The lossless picture of a screen of few colours (indexed.rs; keelOS docs/71 §4.4).
pub const CODEC_INDEXED: u8 = 4;

// --- messages (little-endian; copied from keel-rdp video.rs) ---------------------------------

pub fn message(ty: u16, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + body.len());
    m.extend_from_slice(&ty.to_le_bytes());
    m.extend_from_slice(&0u16.to_le_bytes());
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(body);
    m
}

/// Splits a channel payload into (type, body) messages (the DVC layer already reassembled).
pub fn messages(mut p: &[u8]) -> Vec<(u16, &[u8])> {
    let mut v = Vec::new();
    while p.len() >= 8 {
        let ty = u16::from_le_bytes([p[0], p[1]]);
        let len = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as usize;
        if p.len() - 8 < len {
            warn!(ty, len, have = p.len() - 8, "keelOS::Video: truncated message");
            break;
        }
        v.push((ty, &p[8..8 + len]));
        p = &p[8 + len..];
    }
    v
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().unwrap()))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Caps {
    pub version: u32,
    pub max_width: u16,
    pub max_height: u16,
    pub max_in_flight: u8,
    /// (codec_id, codec_flags), the client's preference first
    pub codecs: Vec<(u8, u8)>,
}

impl Caps {
    pub fn decode(b: &[u8]) -> Option<Caps> {
        if b.len() < 10 {
            return None;
        }
        let n = b[9] as usize;
        let list = b.get(10..10 + n * 2)?;
        Some(Caps {
            version: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            max_width: u16::from_le_bytes([b[4], b[5]]),
            max_height: u16::from_le_bytes([b[6], b[7]]),
            max_in_flight: b[8],
            codecs: list.chunks(2).map(|c| (c[0], c[1])).collect(),
        })
    }

    /// Does the client offer H.264 (we only encode that, stage 2)? Returns its codec_flags.
    pub fn h264(&self) -> Option<u8> {
        self.codecs.iter().find(|(id, _)| *id == CODEC_H264).map(|&(_, f)| f)
    }

    /// Does the client take the lossless picture (indexed.rs)?
    pub fn indexed(&self) -> bool {
        self.codecs.iter().any(|(id, _)| *id == CODEC_INDEXED)
    }
}

pub fn start_body(stream: u32, codec: u8, w: u16, h: u16, fps: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&VERSION.to_le_bytes());
    b.extend_from_slice(&stream.to_le_bytes());
    b.extend_from_slice(&[codec, 0]);
    b.extend_from_slice(&w.to_le_bytes());
    b.extend_from_slice(&h.to_le_bytes());
    b.extend_from_slice(&fps.to_le_bytes());
    b
}

/// FRAME: the 20-byte header before the bitstream.
pub fn frame_head(stream: u32, frame: u32, capture_us: u64, key: bool) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..4].copy_from_slice(&stream.to_le_bytes());
    b[4..8].copy_from_slice(&frame.to_le_bytes());
    b[8..16].copy_from_slice(&capture_us.to_le_bytes());
    b[16] = key as u8;
    b
}

/// Monotonic microseconds since the process started (FRAME's capture_us).
pub fn now_us() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// A message as the DVC layer wants it.
struct Msg(Vec<u8>);

impl Encode for Msg {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.0.len());
        dst.write_slice(&self.0);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "keelOS::Video"
    }
    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for Msg {}

// --- channel state ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    channel: Option<u32>,
    caps: Option<Caps>,
    /// The stream id in progress, if any, and its size.
    stream: Option<u32>,
    size: (u32, u32),
    /// The size the stream is encoded at: `size`, or smaller after the client's SIZE.
    out: (u32, u32),
    /// The current stream's codec_id.
    codec: u8,
    /// Frames of the current stream sent (or being encoded) and not yet acknowledged.
    in_flight: u32,
    /// Since when begin() has answered Busy with no FRAME_ACK in between (the stall watchdog).
    busy_since: Option<Instant>,
    /// The next frame id to send on the current stream (the encoder keeps order).
    next_frame: u32,
    /// The next encode must be a keyframe (START, or KEYFRAME_REQUEST).
    keyframe: bool,
    /// When begin() last let a frame through (the client's LIMIT on frames a second: the
    /// capture runs at its own rate).
    let_through: Option<Instant>,
    /// v2: the second (video-only) TCP connection, once the client said TRANSPORT_READY:
    /// FRAMEs go there instead of the DVC. None: DVC (v1, or v2 before/after the transport).
    transport: Option<UnboundedSender<Vec<u8>>>,
    /// v2: the session id offered to this client (TRANSPORT_OFFER), until READY.
    session: Option<u32>,
}

/// The TRANSPORT_OFFERs outstanding on this server (all connections): session -> (token,
/// the connection's Video, when it expires). The second connection's handshake looks here.
pub type Offers = Arc<Mutex<HashMap<u32, (Vec<u8>, Video, Instant)>>>;

/// What the capture should do with a frame (begin()).
pub enum Go {
    /// Encode it for this stream, as a keyframe if asked: H.264, or `indexed` (indexed.rs).
    Encode { stream: u32, key: bool, indexed: bool },
    /// A stream runs but the client has enough in flight: drop this frame, send no bitmap.
    Busy,
    /// No video for this connection: the bitmap path.
    NoVideo,
}

/// One connection's video channel. Cloned between the DVC side and the frame feeder.
#[derive(Clone, Default)]
pub struct Video {
    state: Arc<Mutex<State>>,
    sender: Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>,
    /// true once the client's CAPS offers H.264: the display may switch to video.
    ready: Arc<AtomicBool>,
    /// next stream id
    next_stream: Arc<AtomicU32>,
    /// the server-wide offer table (v2); None: v2 not offered
    offers: Option<Offers>,
    /// this connection's RDPSND (sound.rs), for the client's SOUND switch; None: sound is off
    sound: Option<crate::sound::Sound>,
    /// the client is on the local network (set_lan): the high frame rate
    lan: Arc<AtomicBool>,
    /// the client's LIMIT: frames a second (the upper half) and kilobits a second, 0 = none
    limit: Arc<std::sync::atomic::AtomicU64>,
    /// the client's SIZE: the width in the upper half, 0 = as it is
    shown: Arc<AtomicU32>,
    /// how many of the desktop's pixels the running stream has, in thousandths (begin())
    share: Arc<AtomicU32>,
}

/// An address of the local network: private, link-local or this Mac itself. (A VPN that hands
/// out private addresses counts too; one on 100.64/10, as Tailscale's, does not.)
pub fn is_lan(ip: std::net::IpAddr) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(a) => a.is_private() || a.is_loopback() || a.is_link_local(),
        std::net::IpAddr::V6(a) => a.is_loopback() || a.segments()[0] & 0xfe00 == 0xfc00 || a.segments()[0] & 0xffc0 == 0xfe80,
    }
}

pub struct Factory(Video);

impl Video {
    pub fn new(offers: Option<Offers>, sound: Option<crate::sound::Sound>) -> (Self, Factory) {
        let v = Video { offers, sound, ..Default::default() };
        v.next_stream.store(1, Ordering::Relaxed);
        (v.clone(), Factory(v))
    }

    /// v2: the second connection is up and verified (main.rs): FRAMEs go through `tx` from
    /// the client's TRANSPORT_READY on; the next frame is a keyframe (§4.8.1: a change of line
    /// always starts with one).
    /// The client is on the local network (user 2026-11-11: "本地LAN的话，就可以上高刷").
    pub fn set_lan(&self, lan: bool) {
        self.lan.store(lan, Ordering::Relaxed);
    }

    /// Frames a second for this connection: 60 on the local network, else capture::fps()'s 30
    /// (a WAN's jitter is as long as a 60 fps frame). MACRDP_FPS, when set, is it for both.
    pub fn fps(&self) -> i32 {
        let own = if std::env::var_os("MACRDP_FPS").is_none() && self.lan.load(Ordering::Relaxed) { 60 } else { crate::capture::fps() };
        match self.limit().0 {
            0 => own,
            most => own.min(most as i32),
        }
    }

    /// The size an H.264 stream of a `w`×`h` desktop is encoded at: the desktop's, or
    /// smaller when the client shows it smaller (SIZE). Even numbers; a size within a
    /// twentieth of the desktop's is not worth another picture.
    fn out_size(&self, w: u32, h: u32) -> (u32, u32) {
        let shown = self.shown.load(Ordering::Relaxed);
        let (max_w, max_h) = (shown >> 16, shown & 0xffff);
        if max_w == 0 || max_h == 0 || w == 0 || h == 0 {
            return (w, h);
        }
        let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
        if scale >= 0.95 {
            return (w, h);
        }
        let even = |n: f64| (((n / 2.0).round() as u32) * 2).max(64);
        (even(w as f64 * scale).min(w), even(h as f64 * scale).min(h))
    }

    /// The running stream's encoded size (begin() made it): what the encoder is made for.
    pub fn stream_size(&self) -> (u32, u32) {
        self.state.lock().unwrap().out
    }

    /// The client's LIMIT: frames and kilobits a second, 0 = none.
    fn limit(&self) -> (u32, u32) {
        let limit = self.limit.load(Ordering::Relaxed);
        ((limit >> 32) as u32, limit as u32)
    }

    /// The H.264 bit rate: 6 Mbps is 1080p30 over a home uplink; twice the frames on the local
    /// network get twice the bits. MACRDP_VIDEO_KBPS, when set, is it for both.
    pub fn kbps(&self) -> u32 {
        let own: u32 = std::env::var("MACRDP_VIDEO_KBPS").ok().and_then(|s| s.parse().ok()).unwrap_or(if self.fps() > 30 { 12_000 } else { 6_000 });
        // a stream made smaller for the client (SIZE) gets its share of the bits, not all
        // of them: fewer pixels are the point
        let own = match self.share.load(Ordering::Relaxed) {
            0 | 1000.. => own,
            share => (own * share / 1000).max(500),
        };
        match self.limit().1 {
            0 => own,
            // (below this VideoToolbox's picture is not one to read a desktop by)
            most => own.min(most.max(200)),
        }
    }

    pub fn attach_transport(&self, session: u32, tx: UnboundedSender<Vec<u8>>) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.session != Some(session) {
            return false;
        }
        s.transport = Some(tx);
        s.keyframe = true;
        // a change of line starts clean: what was unacknowledged on the old one is not waited for
        s.in_flight = 0;
        s.busy_since = None;
        info!(session, "keelOS::Video: video transport attached (frames leave the RDP connection)");
        true
    }

    /// v2: the second connection ended (the client closed it = released it, or it died): back
    /// to the DVC with a keyframe, and the session is forgotten so a later TRANSPORT_REQUEST
    /// gets a fresh offer (the client asks and releases on demand, kRDPclient 2026-11-09).
    pub fn detach_transport(&self) {
        let mut s = self.state.lock().unwrap();
        let had = s.transport.take().is_some();
        s.session = None;
        if had {
            s.keyframe = true;
            // The frames still on the closed connection, and the ACKs the client had queued
            // there, are lost: their slots would stay taken for ever, begin() would answer
            // Busy on every frame and the picture froze (2026-11-09, release then re-request).
            // The next frame is a keyframe anyway, so nothing depends on the lost ones.
            s.in_flight = 0;
            s.busy_since = None;
            info!("keelOS::Video: video transport gone, frames back on the RDP connection");
        }
    }

    /// Something from the second connection (FRAME_ACK, KEYFRAME_REQUEST): same handling.
    pub fn from_transport(&self, payload: &[u8]) {
        self.process(payload);
    }

    fn max_in_flight(s: &State) -> u32 {
        s.caps.as_ref().map_or(2, |c| u32::from(c.max_in_flight.max(1)))
    }

    /// Hand the channel's messages to the connection (DVC-wrapped through the server event sink).
    fn send(&self, msgs: Vec<Vec<u8>>) {
        let (sink, channel) = {
            let s = self.state.lock().unwrap();
            (self.sender.lock().unwrap().clone(), s.channel)
        };
        let (Some(tx), Some(channel)) = (sink, channel) else { return };
        let msgs: Vec<DvcMessage> = msgs.into_iter().map(|m| Box::new(Msg(m)) as DvcMessage).collect();
        match encode_dvc_messages(channel, msgs, ChannelFlags::SHOW_PROTOCOL) {
            Ok(m) => {
                let _ = tx.send(ServerEvent::Dvc(m));
            }
            Err(e) => warn!("keelOS::Video messages: {e}"),
        }
    }

    /// A captured frame of this size is here: encode it, drop it, or take the bitmap path.
    /// Starts a stream (START) when the client can take video and none runs at this size and
    /// in this codec. `indexed`: the caller has this picture as a lossless one (takes_indexed(),
    /// indexed.rs) — a client that does not take those gets H.264 as before.
    pub fn begin(&self, w: u32, h: u32, indexed: bool) -> Go {
        let mut s = self.state.lock().unwrap();
        let codec = {
            let ready = self.ready.load(Ordering::Relaxed);
            s.caps.as_ref().and_then(|c| {
                let fits = w <= c.max_width as u32 && h <= c.max_height as u32;
                if indexed && c.indexed() && fits {
                    Some(CODEC_INDEXED)
                } else if ready && c.h264().is_some() && fits && w % 2 == 0 && h % 2 == 0 {
                    Some(CODEC_H264)
                } else {
                    None
                }
            })
        };
        let Some(codec) = codec else {
            if let Some(id) = s.stream.take() {
                drop(s);
                self.stop(id);
            }
            return Go::NoVideo;
        };
        let out = if codec == CODEC_H264 { self.out_size(w, h) } else { (w, h) };
        if s.stream.is_none() || s.size != (w, h) || s.codec != codec || s.out != out {
            let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
            info!(stream = id, w, h, codec = if codec == CODEC_INDEXED { "lossless" } else { "H.264" }, "keelOS::Video: START");
            s.stream = Some(id);
            s.size = (w, h);
            s.out = out;
            self.share.store((u64::from(out.0) * u64::from(out.1) * 1000 / (u64::from(w) * u64::from(h)).max(1)) as u32, Ordering::Relaxed);
            s.codec = codec;
            s.in_flight = 0;
            s.next_frame = 0;
            s.keyframe = true;
            drop(s);
            // fps_hint = the capture ceiling actually in force (MACRDP_FPS), not a constant
            let hint = self.fps() as u16;
            let mut start = start_body(id, codec, out.0 as u16, out.1 as u16, hint);
            if out != (w, h) {
                info!(stream = id, w = out.0, h = out.1, "keelOS::Video: the stream is smaller than the desktop (the client's SIZE)");
                start.extend_from_slice(&(w as u16).to_le_bytes());
                start.extend_from_slice(&(h as u16).to_le_bytes());
            }
            self.send(vec![message(START, &start)]);
            s = self.state.lock().unwrap();
        }
        // The client's LIMIT on frames a second: the frames between are not encoded.
        if let most @ 1.. = self.limit().0 {
            let apart = std::time::Duration::from_micros(900_000 / u64::from(most));
            if s.let_through.is_some_and(|at| at.elapsed() < apart) {
                return Go::Busy;
            }
            s.let_through = Some(Instant::now());
        }
        if s.in_flight >= Self::max_in_flight(&s) {
            // Watchdog: full and not one ACK for STALL — the ACKs are lost (a line that went
            // away, a client bug), not late. Start over with a keyframe instead of freezing.
            let since = *s.busy_since.get_or_insert_with(Instant::now);
            if since.elapsed() < STALL {
                return Go::Busy;
            }
            warn!(in_flight = s.in_flight, "keelOS::Video: no FRAME_ACK for {STALL:?} with the window full; starting over with a keyframe");
            s.in_flight = 0;
            s.keyframe = true;
        }
        s.busy_since = None;
        s.in_flight += 1;
        let key = std::mem::take(&mut s.keyframe);
        Go::Encode { stream: s.stream.unwrap(), key, indexed: s.codec == CODEC_INDEXED }
    }

    /// The client takes the lossless picture (indexed.rs): worth looking whether the screen is one.
    pub fn takes_indexed(&self) -> bool {
        self.state.lock().unwrap().caps.as_ref().is_some_and(Caps::indexed)
    }

    /// No encoder for this connection (VideoToolbox refused): bitmaps from now on. Ends the
    /// stream with STOP so the client leaves video mode and shows EGFX again (§4.6) —
    /// without it the picture froze on a client waiting for frames that never came.
    pub fn disable(&self) {
        self.ready.store(false, Ordering::Relaxed);
        let ended = self.state.lock().unwrap().stream.take();
        if let Some(id) = ended {
            self.stop(id);
        }
    }

    /// STOP for a stream that ended: the client goes back to EGFX; the bitmap path then
    /// repaints (every bitmap update is a full frame, the encoder diffs it).
    fn stop(&self, stream: u32) {
        info!(stream, "keelOS::Video: STOP, back to bitmaps");
        self.send(vec![message(STOP, &stream.to_le_bytes())]);
    }

    /// The encoder failed this frame: give its in-flight slot back.
    pub fn abort(&self, stream: u32) {
        let mut s = self.state.lock().unwrap();
        if s.stream == Some(stream) {
            s.in_flight = s.in_flight.saturating_sub(1);
        }
    }

    /// An encoded access unit for `stream`: send it as FRAME (frame ids in encode order).
    pub fn frame_out(&self, stream: u32, key: bool, data: &[u8], capture_us: u64) {
        let (id, transport) = {
            let mut s = self.state.lock().unwrap();
            if s.stream != Some(stream) {
                // a frame of a stream that ended meanwhile
                return;
            }
            let id = s.next_frame;
            s.next_frame += 1;
            (id, s.transport.clone())
        };
        let mut m = Vec::with_capacity(20 + data.len());
        m.extend_from_slice(&frame_head(stream, id, capture_us, key));
        m.extend_from_slice(data);
        debug!(stream, frame = id, key, bytes = data.len(), "keelOS::Video: FRAME");
        match transport {
            // v2: the video-only connection; if it just died, this frame is lost and the
            // detach (main.rs) asks for a keyframe on the DVC
            Some(tx) => {
                if tx.send(message(FRAME, &m)).is_err() {
                    self.detach_transport();
                }
            }
            None => self.send(vec![message(FRAME, &m)]),
        }
    }

    /// v2: the TRANSPORT_OFFER message for this client (after its CAPS says v2); the caller
    /// sends it once it has let go of the state lock. Registers the session in the offer table.
    fn offer_transport(&self, s: &mut State) -> Option<Vec<u8>> {
        let offers = self.offers.as_ref()?;
        let mut token = [0u8; 16];
        std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut token)).ok()?;
        let session = self.next_stream.fetch_add(1, Ordering::Relaxed) | 0x8000_0000;
        s.session = Some(session);
        let mut o = offers.lock().unwrap();
        o.retain(|_, (_, _, until)| *until > Instant::now());
        o.insert(session, (token.to_vec(), self.clone(), Instant::now() + OFFER_TTL));
        drop(o);
        let mut b = Vec::with_capacity(22);
        b.extend_from_slice(&session.to_le_bytes());
        b.extend_from_slice(&token);
        b.extend_from_slice(&0u16.to_le_bytes()); // same port as this connection
        info!(session, "keelOS::Video: TRANSPORT_OFFER (v2, a second TCP connection for the frames)");
        Some(message(TRANSPORT_OFFER, &b))
    }

    fn process(&self, payload: &[u8]) {
        // messages to send once the state lock is released (send() takes it too)
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut s = self.state.lock().unwrap();
        for (ty, b) in messages(payload) {
            match ty {
                CAPS => match Caps::decode(b) {
                    Some(c) => {
                        info!(?c, "keelOS::Video: client capabilities");
                        self.ready.store(c.h264().is_some(), Ordering::Relaxed);
                        if c.h264().is_none() {
                            info!(lossless = c.indexed(), "keelOS::Video: client offers no H.264 — staying on EGFX");
                        }
                        s.caps = Some(c);
                    }
                    None => warn!(len = b.len(), "keelOS::Video: bad CAPS"),
                },
                TRANSPORT_REQUEST => {
                    if s.caps.as_ref().is_some_and(|c| c.version >= 2) {
                        if s.transport.is_some() {
                            debug!("keelOS::Video: TRANSPORT_REQUEST while the transport is up; ignored");
                        } else {
                            // an earlier offer the client never used (or that expired) is dropped
                            if let (Some(old), Some(offers)) = (s.session.take(), &self.offers) {
                                offers.lock().unwrap().remove(&old);
                            }
                            out.extend(self.offer_transport(&mut s));
                        }
                    } else {
                        warn!("keelOS::Video: TRANSPORT_REQUEST from a v1 client; ignored");
                    }
                }
                TRANSPORT_READY => match u32_at(b, 0) {
                    Some(session) if s.session == Some(session) => {
                        // the attach itself happened when the second connection verified its
                        // token (attach_transport); READY only says the client is listening there
                        if s.transport.is_some() {
                            info!(session, "keelOS::Video: TRANSPORT_READY, frames move to the video connection");
                            s.keyframe = true;
                        } else {
                            warn!(session, "keelOS::Video: TRANSPORT_READY before the video connection; ignored");
                        }
                    }
                    other => warn!(?other, "keelOS::Video: TRANSPORT_READY for an unknown session"),
                },
                FRAME_ACK => {
                    if let (Some(stream), Some(_frame), Some(_decode)) =
                        (u32_at(b, 0), u32_at(b, 4), u32_at(b, 8))
                    {
                        if s.stream == Some(stream) {
                            s.in_flight = s.in_flight.saturating_sub(1);
                            s.busy_since = None;
                        }
                    }
                }
                KEYFRAME_REQUEST => {
                    if u32_at(b, 0).is_some_and(|id| s.stream == Some(id)) {
                        info!("keelOS::Video: the client asked for a keyframe");
                        s.keyframe = true;
                    }
                }
                SOUND => match b.first() {
                    Some(&on) => match &self.sound {
                        Some(sound) => sound.set_client(on != 0),
                        None => info!(on, "keelOS::Video: SOUND, but sound is off on this Mac (hyperMAC's switch)"),
                    },
                    None => warn!("keelOS::Video: bad SOUND"),
                },
                LIMIT => match (b.get(0..2), u32_at(b, 4)) {
                    (Some(fps), Some(kbps)) => {
                        let fps = u16::from_le_bytes([fps[0], fps[1]]);
                        let limit = (u64::from(fps) << 32) | u64::from(kbps);
                        if self.limit.swap(limit, Ordering::Relaxed) != limit {
                            info!(max_fps = fps, max_kbps = kbps, "keelOS::Video: LIMIT (0 = this server's own)");
                            // The encoder is made for a stream: the running one ends here,
                            // and the next frame starts one at the new rates (begin()).
                            s.stream = None;
                            s.let_through = None;
                        }
                    }
                    _ => warn!(len = b.len(), "keelOS::Video: bad LIMIT"),
                },
                SIZE => match b.get(0..4) {
                    Some(b) => {
                        let (w, h) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
                        let shown = (u32::from(w) << 16) | u32::from(h);
                        if self.shown.swap(shown, Ordering::Relaxed) != shown {
                            // (begin() sees the other size at the next frame and starts a stream)
                            info!(max_width = w, max_height = h, "keelOS::Video: SIZE (0 = the desktop's own)");
                        }
                    }
                    None => warn!(len = b.len(), "keelOS::Video: bad SIZE"),
                },
                _ => debug!(ty, len = b.len(), "keelOS::Video: unknown message skipped"),
            }
        }
        drop(s);
        if !out.is_empty() {
            self.send(out);
        }
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0.sender.lock().unwrap() = Some(sender);
    }
}

impl DvcServerFactory for Factory {
    fn build(&self) -> Box<dyn DvcServerProcessor> {
        Box::new(Channel(self.0.clone()))
    }
}

struct Channel(Video);

impl_as_any!(Channel);

impl DvcProcessor for Channel {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        info!(channel_id, "keelOS::Video: the client opened the channel");
        self.0.state.lock().unwrap().channel = Some(channel_id);
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        self.0.process(payload);
        Ok(Vec::new())
    }

    fn close(&mut self, _channel_id: u32) {
        info!("keelOS::Video: channel closed");
        let mut s = self.0.state.lock().unwrap();
        s.channel = None;
        s.caps = None;
        s.transport = None; // the writer task sees the closed channel and ends the connection
        if let (Some(session), Some(offers)) = (s.session.take(), &self.0.offers) {
            offers.lock().unwrap().remove(&session);
        }
        self.0.ready.store(false, Ordering::Relaxed);
    }
}

impl DvcServerProcessor for Channel {}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_h264() -> Vec<u8> {
        // version 2, 3840x2160, 3 in flight, one codec: H.264 in hardware
        let mut b = Vec::new();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&3840u16.to_le_bytes());
        b.extend_from_slice(&2160u16.to_le_bytes());
        b.extend_from_slice(&[3, 1, CODEC_H264, 1]);
        message(CAPS, &b)
    }

    fn limit(fps: u16, kbps: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&fps.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&kbps.to_le_bytes());
        message(LIMIT, &b)
    }

    /// kRDPclient's LIMIT (a phone on mobile data): the rates go down to it, the running
    /// stream ends for a new one, frames closer than the limit are not encoded; 0 0 takes
    /// it away again.
    #[test]
    fn limit_lowers_the_rates_and_starts_a_new_stream() {
        let (v, _factory) = Video::new(None, None);
        v.process(&caps_h264());
        let (own_fps, own_kbps) = (v.fps(), v.kbps());
        assert!(own_fps >= 30 && own_kbps >= 6000);
        let first = match v.begin(1920, 1080, false) {
            Go::Encode { stream, key, .. } => {
                assert!(key);
                stream
            }
            _ => panic!("no stream"),
        };
        v.process(&limit(10, 1000));
        assert_eq!((v.fps(), v.kbps()), (10, 1000));
        let second = match v.begin(1920, 1080, false) {
            Go::Encode { stream, key, .. } => {
                assert!(key, "the new stream starts with a keyframe");
                stream
            }
            _ => panic!("no stream after LIMIT"),
        };
        assert_ne!(first, second);
        // the capture's next frame, a moment later: too soon for 10 a second
        assert!(matches!(v.begin(1920, 1080, false), Go::Busy));
        // the same limit again changes nothing
        v.process(&limit(10, 1000));
        assert!(matches!(v.begin(1920, 1080, false), Go::Busy));
        // a limit below what a picture needs is raised to the least
        v.process(&limit(0, 50));
        assert_eq!((v.fps(), v.kbps()), (own_fps, 200));
        v.process(&limit(0, 0));
        assert_eq!((v.fps(), v.kbps()), (own_fps, own_kbps));
        // SIZE: a phone shows the desktop 1206 wide
        let mut size = Vec::new();
        size.extend_from_slice(&1206u16.to_le_bytes());
        size.extend_from_slice(&2622u16.to_le_bytes());
        v.process(&message(SIZE, &size));
        assert_eq!(v.out_size(1920, 1080), (1206, 678));
        let third = match v.begin(1920, 1080, false) {
            Go::Encode { stream, key, .. } => {
                assert!(key);
                stream
            }
            _ => panic!("no stream after SIZE"),
        };
        assert_ne!(second, third);
        assert_eq!(v.stream_size(), (1206, 678));
        // its share of the bits: 1206*678 / 1920*1080 = 394 thousandths
        assert_eq!(v.kbps(), own_kbps * 394 / 1000);
        // lossless streams and sizes near the desktop's stay whole; 0 0 takes it away
        assert_eq!(v.out_size(1280, 678), (1206, 638));
        assert_eq!(v.out_size(1206, 600), (1206, 600));
        v.process(&message(SIZE, &[0, 0, 0, 0]));
        assert_eq!(v.out_size(1920, 1080), (1920, 1080));
        assert!(matches!(v.begin(1920, 1080, false), Go::Encode { key: true, .. }));
        assert_eq!((v.stream_size(), v.kbps()), ((1920, 1080), own_kbps));
        // a short LIMIT is skipped
        v.process(&message(LIMIT, &[10, 0]));
        assert_eq!((v.fps(), v.kbps()), (own_fps, own_kbps));
    }
}
