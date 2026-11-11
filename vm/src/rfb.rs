//! Our own VNC server (RFB 3.3/3.7/3.8) for VMs on our virtio-gpu and virtio-input
//! (HYPERMAC_VIRTIO=1): VZ's private VNC server only knows VZ's own graphics and USB HID.
//! Picture: the virtio-gpu scanout (vgpu::snapshot), Raw encoding, DesktopSize when the guest
//! changes mode. Input: X keysyms -> Linux key codes, the absolute pointer and three buttons,
//! into vinput. Auth: VNC password (DES challenge), the VM's vnc_password. Listens on every
//! interface, like VZ's server did.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use des::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};

/// Listen on `port` (0 = any); returns the port.
pub fn start(port: u16, password: String) -> anyhow::Result<u16> {
    let l = TcpListener::bind(("0.0.0.0", port))?;
    let port = l.local_addr()?.port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let pw = password.clone();
            std::thread::spawn(move || {
                let peer = c.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                if let Err(e) = session(c, &pw) {
                    eprintln!("vnc {peer}: {e}");
                }
            });
        }
    });
    Ok(port)
}

#[derive(Clone, Copy)]
struct Fmt {
    bpp: u8,
    big: bool,
    max: [u16; 3],
    shift: [u8; 3],
}
const OURS: Fmt = Fmt { bpp: 32, big: false, max: [255, 255, 255], shift: [16, 8, 0] };

struct Shared {
    fmt: Mutex<Fmt>,
    /// 0 none, 1 incremental, 2 full
    want: AtomicU8,
    desktop_size: AtomicBool,
    gone: AtomicBool,
}

fn vnc_key(password: &str) -> [u8; 8] {
    let mut k = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        k[i] = b.reverse_bits(); // VNC's DES key quirk: each byte bit-reversed
    }
    k
}

fn session(mut s: TcpStream, password: &str) -> anyhow::Result<()> {
    s.set_nodelay(true)?;
    s.write_all(b"RFB 003.008\n")?;
    let mut v = [0u8; 12];
    s.read_exact(&mut v)?;
    let minor: u32 = std::str::from_utf8(&v[8..11]).unwrap_or("3").parse().unwrap_or(3);
    if minor >= 7 {
        s.write_all(&[1, 2])?; // one type: VNC authentication
        let mut t = [0u8];
        s.read_exact(&mut t)?;
        anyhow::ensure!(t[0] == 2, "client chose security type {}", t[0]);
    } else {
        s.write_all(&2u32.to_be_bytes())?;
    }
    let mut ch = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut ch)?;
    s.write_all(&ch)?;
    let mut resp = [0u8; 16];
    s.read_exact(&mut resp)?;
    let cipher = des::Des::new(&GenericArray::from(vnc_key(password)));
    let mut want = ch;
    for blk in want.chunks_exact_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(blk));
    }
    if resp != want {
        s.write_all(&1u32.to_be_bytes())?;
        if minor >= 8 {
            let m = b"wrong password";
            s.write_all(&(m.len() as u32).to_be_bytes())?;
            s.write_all(m)?;
        }
        anyhow::bail!("wrong password");
    }
    s.write_all(&0u32.to_be_bytes())?;
    let mut shared_flag = [0u8];
    s.read_exact(&mut shared_flag)?;

    // the first frame decides the size
    let (w, h) = loop {
        if let Some(wh) = crate::vgpu::size() {
            break wh;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut init = Vec::new();
    init.extend_from_slice(&(w as u16).to_be_bytes());
    init.extend_from_slice(&(h as u16).to_be_bytes());
    init.extend_from_slice(&pixel_format(&OURS));
    let name = b"hyperMAC";
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name);
    s.write_all(&init)?;

    let sh = Arc::new(Shared {
        fmt: Mutex::new(OURS),
        want: AtomicU8::new(0),
        desktop_size: AtomicBool::new(false),
        gone: AtomicBool::new(false),
    });
    let (sh2, mut out) = (sh.clone(), s.try_clone()?);
    std::thread::spawn(move || {
        if let Err(e) = sender(&mut out, &sh2, (w, h)) {
            if !sh2.gone.load(Ordering::Relaxed) {
                eprintln!("vnc: send: {e}");
            }
        }
        sh2.gone.store(true, Ordering::Relaxed);
    });
    let r = reader(&mut s, &sh, (w, h));
    sh.gone.store(true, Ordering::Relaxed);
    r
}

fn pixel_format(f: &Fmt) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = f.bpp;
    p[1] = if f.bpp == 32 { 24 } else { f.bpp };
    p[2] = f.big as u8;
    p[3] = 1;
    p[4..6].copy_from_slice(&f.max[0].to_be_bytes());
    p[6..8].copy_from_slice(&f.max[1].to_be_bytes());
    p[8..10].copy_from_slice(&f.max[2].to_be_bytes());
    p[10] = f.shift[0];
    p[11] = f.shift[1];
    p[12] = f.shift[2];
    p
}

fn reader(s: &mut TcpStream, sh: &Shared, mut size: (u32, u32)) -> anyhow::Result<()> {
    let mut buttons = 0u8;
    loop {
        let mut t = [0u8];
        s.read_exact(&mut t)?;
        match t[0] {
            0 => {
                // SetPixelFormat: pad 3, format 16
                let mut b = [0u8; 19];
                s.read_exact(&mut b)?;
                let p = &b[3..];
                anyhow::ensure!(p[3] == 1, "only true-colour pixel formats");
                let f = Fmt {
                    bpp: p[0],
                    big: p[2] != 0,
                    max: [u16::from_be_bytes([p[4], p[5]]), u16::from_be_bytes([p[6], p[7]]), u16::from_be_bytes([p[8], p[9]])],
                    shift: [p[10], p[11], p[12]],
                };
                anyhow::ensure!(matches!(f.bpp, 8 | 16 | 32), "{} bits per pixel", f.bpp);
                *sh.fmt.lock().unwrap() = f;
            }
            2 => {
                // SetEncodings: pad 1, count 2, then i32 each; we always send Raw
                let mut b = [0u8; 3];
                s.read_exact(&mut b)?;
                let n = u16::from_be_bytes([b[1], b[2]]) as usize;
                let mut e = vec![0u8; n * 4];
                s.read_exact(&mut e)?;
                let ds = e.chunks_exact(4).any(|c| i32::from_be_bytes(c.try_into().unwrap()) == -223);
                sh.desktop_size.store(ds, Ordering::Relaxed);
            }
            3 => {
                // FramebufferUpdateRequest: incremental, x, y, w, h
                let mut b = [0u8; 9];
                s.read_exact(&mut b)?;
                let full = if b[0] == 0 { 2 } else { 1 };
                sh.want.fetch_max(full, Ordering::Relaxed);
            }
            4 => {
                // KeyEvent: down, pad 2, keysym
                let mut b = [0u8; 7];
                s.read_exact(&mut b)?;
                let sym = u32::from_be_bytes([b[3], b[4], b[5], b[6]]);
                if let Some(code) = keysym_to_linux(sym) {
                    crate::vinput::key(code, b[0] != 0);
                }
            }
            5 => {
                // PointerEvent: buttons, x, y
                let mut b = [0u8; 5];
                s.read_exact(&mut b)?;
                let (x, y) = (u16::from_be_bytes([b[1], b[2]]) as f64, u16::from_be_bytes([b[3], b[4]]) as f64);
                if let Some(wh) = crate::vgpu::size() {
                    size = wh;
                }
                crate::vinput::abs(x / (size.0.max(2) - 1) as f64, y / (size.1.max(2) - 1) as f64);
                // RFB: bit 0 left, 1 middle, 2 right; vinput: 0 left, 1 right, 2 middle
                for (bit, btn) in [(0u8, 0u16), (1, 2), (2, 1)] {
                    let (was, now) = (buttons >> bit & 1, b[0] >> bit & 1);
                    if was != now {
                        crate::vinput::button(btn, now != 0);
                    }
                }
                buttons = b[0];
            }
            6 => {
                // ClientCutText: pad 3, length, text
                let mut b = [0u8; 7];
                s.read_exact(&mut b)?;
                let n = u32::from_be_bytes([b[3], b[4], b[5], b[6]]) as usize;
                std::io::copy(&mut s.take(n as u64), &mut std::io::sink())?;
            }
            t => anyhow::bail!("unknown client message {t}"),
        }
    }
}

fn sender(s: &mut TcpStream, sh: &Shared, mut size: (u32, u32)) -> anyhow::Result<()> {
    let mut last: Option<u64> = None;
    loop {
        if sh.gone.load(Ordering::Relaxed) {
            return Ok(());
        }
        let want = sh.want.load(Ordering::Relaxed);
        if want == 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let Some((n, w, h, pix)) = crate::vgpu::snapshot(if want == 2 { None } else { last }) else {
            std::thread::sleep(Duration::from_millis(30));
            continue;
        };
        if pix.is_empty() {
            // nothing new yet: an incremental request waits
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }
        sh.want.store(0, Ordering::Relaxed);
        last = Some(n);
        let mut msg = Vec::with_capacity(16 + pix.len());
        msg.extend_from_slice(&[0, 0]);
        if (w, h) != size && sh.desktop_size.load(Ordering::Relaxed) {
            size = (w, h);
            msg.extend_from_slice(&2u16.to_be_bytes());
            rect(&mut msg, 0, 0, w, h, -223);
        } else {
            msg.extend_from_slice(&1u16.to_be_bytes());
        }
        // clients that can't resize get the top-left part of a bigger picture
        let (cw, ch) = if sh.desktop_size.load(Ordering::Relaxed) { (w, h) } else { (size.0.min(w), size.1.min(h)) };
        rect(&mut msg, 0, 0, cw, ch, 0);
        let f = *sh.fmt.lock().unwrap();
        encode(&mut msg, &pix, w, cw, ch, &f);
        s.write_all(&msg)?;
    }
}

fn rect(m: &mut Vec<u8>, x: u32, y: u32, w: u32, h: u32, enc: i32) {
    for v in [x, y, w, h] {
        m.extend_from_slice(&(v as u16).to_be_bytes());
    }
    m.extend_from_slice(&enc.to_be_bytes());
}

/// B,G,R,X source rows (stride w) -> the client's pixel format.
fn encode(m: &mut Vec<u8>, pix: &[u8], w: u32, cw: u32, ch: u32, f: &Fmt) {
    let same = f.bpp == 32 && !f.big && f.max == [255; 3] && f.shift == [16, 8, 0];
    for y in 0..ch as usize {
        let row = &pix[y * w as usize * 4..(y * w as usize + cw as usize) * 4];
        if same {
            m.extend_from_slice(row);
            continue;
        }
        for p in row.chunks_exact(4) {
            let c = [p[2], p[1], p[0]];
            let mut v = 0u32;
            for i in 0..3 {
                v |= (c[i] as u32 * f.max[i] as u32 / 255) << f.shift[i];
            }
            match (f.bpp, f.big) {
                (32, false) => m.extend_from_slice(&v.to_le_bytes()),
                (32, true) => m.extend_from_slice(&v.to_be_bytes()),
                (16, false) => m.extend_from_slice(&(v as u16).to_le_bytes()),
                (16, true) => m.extend_from_slice(&(v as u16).to_be_bytes()),
                _ => m.push(v as u8),
            }
        }
    }
}

/// X keysym -> Linux input key code (US layout positions; the client sends Shift itself).
fn keysym_to_linux(sym: u32) -> Option<u16> {
    let k = match sym {
        0x61..=0x7a => return keysym_to_linux(sym - 0x20),
        0x41..=0x5a => {
            const L: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
            L[(sym - 0x41) as usize]
        }
        0x31..=0x39 => 2 + (sym - 0x31) as u16,
        0x30 => 11,
        0x20 => 57,
        // shifted symbols on the US layout -> their key
        0x21 => 2,
        0x40 => 3,
        0x23 => 4,
        0x24 => 5,
        0x25 => 6,
        0x5e => 7,
        0x26 => 8,
        0x2a => 9,
        0x28 => 10,
        0x29 => 11,
        0x2d | 0x5f => 12,
        0x3d | 0x2b => 13,
        0x5b | 0x7b => 26,
        0x5d | 0x7d => 27,
        0x5c | 0x7c => 43,
        0x3b | 0x3a => 39,
        0x27 | 0x22 => 40,
        0x60 | 0x7e => 41,
        0x2c | 0x3c => 51,
        0x2e | 0x3e => 52,
        0x2f | 0x3f => 53,
        0xff08 => 14,  // BackSpace
        0xff09 => 15,  // Tab
        0xff0d => 28,  // Return
        0xff1b => 1,   // Escape
        0xffff => 111, // Delete
        0xff50 => 102, // Home
        0xff51 => 105, // Left
        0xff52 => 103, // Up
        0xff53 => 106, // Right
        0xff54 => 108, // Down
        0xff55 => 104, // Page Up
        0xff56 => 109, // Page Down
        0xff57 => 107, // End
        0xff63 => 110, // Insert
        0xffbe..=0xffc7 => 59 + (sym - 0xffbe) as u16, // F1..F10
        0xffc8 => 87,
        0xffc9 => 88,
        0xffe1 => 42,  // Shift_L
        0xffe2 => 54,  // Shift_R
        0xffe3 => 29,  // Control_L
        0xffe4 => 97,  // Control_R
        0xffe9 | 0xffe7 => 56, // Alt_L / Meta_L
        0xffea | 0xffe8 => 100, // Alt_R / Meta_R
        0xffeb => 125, // Super_L (Windows key; the Mac's Command)
        0xffec => 126, // Super_R
        0xffe5 => 58,  // Caps_Lock
        0xff61 => 99,  // Print
        _ => return None,
    };
    Some(k)
}
