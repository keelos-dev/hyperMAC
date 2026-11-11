//! VideoToolbox H.264 for the keelOS::Video channel (docs/design.md "编码"): the M-series
//! media engine encodes the ScreenCaptureKit frames (their CVPixelBuffer, zero copy) in
//! real-time mode without frame reordering; the output is turned from AVCC (length-prefixed
//! NAL units) into Annex B (start codes) with SPS/PPS before every keyframe, which is what
//! codec_id 3 means on the wire (keelOS docs/71 §4.4) and what kRDPclient's Media Foundation
//! decoder takes.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Context, anyhow, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxFrameDelayCount, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};
use tracing::{debug, warn};

/// One encoded picture, Annex B, ready for a FRAME message.
pub struct Encoded {
    pub data: Vec<u8>,
    pub key: bool,
    /// the capture_us the caller passed to encode()
    pub pts_us: u64,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    /// the callback's refcon: a leaked Box<Sender>, freed in Drop after the session is gone
    refcon: *mut std::sync::mpsc::Sender<Encoded>,
    pub width: u32,
    pub height: u32,
}

// The session and the sender are used from the encoding thread only; VideoToolbox calls the
// output callback on its own thread, which touches only the Sender (Send + Sync).
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> anyhow::Result<(Encoder, std::sync::mpsc::Receiver<Encoded>)> {
        let (tx, rx) = std::sync::mpsc::channel::<Encoded>();
        let refcon = Box::into_raw(Box::new(tx));
        let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
        // Apple Silicon's low-latency rate-control encoder (macOS 11+): made for real-time
        // streaming — no frame reordering, minimal encoder delay. Typing felt slow with the
        // default encoder (user 2026-11-09); every ms in the loop counts.
        let spec = {
            let k: &CFType = unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl };
            CFDictionary::from_slices(&[k], &[kCFBooleanTrue()])
        };
        let spec: &CFDictionary = unsafe { std::mem::transmute::<&CFDictionary<CFType, CFType>, &CFDictionary>(&spec) };
        let st = unsafe {
            VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                Some(spec),
                None,
                None,
                Some(output),
                refcon as *mut c_void,
                NonNull::from(&mut raw),
            )
        };
        if st != 0 || raw.is_null() {
            unsafe { drop(Box::from_raw(refcon)) };
            bail!("VTCompressionSessionCreate: {st}");
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
        let e = Encoder { session, refcon, width, height };
        // Properties are tuning, not requirements: the hardware encoder refuses some of them
        // (MaxFrameDelayCount is -12900 kVTPropertyNotSupportedErr on the M4) and that must not
        // cost us the stream. Each failure is logged once, at session creation.
        e.set("RealTime", unsafe { kVTCompressionPropertyKey_RealTime }, kCFBooleanTrue());
        e.set("AllowFrameReordering", unsafe { kVTCompressionPropertyKey_AllowFrameReordering }, kCFBooleanFalse());
        e.set("ProfileLevel", unsafe { kVTCompressionPropertyKey_ProfileLevel }, unsafe {
            kVTProfileLevel_H264_Main_AutoLevel
        });
        // Keyframes are ours to decide (START, KEYFRAME_REQUEST); a long interval keeps VT from
        // inserting its own. 0 would mean "only the first".
        e.set_num("MaxKeyFrameInterval", unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval }, 3600.0);
        e.set_num("ExpectedFrameRate", unsafe { kVTCompressionPropertyKey_ExpectedFrameRate }, fps as f64);
        e.set_num("AverageBitRate", unsafe { kVTCompressionPropertyKey_AverageBitRate }, (bitrate_kbps as f64) * 1000.0);
        // AverageBitRate is a soft target: a busy picture made VT burst to 1.4x it, which over a
        // home uplink queued in TCP and showed as 200 ms latency spikes (user, from the office,
        // 2026-11-09). DataRateLimits is the hard cap: at most this many bytes per second
        // (average + 10 % headroom), so bursts are bounded by the window, not the content.
        {
            let bytes_per_s = CFNumber::new_cgfloat((bitrate_kbps as f64) * 1000.0 / 8.0 * 1.1);
            let one_second = CFNumber::new_cgfloat(1.0);
            let limits = objc2_core_foundation::CFArray::from_objects(&[&*bytes_per_s, &*one_second]);
            let limits: &CFType = &limits;
            e.set("DataRateLimits", unsafe { kVTCompressionPropertyKey_DataRateLimits }, limits);
        }
        // one frame in, one frame out: no pipelining latency (not every encoder has it)
        e.set_num("MaxFrameDelayCount", unsafe { kVTCompressionPropertyKey_MaxFrameDelayCount }, 0.0);
        // Profile (user 2026-11-09: "速度优先可以作为缺省的 profile"): MACRDP_VIDEO_PROFILE=
        // speed (default) — the ms saved per frame matter more than the bits on a desktop of
        // text; quality — let the encoder spend its time, for pictures/video over a fat link.
        let speed = std::env::var("MACRDP_VIDEO_PROFILE").map_or(true, |p| p != "quality");
        e.set(
            "PrioritizeEncodingSpeedOverQuality",
            unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
            if speed { kCFBooleanTrue() } else { kCFBooleanFalse() },
        );
        debug!(profile = if speed { "speed" } else { "quality" }, "VideoToolbox profile");
        let st = unsafe { e.session.prepare_to_encode_frames() };
        if st != 0 {
            warn!("VTCompressionSessionPrepareToEncodeFrames: {st} (going on)");
        }
        debug!(width, height, fps, bitrate_kbps, "VideoToolbox H.264 session");
        Ok((e, rx))
    }

    fn set(&self, name: &str, key: &CFString, value: &CFType) {
        let st = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if st != 0 {
            // -12900 kVTPropertyNotSupportedErr for MaxFrameDelayCount on the M4 is expected
            tracing::info!(name, status = st, "VideoToolbox: property not taken (going on without it)");
        }
    }

    fn set_num(&self, name: &str, key: &CFString, v: f64) {
        let n = CFNumber::new_cgfloat(v);
        self.set(name, key, &n)
    }

    /// Encode one picture. `pts_us` becomes the presentation time and comes back in `Encoded`.
    /// The output arrives on the receiver from VideoToolbox's thread, usually within a few ms.
    pub fn encode(&self, image: &CVImageBuffer, pts_us: u64, force_key: bool) -> anyhow::Result<()> {
        let pts = CMTime { value: pts_us as i64, timescale: 1_000_000, flags: CMTimeFlags::Valid, epoch: 0 };
        let dur = CMTime { value: 0, timescale: 1_000_000, flags: CMTimeFlags::empty(), epoch: 0 };
        let props = force_key.then(|| {
            // an untyped CFDictionary<CFType, CFType>, which is what encode_frame takes
            let k: &CFType = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            CFDictionary::from_slices(&[k], &[kCFBooleanTrue()])
        });
        let mut flags = VTEncodeInfoFlags::empty();
        // CFDictionary<K, V>'s parameters are PhantomData only (same layout); encode_frame
        // wants the untyped CFDictionary<Opaque, Opaque>
        let props: Option<&CFDictionary> =
            props.as_deref().map(|d| unsafe { std::mem::transmute::<&CFDictionary<CFType, CFType>, &CFDictionary>(d) });
        // the pts doubles as the per-frame refcon so the callback can report it
        let st = unsafe { self.session.encode_frame(image, pts, dur, props, pts_us as usize as *mut c_void, &mut flags) };
        if st != 0 {
            bail!("VTCompressionSessionEncodeFrame: {st}");
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            // flush and stop: no callback runs after Invalidate returns
            let _ = self.session.complete_frames(CMTime { value: 0, timescale: 1, flags: CMTimeFlags::empty(), epoch: 0 });
            self.session.invalidate();
            drop(Box::from_raw(self.refcon));
        }
    }
}

fn kCFBooleanTrue() -> &'static CFType {
    unsafe { objc2_core_foundation::kCFBooleanTrue }.expect("kCFBooleanTrue")
}

fn kCFBooleanFalse() -> &'static CFType {
    unsafe { objc2_core_foundation::kCFBooleanFalse }.expect("kCFBooleanFalse")
}

/// Frames the encoder took and gave nothing for (all sessions; vms.rs's timing line says it).
pub static DROPPED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// VideoToolbox's output callback: one compressed frame -> Annex B -> the channel.
unsafe extern "C-unwind" fn output(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let tx = unsafe { &*(refcon as *const std::sync::mpsc::Sender<Encoded>) };
    if status != 0 {
        warn!(status, "VideoToolbox: frame failed");
        return;
    }
    let Some(sb) = (unsafe { sample.as_ref() }) else {
        // the rate control left this frame out
        DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    match unsafe { annex_b(sb) } {
        Ok((data, key)) => {
            let _ = tx.send(Encoded { data, key, pts_us: frame_refcon as usize as u64 });
        }
        Err(e) => warn!("VideoToolbox: {e:#}"),
    }
}

/// The sample's NAL units with start codes; SPS and PPS first when it is a keyframe.
unsafe fn annex_b(sb: &CMSampleBuffer) -> anyhow::Result<(Vec<u8>, bool)> {
    // keyframe unless the first sample's attachments say NotSync
    let key = unsafe { sb.sample_attachments_array(false) }
        .and_then(|arr| {
            if unsafe { arr.count() } == 0 {
                return None;
            }
            let d = unsafe { arr.value_at_index(0) } as *const CFDictionary;
            let d = unsafe { d.as_ref() }?;
            let v = unsafe { d.value(kCMSampleAttachmentKey_NotSync as *const CFString as *const c_void) };
            if v.is_null() {
                return Some(true);
            }
            let b = unsafe { &*(v as *const CFBoolean) };
            Some(!b.value())
        })
        .unwrap_or(true);

    let mut out = Vec::with_capacity(64 * 1024);
    let mut nal_len_size = 4usize;
    if key {
        let fmt = unsafe { sb.format_description() }.context("no format description")?;
        let mut count = 0usize;
        let mut hdr = 0i32;
        let st = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &fmt,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
                &mut hdr,
            )
        };
        if st != 0 {
            bail!("H264ParameterSetAtIndex(count): {st}");
        }
        nal_len_size = hdr.max(1) as usize;
        for i in 0..count {
            let (mut p, mut n) = (std::ptr::null::<u8>(), 0usize);
            let st = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&fmt, i, &mut p, &mut n, std::ptr::null_mut(), std::ptr::null_mut())
            };
            if st != 0 || p.is_null() {
                bail!("H264ParameterSetAtIndex({i}): {st}");
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(p, n) });
        }
    }

    let bb: CFRetained<CMBlockBuffer> = unsafe { sb.data_buffer() }.context("no data buffer")?;
    let len = unsafe { bb.data_length() };
    let mut avcc = vec![0u8; len];
    let st = unsafe { bb.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr() as *mut c_void).unwrap()) };
    if st != 0 {
        bail!("CMBlockBufferCopyDataBytes: {st}");
    }
    // length-prefixed NAL units -> start codes
    let mut i = 0usize;
    while i + nal_len_size <= avcc.len() {
        let mut n = 0usize;
        for b in &avcc[i..i + nal_len_size] {
            n = (n << 8) | *b as usize;
        }
        i += nal_len_size;
        let end = (i + n).min(avcc.len());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..end]);
        i = end;
    }
    if out.is_empty() {
        return Err(anyhow!("empty access unit"));
    }
    Ok((out, key))
}
