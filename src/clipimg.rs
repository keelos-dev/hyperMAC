//! Pictures on the clipboard (clip.rs): the Mac keeps PNG / TIFF on NSPasteboard, Windows
//! CF_DIB (a BITMAPINFOHEADER + pixels) and, in most apps, a registered "PNG" format.
//!
//! - Mac → client: CF_DIB (32-bit BGRA, bottom-up) made by drawing the pasteboard picture into
//!   a known RGBA bitmap; "PNG" is the pasteboard's own PNG (or the TIFF re-encoded).
//! - Client → Mac: "PNG" goes on as it is; a DIB (24- or 32-bit, BI_RGB or BI_BITFIELDS, any
//!   header version, top-down or bottom-up) becomes RGBA, then PNG + TIFF on the pasteboard.
//! The DIB code is plain Rust (tested below); AppKit only encodes and decodes PNG / TIFF.

use objc2::AllocAnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageFileType, NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext, NSPasteboard,
    NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSPoint, NSRect, NSSize};

/// Pictures bigger than this (pixels) are not offered or taken: ~64 MB of BGRA.
const MAX_PIXELS: usize = 16_000_000;

/// An image as straight (not premultiplied) RGBA rows, top row first.
pub struct Rgba {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// RGBA -> CF_DIB: BITMAPINFOHEADER (40 bytes, 32 bpp, BI_RGB) + BGRA rows bottom-up.
pub fn to_dib(img: &Rgba) -> Vec<u8> {
    let (w, h) = (img.w, img.h);
    let mut out = Vec::with_capacity(40 + w * h * 4);
    let le32 = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    le32(&mut out, 40); // biSize
    le32(&mut out, w as u32); // biWidth
    le32(&mut out, h as u32); // biHeight > 0: bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    le32(&mut out, 0); // BI_RGB
    le32(&mut out, (w * h * 4) as u32); // biSizeImage
    le32(&mut out, 2835); // 72 dpi
    le32(&mut out, 2835);
    le32(&mut out, 0);
    le32(&mut out, 0);
    for y in (0..h).rev() {
        for p in img.px[y * w * 4..(y + 1) * w * 4].chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

/// CF_DIB / CF_DIBV5 -> RGBA. 24- and 32-bit only (what screenshots and image apps put there).
pub fn from_dib(d: &[u8]) -> Option<Rgba> {
    let u32_at = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let hdr = u32_at(0)? as usize;
    if hdr < 40 {
        return None;
    }
    let w = u32_at(4)? as i32;
    let hgt = u32_at(8)? as i32;
    let bits = u16::from_le_bytes(d.get(14..16)?.try_into().ok()?);
    let compression = u32_at(16)?;
    let used = u32_at(32)? as usize;
    if w <= 0 || hgt == 0 || !matches!(bits, 24 | 32) || !matches!(compression, 0 | 3) {
        return None;
    }
    let (w, h, top_down) = (w as usize, hgt.unsigned_abs() as usize, hgt < 0);
    if w * h > MAX_PIXELS {
        return None;
    }
    // channel masks: BI_BITFIELDS has them after a 40-byte header, or inside V4/V5 headers
    let (masks, extra) = if compression == 3 {
        if hdr == 40 {
            ([u32_at(40)?, u32_at(44)?, u32_at(48)?, 0], 12)
        } else {
            ([u32_at(40)?, u32_at(44)?, u32_at(48)?, u32_at(52)?], 0)
        }
    } else {
        ([0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0], 0)
    };
    let start = hdr + extra + used * 4;
    let bpp = bits as usize / 8;
    let stride = (w * bpp + 3) & !3;
    if d.len() < start + stride * h {
        return None;
    }
    let chan = |v: u32, m: u32| if m == 0 { 0 } else { ((v & m) >> m.trailing_zeros()) as u8 };
    let mut px = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        let src_row = if top_down { y } else { h - 1 - y };
        let row = &d[start + src_row * stride..];
        for x in 0..w {
            let o = (y * w + x) * 4;
            if bpp == 3 {
                let p = &row[x * 3..x * 3 + 3];
                px[o..o + 4].copy_from_slice(&[p[2], p[1], p[0], 255]);
            } else {
                let v = u32::from_le_bytes(row[x * 4..x * 4 + 4].try_into().unwrap());
                let a = if compression == 0 { (v >> 24) as u8 } else { chan(v, masks[3]) };
                any_alpha |= a != 0;
                px[o..o + 4].copy_from_slice(&[chan(v, masks[0]), chan(v, masks[1]), chan(v, masks[2]), a]);
            }
        }
    }
    // 32-bit DIBs mostly leave the 4th byte 0 (no alpha): opaque, not invisible
    if bpp == 4 && !any_alpha {
        px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    Some(Rgba { w, h, px })
}

// ---- AppKit side ----------------------------------------------------------------------------

/// The pasteboard has a picture to offer (and not a copied file, whose icon comes along).
pub fn mac_has_image() -> bool {
    has_image(&NSPasteboard::generalPasteboard())
}

fn has_image(pb: &NSPasteboard) -> bool {
    let Some(types) = pb.types() else { return false };
    let has = |t: &objc2_app_kit::NSPasteboardType| types.iter().any(|x| &*x == t);
    unsafe { !has(NSPasteboardTypeFileURL) && (has(NSPasteboardTypePNG) || has(NSPasteboardTypeTIFF)) }
}

fn pasteboard_rep(pb: &NSPasteboard) -> Option<Retained<NSBitmapImageRep>> {
    let data = unsafe { pb.dataForType(NSPasteboardTypePNG).or_else(|| pb.dataForType(NSPasteboardTypeTIFF)) }?;
    NSBitmapImageRep::imageRepWithData(&data)
}

/// The pasteboard picture as PNG bytes.
pub fn mac_png() -> Option<Vec<u8>> {
    png_of(&NSPasteboard::generalPasteboard())
}

fn png_of(pb: &NSPasteboard) -> Option<Vec<u8>> {
    autoreleasepool(|_| {
        if !has_image(pb) {
            return None;
        }
        if let Some(png) = unsafe { pb.dataForType(NSPasteboardTypePNG) } {
            return Some(png.to_vec());
        }
        let rep = pasteboard_rep(pb)?;
        let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
        Some(png.to_vec())
    })
}

/// The pasteboard picture as CF_DIB: drawn into a known 8-bit RGBA bitmap first, whatever its
/// own layout (16-bit, planar, CMYK...).
pub fn mac_dib() -> Option<Vec<u8>> {
    dib_of(&NSPasteboard::generalPasteboard())
}

fn dib_of(pb: &NSPasteboard) -> Option<Vec<u8>> {
    autoreleasepool(|_| {
        if !has_image(pb) {
            return None;
        }
        let src = pasteboard_rep(pb)?;
        let (w, h) = (src.pixelsWide() as usize, src.pixelsHigh() as usize);
        if w == 0 || h == 0 || w * h > MAX_PIXELS {
            return None;
        }
        let dst = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                w as isize,
                h as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                (w * 4) as isize,
                32,
            )
        }?;
        let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&dst)?;
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&ctx));
        src.drawInRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w as f64, h as f64)));
        ctx.flushGraphics();
        NSGraphicsContext::restoreGraphicsState_class();
        let stride = dst.bytesPerRow() as usize;
        let base = dst.bitmapData();
        if base.is_null() {
            return None;
        }
        let mut px = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w * 4) };
            // premultiplied in the bitmap: back to straight alpha
            for p in row.chunks_exact(4) {
                let a = p[3] as u32;
                let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
                px.extend_from_slice(&[un(p[0]), un(p[1]), un(p[2]), p[3]]);
            }
        }
        Some(to_dib(&Rgba { w, h, px }))
    })
}

/// Put PNG bytes (and the TIFF most Mac apps read) on the pasteboard. Returns its changeCount.
pub fn put_png(png: &[u8]) -> Option<isize> {
    put_png_on(&NSPasteboard::generalPasteboard(), png)
}

fn put_png_on(pb: &NSPasteboard, png: &[u8]) -> Option<isize> {
    autoreleasepool(|_| {
        let data = NSData::with_bytes(png);
        let tiff = NSBitmapImageRep::imageRepWithData(&data).and_then(|r| r.TIFFRepresentation());
        pb.clearContents();
        let ok = unsafe { pb.setData_forType(Some(&data), NSPasteboardTypePNG) };
        if let Some(t) = tiff {
            unsafe { pb.setData_forType(Some(&t), NSPasteboardTypeTIFF) };
        }
        ok.then(|| pb.changeCount())
    })
}

/// Put a client's DIB on the pasteboard (as PNG + TIFF).
pub fn put_dib(dib: &[u8]) -> Option<isize> {
    put_dib_on(&NSPasteboard::generalPasteboard(), dib)
}

fn put_dib_on(pb: &NSPasteboard, dib: &[u8]) -> Option<isize> {
    let img = from_dib(dib)?;
    let png = autoreleasepool(|_| {
        let rep = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                img.w as isize,
                img.h as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                NSBitmapFormat::AlphaNonpremultiplied,
                (img.w * 4) as isize,
                32,
            )
        }?;
        let base = rep.bitmapData();
        if base.is_null() {
            return None;
        }
        unsafe { std::ptr::copy_nonoverlapping(img.px.as_ptr(), base, img.px.len()) };
        let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
        Some(png.to_vec())
    })?;
    put_png_on(pb, &png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through AppKit for real, on a private pasteboard (never the user's clipboard): a DIB
    /// from the client lands as PNG + TIFF, and comes back out as the same pixels.
    #[test]
    fn pasteboard_round_trip() {
        let pb = NSPasteboard::pasteboardWithUniqueName();
        let (w, h) = (4usize, 3usize);
        let px: Vec<u8> = (0..w * h).flat_map(|i| [(i * 20) as u8, 100, (255 - i * 20) as u8, 255]).collect();
        let dib = to_dib(&Rgba { w, h, px: px.clone() });
        assert!(put_dib_on(&pb, &dib).is_some());
        assert!(has_image(&pb));
        let png = png_of(&pb).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let back = from_dib(&dib_of(&pb).unwrap()).unwrap();
        assert_eq!((back.w, back.h), (w, h));
        assert_eq!(back.px, px);
        let _: () = unsafe { objc2::msg_send![&*pb, releaseGlobally] };
    }

    #[test]
    fn dib_round_trip() {
        let img = Rgba { w: 3, h: 2, px: (0..24).map(|i| (i * 10) as u8).collect() };
        let back = from_dib(&to_dib(&img)).unwrap();
        assert_eq!((back.w, back.h), (3, 2));
        assert_eq!(back.px, img.px);
    }

    #[test]
    fn dib_24bit_bottom_up_padded() {
        // 1x2, 24-bit: each row 3 bytes + 1 padding; bottom row first
        let mut d = vec![0u8; 40];
        d[0] = 40;
        d[4] = 1;
        d[8] = 2;
        d[12] = 1;
        d[14] = 24;
        d.extend_from_slice(&[1, 2, 3, 0]); // bottom pixel: B G R
        d.extend_from_slice(&[4, 5, 6, 0]); // top pixel
        let img = from_dib(&d).unwrap();
        assert_eq!(img.px, vec![6, 5, 4, 255, 3, 2, 1, 255]);
    }

    #[test]
    fn dib_32bit_without_alpha_is_opaque() {
        let mut d = vec![0u8; 40];
        d[0] = 40;
        d[4] = 1;
        d[8] = 0xff; // height -1: top-down
        d[9] = 0xff;
        d[10] = 0xff;
        d[11] = 0xff;
        d[12] = 1;
        d[14] = 32;
        d.extend_from_slice(&[10, 20, 30, 0]);
        let img = from_dib(&d).unwrap();
        assert_eq!(img.px, vec![30, 20, 10, 255]);
    }
}
