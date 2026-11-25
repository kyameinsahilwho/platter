//! App icons for the Uninstall apps list: one icon out of an .exe/.dll/.ico, rendered to a small
//! PNG data URL.
//!
//! The icon is drawn twice with DrawIconEx, once over black and once over white, and the alpha is
//! recovered from the difference (alpha = 255 - (white - black)). That works the same for modern
//! 32-bit alpha icons and for old mask-based ones, which have no alpha channel at all; reading the
//! icon's colour bitmap directly gets the old ones wrong (a black box around the picture).
//! Loading is done with SHDefExtractIconW, which opens the file as data: nothing in it runs.

use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::Shell::SHDefExtractIconW;
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL, HICON};

/// Pixel size the icon is extracted and drawn at. The row shows it at 28 CSS px; 64 stays sharp at
/// 200 % display scaling, and a 64x64 PNG with stored deflate is ~16 KB.
pub const SIZE: u32 = 64;

/// The icon at `index` in `file` (DisplayIcon's "path,index": index >= 0 is a position, a negative
/// one a resource id) as a `data:image/png;base64,…` URL. None when the file has no usable icon.
pub fn data_url(file: &Path, index: i32) -> Option<String> {
    let (w, h, bgra) = render(file, index)?;
    // Fully transparent: nothing to show (some resource-only DLLs hand back an empty icon).
    if !bgra.chunks_exact(4).any(|p| p[3] > 8) {
        return None;
    }
    Some(crate::dupes::png_data_url_premultiplied(w, h, bgra, SIZE))
}

/// Premultiplied BGRA pixels of the icon, top-down.
fn render(file: &Path, index: i32) -> Option<(u32, u32, Vec<u8>)> {
    let path = crate::util::wide(&file.to_string_lossy());
    let mut large: HICON = std::ptr::null_mut();
    // nIconSize: LOWORD = large size, HIWORD = small size.
    let sizes = SIZE | (16 << 16);
    let hr = unsafe { SHDefExtractIconW(path.as_ptr(), index, 0, &mut large, std::ptr::null_mut(), sizes) };
    if hr != 0 || large.is_null() {
        return None;
    }
    let out = draw_on(large, 0x00).zip(draw_on(large, 0xFF)).map(|(black, white)| unmix(&black, &white));
    unsafe { DestroyIcon(large) };
    out.map(|px| (SIZE, SIZE, px))
}

/// The icon drawn over a solid background (every byte = `fill`).
fn draw_on(icon: HICON, fill: u8) -> Option<Vec<u8>> {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: SIZE as i32,
        biHeight: -(SIZE as i32), // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
    if dc.is_null() {
        return None;
    }
    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0) };
    if dib.is_null() || bits.is_null() {
        unsafe { DeleteDC(dc) };
        return None;
    }
    let len = (SIZE * SIZE * 4) as usize;
    let old = unsafe { SelectObject(dc, dib) };
    unsafe { std::ptr::write_bytes(bits as *mut u8, fill, len) };
    let ok = unsafe { DrawIconEx(dc, 0, 0, icon, SIZE as i32, SIZE as i32, 0, std::ptr::null_mut(), DI_NORMAL) } != 0;
    // GDI may batch drawing: flush before reading the DIB's memory.
    unsafe { windows_sys::Win32::Graphics::Gdi::GdiFlush() };
    let px = ok.then(|| unsafe { std::slice::from_raw_parts(bits as *const u8, len) }.to_vec());
    unsafe {
        SelectObject(dc, old);
        DeleteObject(dib);
        DeleteDC(dc);
    }
    px
}

/// Recovers premultiplied BGRA from the same picture composited over black and over white:
/// over black a pixel is c·a; over white it is c·a + 255·(1 − a), so a = 255 − (white − black)
/// and the black drawing already holds the premultiplied colour.
fn unmix(black: &[u8], white: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(black.len());
    for (b, w) in black.chunks_exact(4).zip(white.chunks_exact(4)) {
        // Average the three channels' estimates; anti-aliased edges differ by a level or two.
        let diff: u32 = (0..3).map(|c| (w[c] as i32 - b[c] as i32).clamp(0, 255) as u32).sum::<u32>() / 3;
        let a = 255 - diff.min(255);
        let clamp = |c: u8| (c as u32).min(a) as u8; // premultiplied colour can't exceed alpha
        out.extend_from_slice(&[clamp(b[0]), clamp(b[1]), clamp(b[2]), a as u8]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appicon_unmix_recovers_alpha() {
        // Opaque red, fully transparent, and 50 % white (premultiplied 128) as drawn over black/white.
        let black = [0, 0, 255, 0, 0, 0, 0, 0, 128, 128, 128, 0];
        let white = [0, 0, 255, 0, 255, 255, 255, 0, 255, 255, 255, 0];
        let px = unmix(&black, &white);
        assert_eq!(&px[0..4], &[0, 0, 255, 255]);
        assert_eq!(px[7], 0);
        assert!((px[11] as i32 - 128).abs() <= 1, "alpha {}", px[11]);
        assert_eq!(px[8], 128);
    }

    #[test]
    fn appicon_from_a_system_exe() {
        // notepad.exe ships an icon on every Windows; read-only.
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let url = data_url(&Path::new(&win).join(r"System32\notepad.exe"), 0).expect("notepad has an icon");
        assert!(url.starts_with("data:image/png;base64,iVBORw0KGgo"));
        assert!(data_url(Path::new(r"C:\definitely\not\here.exe"), 0).is_none());
    }
}
