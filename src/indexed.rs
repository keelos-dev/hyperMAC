//! The lossless picture for screens of few colours: `keelOS::Video` codec_id 4 (video.rs).
//! H.264 as VideoToolbox makes it is 4:2:0, the colour at half the resolution: a text console's
//! one pixel wide coloured strokes came out grey with the colour smeared beside them (user
//! 2026-11-13, a Debian console in kRDPclient). Such a screen has a handful of colours, so it
//! goes as palette indices, deflated: exact, and small (most of a console is one colour).
//!
//! A FRAME's data, little-endian:
//!   u16 x, y, w, h    the rectangle this frame replaces (a keyframe: the whole picture)
//!   u16 n             colours, 1..=256
//!   n * 3 bytes       the palette: R, G, B
//!   zlib stream       w * h bytes, a palette index per pixel, rows top down
//! A frame that is not a keyframe leaves the rest of the picture as the frames before made it.
//! kRDPclient's side: src/video/indexed.rs.

use std::io::Write;

pub const MAX_COLOURS: usize = 256;

/// x, y, w, h in pixels
pub type Rect = (usize, usize, usize, usize);

/// The smallest rectangle outside which two pictures of `w` x `h` (4 bytes a pixel) are the
/// same; None: they are the same everywhere.
pub fn changed(prev: &[u8], cur: &[u8], w: usize, h: usize) -> Option<Rect> {
    let (mut x0, mut x1, mut y0, mut y1) = (w, 0usize, h, 0usize);
    for y in 0..h {
        let (a, b) = (&prev[y * w * 4..][..w * 4], &cur[y * w * 4..][..w * 4]);
        if a == b {
            continue;
        }
        y0 = y0.min(y);
        y1 = y + 1;
        // only what would widen the rectangle is looked at byte by byte
        if let Some(p) = a[..x0 * 4].iter().zip(&b[..x0 * 4]).position(|(p, q)| p != q) {
            x0 = p / 4;
        }
        if let Some(p) = a[x1 * 4..].iter().zip(&b[x1 * 4..]).rposition(|(p, q)| p != q) {
            x1 += p / 4 + 1;
        }
    }
    (y1 > y0).then(|| (x0, y0, x1 - x0, y1 - y0))
}

/// A FRAME's data for `rect` of a picture `w` wide (B, G, R, x per pixel); None: the rectangle
/// has more than MAX_COLOURS colours (the screen is no console any more: H.264's turn).
pub fn encode(pix: &[u8], w: usize, (x, y, rw, rh): Rect) -> Option<Vec<u8>> {
    // colour -> index, open addressing; a colour is 24 bits, so FREE is none
    const FREE: u32 = u32::MAX;
    let mut keys = [FREE; 1024];
    let mut vals = [0u8; 1024];
    let mut palette: Vec<u8> = Vec::with_capacity(MAX_COLOURS * 3);
    let mut idx: Vec<u8> = Vec::with_capacity(rw * rh);
    let (mut last, mut last_i) = (FREE, 0u8);
    for row in y..y + rh {
        for p in pix[(row * w + x) * 4..][..rw * 4].chunks_exact(4) {
            let c = u32::from_le_bytes([p[0], p[1], p[2], 0]);
            if c != last {
                let mut slot = (c.wrapping_mul(0x9E37_79B1) >> 22) as usize;
                last_i = loop {
                    if keys[slot] == c {
                        break vals[slot];
                    }
                    if keys[slot] == FREE {
                        let n = palette.len() / 3;
                        if n == MAX_COLOURS {
                            return None;
                        }
                        keys[slot] = c;
                        vals[slot] = n as u8;
                        palette.extend_from_slice(&[p[2], p[1], p[0]]);
                        break n as u8;
                    }
                    slot = (slot + 1) & 1023;
                };
                last = c;
            }
            idx.push(last_i);
        }
    }
    let mut out = Vec::with_capacity(16 + palette.len() + idx.len() / 16);
    for v in [x, y, rw, rh, palette.len() / 3] {
        out.extend_from_slice(&(v as u16).to_le_bytes());
    }
    out.extend_from_slice(&palette);
    // the fastest level: the indices of a console are long runs, any level makes them small
    let mut z = flate2::write::ZlibEncoder::new(out, flate2::Compression::fast());
    z.write_all(&idx).ok()?;
    z.finish().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// What kRDPclient does with a frame: into `pic` (B, G, R, 0 per pixel), `w` wide.
    fn apply(pic: &mut [u8], w: usize, data: &[u8]) {
        let f = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]) as usize;
        let (x, y, rw, rh, n) = (f(0), f(2), f(4), f(6), f(8));
        let palette = &data[10..10 + n * 3];
        let mut idx = Vec::new();
        flate2::read::ZlibDecoder::new(&data[10 + n * 3..]).read_to_end(&mut idx).unwrap();
        assert_eq!(idx.len(), rw * rh);
        for (i, &c) in idx.iter().enumerate() {
            let (px, py) = (x + i % rw, y + i / rw);
            let c = &palette[c as usize * 3..][..3];
            pic[(py * w + px) * 4..][..4].copy_from_slice(&[c[2], c[1], c[0], 0]);
        }
    }

    /// A console-like picture: black, with "text" in a few colours.
    fn console(w: usize, h: usize, seed: u32) -> Vec<u8> {
        const COLOURS: [[u8; 4]; 5] = [[170, 170, 170, 0], [0, 85, 170, 0], [0, 170, 0, 0], [255, 255, 255, 0], [170, 0, 0, 0]];
        let mut pix = vec![0u8; w * h * 4];
        let mut r = seed;
        for y in 0..h {
            let colour = COLOURS[(y / 16 + seed as usize) % COLOURS.len()];
            for x in 0..w {
                r = r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if x < w * 2 / 3 && r >> 30 == 0 {
                    pix[(y * w + x) * 4..][..4].copy_from_slice(&colour);
                }
            }
        }
        pix
    }

    #[test]
    fn a_keyframe_and_a_change_come_out_exact() {
        let (w, h) = (320, 96);
        let a = console(w, h, 1);
        let mut b = a.clone();
        for y in 40..56 {
            for x in 100..108 {
                b[(y * w + x) * 4..][..4].copy_from_slice(&[0, 85, 170, 0]);
            }
        }
        // (a pixel of the block may have had that colour already: the rectangle is inside it)
        let rect = changed(&a, &b, w, h).unwrap();
        assert!(rect.0 >= 100 && rect.0 + rect.2 <= 108 && rect.1 >= 40 && rect.1 + rect.3 <= 56, "{rect:?}");
        assert_eq!(changed(&a, &a, w, h), None);

        let mut pic = vec![0u8; w * h * 4];
        apply(&mut pic, w, &encode(&a, w, (0, 0, w, h)).unwrap());
        assert_eq!(pic, a);
        apply(&mut pic, w, &encode(&b, w, rect).unwrap());
        assert_eq!(pic, b);
    }

    #[test]
    fn one_changed_pixel_is_a_rectangle_of_one() {
        let (w, h) = (64, 32);
        let a = vec![0u8; w * h * 4];
        let mut b = a.clone();
        b[(7 * w + 63) * 4 + 1] = 9;
        assert_eq!(changed(&a, &b, w, h), Some((63, 7, 1, 1)));
        b[(20 * w) * 4] = 9;
        assert_eq!(changed(&a, &b, w, h), Some((0, 7, 64, 14)));
    }

    #[test]
    fn a_picture_of_many_colours_is_refused_and_256_are_taken() {
        let (w, h) = (300, 2);
        let mut pix = vec![0u8; w * h * 4];
        for (i, p) in pix.chunks_exact_mut(4).enumerate() {
            p[0] = (i % 256) as u8;
        }
        let mut pic = vec![0u8; w * h * 4];
        apply(&mut pic, w, &encode(&pix, w, (0, 0, w, h)).unwrap());
        assert_eq!(pic, pix);
        pix[1] = 1; // the 257th colour
        assert!(encode(&pix, w, (0, 0, w, h)).is_none());
        // the x byte is no colour
        pix[1] = 0;
        pix[3] = 255;
        assert!(encode(&pix, w, (0, 0, w, h)).is_some());
    }

    /// Not a test of anything: what a whole 1080p console costs (cargo test -- --nocapture).
    #[test]
    fn a_full_hd_console_is_small() {
        let (w, h) = (1920, 1080);
        let a = console(w, h, 3);
        let t = std::time::Instant::now();
        let data = encode(&a, w, (0, 0, w, h)).unwrap();
        println!("1080p console: {} bytes in {:?}", data.len(), t.elapsed());
        assert!(data.len() < w * h / 2);
    }
}
