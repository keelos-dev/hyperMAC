//! A VM's sound to the RDP client that has the VM open (vms.rs). bhyve's HD Audio card writes
//! what a Windows guest plays into a ring in <VM dir>/sound.ring (hyperMAC vm/src/sound.rs
//! plays the same ring on this Mac while the VM's window is open; keelOS's layout,
//! vm/bhyve/keel/audio.h):
//!   0  u32 magic "KSN1"      4  u32 version     8  u32 size (of the data)
//!   12 u32 rate              16 u32 epoch       20 u32 pid
//!   24 u64 wpos: bytes written in all; byte n is at 64 + n % size
//! 16-bit little-endian stereo at the guest's rate (44.1 or 48 kHz). A thread per connection
//! follows wpos and hands the new samples to the connection's RDPSND (sound.rs), brought to
//! 48 kHz when the guest plays at another rate. Nothing is replayed: it starts where the guest
//! is now, and while the client takes no sound it only keeps its place.

use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use tracing::{info, warn};

const MAGIC: u32 = 0x314e_534b;
const HEADER: usize = 64;

struct Ring {
    p: *const u8,
    len: usize,
}
unsafe impl Send for Ring {}

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
    /// The samples of bytes `from..to` (whole frames, at most the ring's size).
    fn read(&self, from: u64, to: u64) -> Vec<i16> {
        let size = self.size();
        let mut bytes = vec![0u8; (to - from) as usize];
        let off = (from % size as u64) as usize;
        let first = bytes.len().min(size - off);
        unsafe {
            std::ptr::copy_nonoverlapping(self.p.add(HEADER + off), bytes.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(self.p.add(HEADER), bytes.as_mut_ptr().add(first), bytes.len() - first);
        }
        bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.p as *mut libc::c_void, self.len) };
    }
}

/// Stereo from one rate to another by straight lines between samples: enough for the one
/// case there is (44.1 to 48 kHz, a guest set to "CD quality"). Keeps its place between calls.
struct Resampler {
    from: u32,
    to: u32,
    /// where the next output frame lies, in input frames past `prev`, as a count of 1/to
    phase: u64,
    prev: [f32; 2],
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Resampler { from, to, phase: 0, prev: [0.0; 2] }
    }
    /// Interleaved stereo in, interleaved stereo out.
    fn run(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for f in input.chunks_exact(2) {
            // output frames that lie between prev and f
            while self.phase < self.to as u64 {
                let t = self.phase as f32 / self.to as f32;
                out.push(self.prev[0] + (f[0] - self.prev[0]) * t);
                out.push(self.prev[1] + (f[1] - self.prev[1]) * t);
                self.phase += self.from as u64;
            }
            self.phase -= self.to as u64;
            self.prev = [f[0], f[1]];
        }
    }
}

/// Follow the VM's ring for one connection until `alive` is gone (the connection's display
/// updates were dropped).
pub fn start(name: &str, sound: crate::sound::Sound, alive: std::sync::Weak<()>) {
    let path = crate::vms::vm_dir(name).join("sound.ring");
    let name = name.to_string();
    let _ = std::thread::Builder::new().name("vm-sound".into()).spawn(move || {
        // (a VM without the card has no ring; one that starts later makes it then)
        let mut ring: Option<Ring> = None;
        let mut rpos = 0u64;
        let mut resampler: Option<Resampler> = None;
        let mut out = Vec::new();
        while alive.strong_count() > 0 {
            let Some(r) = &ring else {
                ring = Ring::open(&path);
                match &ring {
                    Some(r) => {
                        rpos = r.wpos();
                        info!(vm = name, rate = r.rate(), "sound: the VM's sound card");
                    }
                    None => std::thread::sleep(Duration::from_secs(1)),
                }
                continue;
            };
            std::thread::sleep(Duration::from_millis(5));
            let (w, rate) = (r.wpos() & !3, r.rate());
            // the writer went back (a new ring in the same file), or we fell a long way behind
            if w < rpos || w - rpos > r.size() as u64 / 2 {
                rpos = w;
                continue;
            }
            if w == rpos {
                continue;
            }
            if !sound.on() || rate == 0 {
                rpos = w; // nobody listens: keep the place
                resampler = None;
                continue;
            }
            let pcm: Vec<f32> = r.read(rpos, w).iter().map(|&s| s as f32 / 32767.0).collect();
            rpos = w;
            if rate == crate::sound::RATE {
                resampler = None;
                sound.push_vm(&pcm);
            } else {
                if resampler.as_ref().is_none_or(|x| x.from != rate) {
                    warn!(vm = name, rate, "sound: the guest plays at another rate than 48 kHz, converted here");
                    resampler = Some(Resampler::new(rate, crate::sound::RATE));
                }
                out.clear();
                resampler.as_mut().unwrap().run(&pcm, &mut out);
                sound.push_vm(&out);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring file as bhyve writes it, wrapped once: read() gives the samples in order.
    #[test]
    fn ring_read_wraps() {
        let size = 4096usize;
        let mut file = vec![0u8; HEADER + size];
        file[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        file[8..12].copy_from_slice(&(size as u32).to_le_bytes());
        file[12..16].copy_from_slice(&48000u32.to_le_bytes());
        // samples numbered by their position in the stream: sample k (bytes 2k, 2k+1) = k as i16
        let wpos = 6000u64;
        for k in (wpos as usize - size) / 2..wpos as usize / 2 {
            let off = HEADER + (2 * k) % size;
            file[off..off + 2].copy_from_slice(&(k as i16).to_le_bytes());
        }
        file[24..32].copy_from_slice(&wpos.to_le_bytes());
        let path = std::env::temp_dir().join(format!("hm-ring-test-{}", std::process::id()));
        std::fs::write(&path, &file).unwrap();
        let r = Ring::open(&path).unwrap();
        assert_eq!((r.size(), r.rate(), r.wpos()), (size, 48000, wpos));
        let got = r.read(3000, 6000); // crosses the end of the data at byte 4096
        assert_eq!(got, (1500..3000).map(|k| k as i16).collect::<Vec<_>>());
        let _ = std::fs::remove_file(&path);
        // not a ring
        std::fs::write(&path, vec![0u8; HEADER + size]).unwrap();
        assert!(Ring::open(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// One second of a 1 kHz tone at 44.1 kHz, in pieces of odd sizes: 48000 frames come out
    /// (give or take one), still a 1 kHz tone of the same level, the same on both channels.
    #[test]
    fn resample_44100() {
        let input: Vec<f32> = (0..44100)
            .flat_map(|i| {
                let s = (i as f32 / 44100.0 * 1000.0 * std::f32::consts::TAU).sin() * 0.5;
                [s, s]
            })
            .collect();
        let mut rs = Resampler::new(44100, 48000);
        let mut out = Vec::new();
        let mut at = 0;
        for n in [2usize, 882, 14, 3000, 5000].iter().cycle() {
            if at >= input.len() {
                break;
            }
            let end = (at + n).min(input.len());
            rs.run(&input[at..end], &mut out);
            at = end;
        }
        let frames = out.len() / 2;
        assert!((47999..=48001).contains(&frames), "{frames}");
        let left: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
        assert!(out.chunks_exact(2).all(|f| f[0] == f[1]));
        let up = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((999..=1001).contains(&up), "{up} Hz");
        let rms = (left.iter().map(|x| x * x).sum::<f32>() / frames as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "rms {rms}");
    }
}
