//! A Windows guest's sound on this Mac's speakers, while the VM's window is open.
//!
//! bhyve's HD Audio card (bhyve.rs: `-s N,hda,play=keel:<VM dir>/sound.ring`) writes what the
//! guest plays into a ring in that file (keelOS's, vm/bhyve/keel/audio.h): a 64-byte header,
//! then 256 KiB of 16-bit little-endian stereo at the guest's rate (44.1 or 48 kHz).
//!   0  u32 magic "KSN1"      4  u32 version     8  u32 size (of the data)
//!   12 u32 rate              16 u32 epoch (a new stream)          20 u32 pid
//!   24 u64 wpos: bytes written in all; byte n is at n % size
//! Here an AudioQueue (AudioToolbox) plays it on the default output device: its callback takes
//! the next 10 ms from the ring. The queue runs only while the guest plays and the window is
//! open; a VM without a window is silent, like a machine without a screen and speakers.
//! The service's RDP server reads the same ring for a remote client (src/vmsound.rs at the root).

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x314e_534b;
const HEADER: usize = 64;
/// One queue buffer.
const BUF_MS: u32 = 10;
/// How far behind the guest the player starts, and falls back to after a gap: room for the
/// jitter of both sides (the card writes every 2 ms, the queue asks every 10).
const TARGET_MS: u64 = 30;
/// Further behind than this: skip ahead (the Mac's clock runs slower than the guest's, or the
/// queue was held up).
const MAX_LAG_MS: u64 = 150;
/// No new sound for this long: the queue is paused.
const IDLE: Duration = Duration::from_secs(2);

#[repr(C)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct QBuffer {
    capacity: u32,
    data: *mut c_void,
    byte_size: u32,
    user: *mut c_void,
    packet_capacity: u32,
    packets: *mut c_void,
    packet_count: u32,
}

type Queue = *mut c_void;
type Callback = extern "C" fn(*mut c_void, Queue, *mut QBuffer);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(format: *const Asbd, cb: Callback, user: *mut c_void, run_loop: *const c_void, mode: *const c_void, flags: u32, out: *mut Queue) -> i32;
    fn AudioQueueAllocateBuffer(q: Queue, size: u32, out: *mut *mut QBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(q: Queue, buf: *mut QBuffer, n: u32, packets: *const c_void) -> i32;
    fn AudioQueueStart(q: Queue, when: *const c_void) -> i32;
    fn AudioQueuePause(q: Queue) -> i32;
    fn AudioQueueDispose(q: Queue, immediate: u8) -> i32;
}

/// The ring, mapped read-only.
struct Ring {
    p: *const u8,
    len: usize,
}
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    fn open(path: &Path) -> Option<Ring> {
        use std::os::fd::AsRawFd;
        let f = std::fs::File::open(path).ok()?;
        let len = f.metadata().ok()?.len() as usize;
        if len <= HEADER {
            return None;
        }
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return None;
        }
        let r = Ring { p: p as *const u8, len };
        // (not ours, or bhyve has not written the header yet: the caller tries again)
        (r.u32(0) == MAGIC && r.size() > 0 && r.size().is_power_of_two() && HEADER + r.size() <= len).then_some(r)
    }
    fn u32(&self, off: usize) -> u32 {
        unsafe { (*(self.p.add(off) as *const AtomicU32)).load(Ordering::Acquire) }
    }
    fn size(&self) -> usize {
        self.u32(8) as usize
    }
    fn rate(&self) -> u32 {
        self.u32(12)
    }
    fn wpos(&self) -> u64 {
        unsafe { (*(self.p.add(24) as *const AtomicU64)).load(Ordering::Acquire) }
    }
    /// `out.len()` bytes from position `pos`.
    fn copy(&self, pos: u64, out: &mut [u8]) {
        let size = self.size();
        let off = (pos % size as u64) as usize;
        let first = out.len().min(size - off);
        unsafe {
            std::ptr::copy_nonoverlapping(self.p.add(HEADER + off), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(self.p.add(HEADER), out.as_mut_ptr().add(first), out.len() - first);
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.p as *mut c_void, self.len) };
    }
}

/// Where the next bytes for the speakers come from. Kept apart from the AudioQueue so that it
/// can be tested without one.
struct Reader {
    /// the position of the next byte; None: start TARGET behind the writer
    rpos: Option<u64>,
    /// ran dry: silence until TARGET is there again
    priming: bool,
}

impl Reader {
    /// Fill `out` (whole frames) for a stream of `rate` whose writer is at `wpos`; `copy`
    /// reads the ring. Returns false when it gave silence.
    fn fill(&mut self, rate: u32, wpos: u64, ring_size: u64, out: &mut [u8], copy: impl FnOnce(u64, &mut [u8])) -> bool {
        let bytes = |ms: u64| ms * rate as u64 / 1000 * 4;
        let (target, max_lag) = (bytes(TARGET_MS), bytes(MAX_LAG_MS).min(ring_size / 2));
        let mut rpos = match self.rpos {
            // (the writer went back: a new ring)
            Some(r) if r <= wpos => r,
            _ => {
                self.priming = false;
                wpos.saturating_sub(target) & !3
            }
        };
        if wpos - rpos > max_lag {
            rpos = (wpos - target) & !3;
            self.priming = false;
        }
        let avail = wpos - rpos;
        let need = out.len() as u64;
        if self.priming && avail >= target.max(need) {
            self.priming = false;
        }
        let ok = !self.priming && avail >= need;
        if ok {
            copy(rpos, out);
            rpos += need;
        } else {
            out.fill(0);
            self.priming = true;
        }
        self.rpos = Some(rpos);
        ok
    }
}

struct Player {
    ring: Ring,
    rate: u32,
    reader: std::sync::Mutex<Reader>,
    /// the callback hands out silence and forgets its place (the queue is paused or about to be)
    quiet: AtomicBool,
}

extern "C" fn callback(user: *mut c_void, q: Queue, buf: *mut QBuffer) {
    let p = unsafe { &*(user as *const Player) };
    let b = unsafe { &mut *buf };
    let out = unsafe { std::slice::from_raw_parts_mut(b.data as *mut u8, b.capacity as usize) };
    let mut r = p.reader.lock().unwrap();
    if p.quiet.load(Ordering::Relaxed) || p.ring.rate() != p.rate {
        out.fill(0);
        r.rpos = None;
    } else {
        r.fill(p.rate, p.ring.wpos(), p.ring.size() as u64, out, |pos, o| p.ring.copy(pos, o));
    }
    b.byte_size = b.capacity;
    unsafe { AudioQueueEnqueueBuffer(q, buf, 0, std::ptr::null()) };
}

/// An AudioQueue for the ring's present rate; the Player it reads through lives as long.
struct Output {
    q: Queue,
    player: *mut Player,
    running: bool,
}

impl Output {
    fn new(ring: Ring) -> Result<Output, String> {
        let rate = ring.rate();
        if !(8000..=192_000).contains(&rate) {
            return Err(format!("the ring's rate is {rate}"));
        }
        let player = Box::into_raw(Box::new(Player {
            ring,
            rate,
            reader: std::sync::Mutex::new(Reader { rpos: None, priming: false }),
            quiet: AtomicBool::new(true),
        }));
        let f = Asbd {
            sample_rate: rate as f64,
            format_id: u32::from_be_bytes(*b"lpcm"),
            format_flags: 0x4 | 0x8, // signed integer, packed (little-endian)
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: 2,
            bits_per_channel: 16,
            reserved: 0,
        };
        let mut q: Queue = std::ptr::null_mut();
        // (no run loop given: the queue calls back on a thread of its own)
        let st = unsafe { AudioQueueNewOutput(&f, callback, player as *mut c_void, std::ptr::null(), std::ptr::null(), 0, &mut q) };
        if st != 0 {
            drop(unsafe { Box::from_raw(player) });
            return Err(format!("AudioQueueNewOutput: {st}"));
        }
        let o = Output { q, player, running: false };
        for _ in 0..3 {
            let mut b: *mut QBuffer = std::ptr::null_mut();
            let st = unsafe { AudioQueueAllocateBuffer(q, rate * BUF_MS / 1000 * 4, &mut b) };
            if st != 0 {
                return Err(format!("AudioQueueAllocateBuffer: {st}")); // (o's drop disposes of the queue)
            }
            callback(player as *mut c_void, q, b); // silence, enqueued
        }
        Ok(o)
    }
    fn player(&self) -> &Player {
        unsafe { &*self.player }
    }
    fn set_running(&mut self, on: bool) {
        if on == self.running {
            return;
        }
        self.player().quiet.store(!on, Ordering::Relaxed);
        let st = unsafe { if on { AudioQueueStart(self.q, std::ptr::null()) } else { AudioQueuePause(self.q) } };
        if st != 0 {
            eprintln!("sound: AudioQueue{}: {st}", if on { "Start" } else { "Pause" });
        }
        self.running = on;
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // (immediate, synchronous: no callback runs after it returns)
        unsafe { AudioQueueDispose(self.q, 1) };
        drop(unsafe { Box::from_raw(self.player) });
    }
}

/// Play `<dir>/sound.ring` for as long as the process lives: a thread that starts the queue
/// when the guest plays and `shown()` says someone is at the window, and pauses it otherwise.
pub fn start(dir: &Path, shown: fn() -> bool) {
    let path = dir.join("sound.ring");
    let _ = std::thread::Builder::new().name("sound".into()).spawn(move || {
        let mut out: Option<Output> = None;
        let mut last = (0u64, Instant::now()); // the writer's position, and when it last moved
        let mut failed = false;
        loop {
            std::thread::sleep(Duration::from_millis(if out.as_ref().is_some_and(|o| o.running) { 100 } else { 20 }));
            if out.is_none() {
                // (bhyve makes the file when it starts; a ring of another rate is opened anew)
                let Some(ring) = Ring::open(&path) else {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                };
                last = (ring.wpos(), Instant::now() - IDLE);
                match Output::new(ring) {
                    Ok(o) => {
                        eprintln!("sound: the guest's sound card at {} Hz", o.player().rate);
                        out = Some(o);
                        failed = false;
                    }
                    Err(e) => {
                        if !failed {
                            eprintln!("sound: {e}");
                        }
                        failed = true;
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            }
            let o = out.as_mut().unwrap();
            if o.player().ring.rate() != o.player().rate {
                out = None; // the guest changed the rate: a new queue
                continue;
            }
            let w = o.player().ring.wpos();
            if w != last.0 {
                last = (w, Instant::now());
            }
            o.set_running(shown() && last.1.elapsed() < IDLE);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const SIZE: u64 = 256 * 1024;
    /// 10 ms at 48 kHz stereo 16-bit
    const BUF: usize = 1920;

    /// a ring whose byte n is n as u8
    fn take(r: &mut Reader, wpos: u64) -> (bool, Vec<u8>) {
        let mut out = vec![0xaa; BUF];
        let ok = r.fill(RATE, wpos, SIZE, &mut out, |pos, o| o.iter_mut().enumerate().for_each(|(i, b)| *b = (pos + i as u64) as u8));
        (ok, out)
    }

    /// A writer and a reader at the same pace: after the start every buffer is sound, in
    /// order, with nothing left out or twice.
    #[test]
    fn steady() {
        let mut r = Reader { rpos: None, priming: false };
        let mut w = 1_000_000u64;
        let mut next = None;
        for i in 0..500 {
            w += BUF as u64;
            let (ok, out) = take(&mut r, w);
            assert!(ok || i < 4, "silence at buffer {i}");
            if ok {
                let pos = r.rpos.unwrap() - BUF as u64;
                assert_eq!(out[0], pos as u8);
                assert_eq!(out[BUF - 1], (pos + BUF as u64 - 1) as u8);
                if let Some(n) = next {
                    assert_eq!(pos, n, "a jump at buffer {i}");
                }
                next = Some(pos + BUF as u64);
                assert!(w - pos <= TARGET_MS * 192 + BUF as u64);
            }
        }
    }

    /// The writer stops: silence, no stale bytes. It goes on: sound again within TARGET.
    #[test]
    fn gap() {
        let mut r = Reader { rpos: None, priming: false };
        let mut w = 500_000u64;
        for _ in 0..20 {
            w += BUF as u64;
            take(&mut r, w);
        }
        let mut silent = 0;
        for _ in 0..20 {
            let (ok, out) = take(&mut r, w);
            if !ok {
                assert!(out.iter().all(|&b| b == 0));
                silent += 1;
            }
        }
        assert!(silent >= 17, "{silent}");
        let mut back = None;
        for i in 0..10 {
            w += BUF as u64;
            if take(&mut r, w).0 && back.is_none() {
                back = Some(i);
            }
        }
        assert!(back.is_some_and(|i| i <= 4), "{back:?}");
    }

    /// The reader was held up for a second: it skips to TARGET behind the writer, it does not
    /// play the second late. A writer that starts again from 0 (a new ring) is followed too.
    #[test]
    fn lag_and_restart() {
        let mut r = Reader { rpos: None, priming: false };
        let mut w = 400_000u64;
        for _ in 0..10 {
            w += BUF as u64;
            take(&mut r, w);
        }
        w += 192_000;
        assert!(take(&mut r, w).0);
        assert!(w - r.rpos.unwrap() <= TARGET_MS * 192);
        assert_eq!(r.rpos.unwrap() % 4, 0);
        let mut ok = false;
        for i in 1..=10u64 {
            ok = take(&mut r, i * BUF as u64).0;
        }
        assert!(ok);
        assert!(r.rpos.unwrap() <= 10 * BUF as u64);
    }
}
