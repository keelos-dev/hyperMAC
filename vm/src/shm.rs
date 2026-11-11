//! The picture of a VM on our own virtio-gpu (vgpu.rs), shared with the main hyperMAC service's
//! RDP server (the reader: src/vms.rs at the repo root) through a file both map: <VM dir>/screen.shm.
//!
//! Layout: a 4096-byte header, then the pixels (B,G,R,X rows, stride = w * 4, the hardware cursor
//! drawn in), room for 3840 x 2160.
//!   0  u64 magic "HMSCRN01"
//!   8  u64 seq: odd while the VM writes a frame, even when it is whole (a seqlock)
//!   16 u32 w, 20 u32 h
//!   24 u64 wanted until (ms since the epoch): a reader sets it; the VM only copies frames
//!      while it is in the future, so nobody watching costs nothing
//!   32 u64 frame number (vgpu's)

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const HEADER: usize = 4096;
pub const MAX_PIXELS: usize = 3840 * 2160;
pub const SIZE: usize = HEADER + MAX_PIXELS * 4;
const MAGIC: u64 = u64::from_le_bytes(*b"HMSCRN01");

pub struct Shm {
    p: *mut u8,
}
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

impl Shm {
    /// Map `path` (created and sized if needed) read-write.
    pub fn open(path: &Path, create: bool) -> anyhow::Result<Shm> {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(create).truncate(false).open(path)?;
        if create && f.metadata()?.len() < SIZE as u64 {
            f.set_len(SIZE as u64)?;
        }
        anyhow::ensure!(f.metadata()?.len() >= SIZE as u64, "{}: too small", path.display());
        use std::os::fd::AsRawFd;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        anyhow::ensure!(p != libc::MAP_FAILED, "mmap {}: {}", path.display(), std::io::Error::last_os_error());
        let s = Shm { p: p as *mut u8 };
        if create {
            unsafe { (s.p as *mut u64).write_volatile(MAGIC) };
        }
        Ok(s)
    }

    fn a64(&self, off: usize) -> &AtomicU64 {
        unsafe { &*(self.p.add(off) as *const AtomicU64) }
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }

    /// Writer: is anybody watching?
    pub fn wanted(&self) -> bool {
        self.a64(24).load(Ordering::Relaxed) > Self::now_ms()
    }

    /// Writer: one frame (B,G,R,X, w * h * 4 bytes).
    pub fn write(&self, frame: u64, w: u32, h: u32, pix: &[u8]) {
        let n = (w as usize * h as usize * 4).min(pix.len()).min(MAX_PIXELS * 4);
        let seq = self.a64(8);
        seq.fetch_add(1, Ordering::AcqRel); // odd: writing
        unsafe {
            std::ptr::copy_nonoverlapping(pix.as_ptr(), self.p.add(HEADER), n);
            (self.p.add(16) as *mut u32).write_volatile(w);
            (self.p.add(20) as *mut u32).write_volatile(h);
        }
        self.a64(32).store(frame, Ordering::Relaxed);
        seq.fetch_add(1, Ordering::AcqRel); // even: whole
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.p as *mut _, SIZE) };
    }
}

/// In the VM process (own devices): export vgpu's picture to <dir>/screen.shm while someone
/// reads it.
pub fn export(dir: &Path) -> anyhow::Result<()> {
    let s = Shm::open(&dir.join("screen.shm"), true)?;
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            // (twice a 60 Hz frame: a poll that just misses a frame costs half of one, not a whole)
            std::thread::sleep(std::time::Duration::from_millis(8));
            if !s.wanted() {
                last = None; // a new reader gets a whole frame at once
                continue;
            }
            if let Some((n, w, h, pix)) = crate::vgpu::snapshot(last) {
                if !pix.is_empty() {
                    last = Some(n);
                    s.write(n, w, h, &pix);
                }
            }
        }
    });
    Ok(())
}
