//! The Mac's system sound through a CoreAudio process tap (macOS 14.2+, design.md "声音"),
//! muted on the Mac while it is read (`CATapMutedWhenTapped`): the sound plays on the client
//! only, as with mstsc's "play on this computer" (user 2026-11-09: "两边都会有，物理机器 vs
//! kRDPclient"). ScreenCaptureKit's audio (capture.rs) only copies the mix, the speakers keep
//! playing; it stays as the fallback when the tap cannot be had (older macOS, the "System
//! Audio Recording" permission refused).
//!
//! Shape (Apple's "Capturing system audio with Core Audio taps", AudioCap): a global stereo
//! tap → a private aggregate device whose clock is the default output device and whose input
//! is the tap → an IOProc block on our own queue → sound.rs. The mute lasts exactly as long
//! as the IOProc runs: Drop (session ends, the client stops sound) or a crash (the HAL drops
//! a dead process's taps) gives the speakers back.
//!
//! Two independent switches (user 2026-11-09: the Mac is at home, sound played remotely should
//! not come out of it): the app's "mute the Mac while remote" (settings.rs "mute_mac", default
//! on, MACRDP_MUTE_MAC=0) silences the Mac for as long as a session is connected, read live
//! (MuteSwitch); whether the client hears it is the client's own sound switch (kRDPclient) —
//! the tap runs either way, its samples go nowhere while RDPSND is off.
//!
//! NOTE 2026-11-09: written on the Windows machine and only `cargo check`ed for
//! aarch64-apple-darwin in a scratch crate; first real run on the Mac mini.

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;

use anyhow::{Context, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_audio::{
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice, AudioObjectGetPropertyData,
    AudioObjectID, AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey,
    kAudioAggregateDeviceIsStackedKey, kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID, kAudioHardwarePropertyDefaultSystemOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsFloat,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};
use tracing::info;

use crate::sound::{RATE, Sound};

/// The app's "mute the Mac while remote" switch, read again while a session runs so flipping
/// it acts at once.
#[derive(Clone)]
pub struct MuteSwitch(pub std::path::PathBuf);

impl MuteSwitch {
    pub fn on(&self) -> bool {
        crate::settings::on(&self.0, "mute_mac", "MACRDP_MUTE_MAC")
    }
}

type CreateTap = unsafe extern "C-unwind" fn(*const CATapDescription, *mut AudioObjectID) -> i32;
type DestroyTap = unsafe extern "C-unwind" fn(AudioObjectID) -> i32;

/// The tap functions looked up at run time: linked directly, the binary would not even load
/// on macOS 13 (LSMinimumSystemVersion), where they do not exist.
fn tap_fns() -> Option<(CreateTap, DestroyTap)> {
    unsafe {
        let c = libc::dlsym(libc::RTLD_DEFAULT, c"AudioHardwareCreateProcessTap".as_ptr());
        let d = libc::dlsym(libc::RTLD_DEFAULT, c"AudioHardwareDestroyProcessTap".as_ptr());
        if c.is_null() || d.is_null() || AnyClass::get(c"CATapDescription").is_none() {
            return None;
        }
        Some((std::mem::transmute::<*mut c_void, CreateTap>(c), std::mem::transmute::<*mut c_void, DestroyTap>(d)))
    }
}

fn check(st: i32, what: &str) -> anyhow::Result<()> {
    if st != 0 {
        let b = st.to_be_bytes();
        let fourcc = if b.iter().all(|c| c.is_ascii_graphic()) { format!(" '{}'", String::from_utf8_lossy(&b)) } else { String::new() };
        bail!("{what}: OSStatus {st}{fourcc}");
    }
    Ok(())
}

fn addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain }
}

/// A fixed-size property of an audio object.
unsafe fn get<T: Copy>(obj: AudioObjectID, selector: u32, what: &str) -> anyhow::Result<T> {
    let a = addr(selector);
    let mut v = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(v.as_mut_ptr().cast()),
        )
    };
    check(st, what)?;
    Ok(unsafe { v.assume_init() })
}

/// A CFString property (returned +1: owned by the Retained).
unsafe fn get_string(obj: AudioObjectID, selector: u32, what: &str) -> anyhow::Result<Retained<NSString>> {
    let p: *mut c_void = unsafe { get(obj, selector, what)? };
    unsafe { Retained::from_raw(p.cast::<NSString>()) }.with_context(|| format!("{what}: null"))
}

fn ns(k: &CStr) -> Retained<NSString> {
    NSString::from_str(k.to_str().unwrap_or_default())
}

fn dict(pairs: &[(&CStr, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| ns(k)).collect();
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let vals: Vec<&AnyObject> = pairs.iter().map(|(_, v)| *v).collect();
    NSDictionary::from_slices(&keys, &vals)
}

/// One running tap. Dropping it stops the IOProc and destroys the aggregate device and the
/// tap, which unmutes the Mac.
pub struct Tap {
    tap: AudioObjectID,
    agg: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    destroy_tap: DestroyTap,
    _queue: DispatchRetained<DispatchQueue>,
    _block: RcBlock<dyn Fn(NonNull<AudioTimeStamp>, NonNull<AudioBufferList>, NonNull<AudioTimeStamp>, NonNull<AudioBufferList>, NonNull<AudioTimeStamp>)>,
}

// The IDs are plain integers; the block and the queue are only kept alive here, never called.
unsafe impl Send for Tap {}

impl Tap {
    /// `sound`: where the samples go (None: the sound switch is off, the tap only mutes).
    pub fn start(sound: Option<Sound>) -> anyhow::Result<Tap> {
        let (create_tap, destroy_tap) = tap_fns().context("no CoreAudio process taps (macOS 14.2+)")?;

        // 1. the tap: every process, stereo, silent on the Mac while we read it
        let desc = unsafe { CATapDescription::initStereoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &NSArray::new()) };
        unsafe {
            desc.setName(&NSString::from_str("hyperMAC"));
            desc.setPrivate(true);
            desc.setMuteBehavior(CATapMuteBehavior::MutedWhenTapped);
        }
        let mut tap: AudioObjectID = 0;
        check(unsafe { create_tap(&*desc, &mut tap) }, "AudioHardwareCreateProcessTap")?;
        let mut me = Tap { tap, agg: 0, proc_id: None, destroy_tap, _queue: DispatchQueue::new("hypermac.tap", None), _block: RcBlock::new(|_, _, _, _, _| {}) };
        // from here on, an error drops `me`, which undoes what was made so far

        let fmt: AudioStreamBasicDescription = unsafe { get(tap, kAudioTapPropertyFormat, "tap format")? };
        if fmt.mFormatFlags & kAudioFormatFlagIsFloat == 0 || fmt.mBitsPerChannel != 32 || fmt.mChannelsPerFrame == 0 {
            bail!("tap format not Float32: {fmt:?}");
        }
        let tap_uid = unsafe { desc.UUID().UUIDString() };

        // 2. the aggregate device: clocked by the default output device, the tap as its input
        let out_dev: AudioObjectID =
            unsafe { get(kAudioObjectSystemObject as AudioObjectID, kAudioHardwarePropertyDefaultSystemOutputDevice, "default output device")? };
        let out_uid = unsafe { get_string(out_dev, kAudioDevicePropertyDeviceUID, "output device UID")? };
        let agg_uid = NSUUID::new().UUIDString();
        let name = NSString::from_str("hyperMAC tap");
        let one = NSNumber::new_i32(1);
        let zero = NSNumber::new_i32(0);
        let sub = dict(&[(kAudioSubDeviceUIDKey, &out_uid)]);
        let subs = NSArray::from_retained_slice(&[sub]);
        let tap_entry = dict(&[(kAudioSubTapDriftCompensationKey, &one), (kAudioSubTapUIDKey, &tap_uid)]);
        let taps = NSArray::from_retained_slice(&[tap_entry]);
        let agg_desc = dict(&[
            (kAudioAggregateDeviceNameKey, &name),
            (kAudioAggregateDeviceUIDKey, &agg_uid),
            (kAudioAggregateDeviceMainSubDeviceKey, &out_uid),
            (kAudioAggregateDeviceIsPrivateKey, &one),
            (kAudioAggregateDeviceIsStackedKey, &zero),
            (kAudioAggregateDeviceTapAutoStartKey, &one),
            (kAudioAggregateDeviceSubDeviceListKey, &subs),
            (kAudioAggregateDeviceTapListKey, &taps),
        ]);
        // NSDictionary and CFDictionary are toll-free bridged
        let cf = unsafe { &*(Retained::as_ptr(&agg_desc).cast::<CFDictionary>()) };
        check(unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut me.agg)) }, "AudioHardwareCreateAggregateDevice")?;

        // 3. the IOProc: input buffers → interleaved stereo at 48 kHz → sound.rs
        let channels = fmt.mChannelsPerFrame as usize;
        // the block is Fn; the queue is serial, so the lock is never contended
        let rs = std::sync::Mutex::new(Resampler::new(fmt.mSampleRate));
        me._block = RcBlock::new(move |_now: NonNull<AudioTimeStamp>, input: NonNull<AudioBufferList>, _t: NonNull<AudioTimeStamp>, _out: NonNull<AudioBufferList>, _ot: NonNull<AudioTimeStamp>| {
            let st = unsafe { stereo(input.as_ref()) };
            let Some(sound) = &sound else { return };
            if st.is_empty() || !sound.on() {
                return;
            }
            let st = rs.lock().unwrap().run(&st);
            sound.push_tap(&st, st.len() / 2);
        });
        unsafe {
            check(
                AudioDeviceCreateIOProcIDWithBlock(NonNull::from(&mut me.proc_id), me.agg, Some(&me._queue), RcBlock::as_ptr(&me._block)),
                "AudioDeviceCreateIOProcIDWithBlock",
            )?;
            check(AudioDeviceStart(me.agg, me.proc_id), "AudioDeviceStart")?;
        }
        info!(rate = fmt.mSampleRate, channels, output = %out_uid, "sound: CoreAudio tap, the Mac is muted for this session");
        Ok(me)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.agg, self.proc_id);
                AudioDeviceDestroyIOProcID(self.agg, self.proc_id);
            }
            if self.agg != 0 {
                AudioHardwareDestroyAggregateDevice(self.agg);
            }
            (self.destroy_tap)(self.tap);
        }
        info!("sound: tap closed, the Mac plays again");
    }
}

/// The IOProc's input as interleaved stereo Float32: one interleaved buffer of `channels`, or
/// one buffer per channel (non-interleaved). Mono goes to both ears.
unsafe fn stereo(list: &AudioBufferList) -> Vec<f32> {
    let n = list.mNumberBuffers as usize;
    let bufs = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), n) };
    let floats = |b: &objc2_core_audio_types::AudioBuffer| -> &[f32] {
        if b.mData.is_null() {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4) }
    };
    match bufs {
        [] => Vec::new(),
        [b] => {
            let ch = (b.mNumberChannels as usize).max(1);
            let d = floats(b);
            d.chunks_exact(ch).flat_map(|f| [f[0], f[1.min(ch - 1)]]).collect()
        }
        [l, r, ..] => {
            let (l, r) = (floats(l), floats(r));
            l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect()
        }
    }
}

/// Linear resampling of interleaved stereo to 48 kHz, for an output device that runs at
/// another rate (44.1 kHz). Not audiophile, but no worse than what the Mac's own mixer does to
/// a 44.1 kHz file; a 48 kHz device (the usual case) skips it.
struct Resampler {
    /// input frames per output frame
    step: f64,
    /// position of the next output frame, in input frames counted from `prev`
    pos: f64,
    /// the last input frame of the previous call
    prev: [f32; 2],
}

impl Resampler {
    fn new(rate: f64) -> Self {
        Resampler { step: if rate > 0.0 { rate / RATE as f64 } else { 1.0 }, pos: 0.0, prev: [0.0; 2] }
    }

    fn run(&mut self, input: &[f32]) -> Vec<f32> {
        if (self.step - 1.0).abs() < 1e-9 {
            return input.to_vec();
        }
        let frames = input.len() / 2;
        if frames == 0 {
            return Vec::new();
        }
        // frame i of [prev, input...]
        let at = |i: usize| -> [f32; 2] { if i == 0 { self.prev } else { [input[(i - 1) * 2], input[(i - 1) * 2 + 1]] } };
        let mut out = Vec::with_capacity((frames as f64 / self.step) as usize * 2 + 4);
        while self.pos + 1.0 <= frames as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push(a[0] + (b[0] - a[0]) * f);
            out.push(a[1] + (b[1] - a[1]) * f);
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        self.prev = at(frames);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_44k1_to_48k() {
        let mut r = Resampler::new(44_100.0);
        let mut n = 0;
        for _ in 0..100 {
            n += r.run(&vec![0.5f32; 441 * 2]).len() / 2; // 10 ms each
        }
        assert!((n as i64 - 48_000).abs() <= 2, "{n}");
        let mut same = Resampler::new(48_000.0);
        assert_eq!(same.run(&[0.1, 0.2, 0.3, 0.4]), vec![0.1, 0.2, 0.3, 0.4]);
    }
}
