//! The icon of the program an app-capture card captures, painted on the
//! card like a badge (spec: meter bridge §4.3). Looked up on a worker
//! thread; a drawn generic icon stands in when there is none.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ColorImage, Pos2, Rect, TextureHandle, TextureOptions, Vec2};

/// An icon as straight (not premultiplied) RGBA rows: (width, height, bytes).
pub type Rgba = (usize, usize, Vec<u8>);

/// The badge on `card`: its centre, side and rotation (radians). It sits on
/// the top-right corner with roughly 40 % of it over the card's edge.
pub fn badge_rect(card: Rect) -> (Pos2, f32, f32) {
    let side = 46.0;
    (Pos2::new(card.right() - 11.0, card.top() + 11.0), side, -12f32.to_radians())
}

/// A transparent border round an icon, so sampling outside it (the texture
/// clamps to its edge) is transparent.
pub fn pad(icon: &Rgba, border: usize) -> Rgba {
    let (w, h, px) = icon;
    let (pw, ph) = (w + 2 * border, h + 2 * border);
    let mut out = vec![0u8; pw * ph * 4];
    for y in 0..*h {
        let from = y * w * 4;
        let to = ((y + border) * pw + border) * 4;
        out[to..to + w * 4].copy_from_slice(&px[from..from + w * 4]);
    }
    (pw, ph, out)
}

/// A generic "program window" icon for apps whose own icon is not found.
pub fn fallback_icon() -> Rgba {
    let n = 32usize;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let inside = (3..29).contains(&x) && (5..27).contains(&y);
            let frame = inside && (x == 3 || x == 28 || y == 5 || y == 26 || (6..10).contains(&y));
            let i = (y * n + x) * 4;
            if frame {
                px[i..i + 4].copy_from_slice(&[0xf2, 0xf2, 0xf2, 0xff]);
            } else if inside {
                px[i..i + 4].copy_from_slice(&[0x80, 0x84, 0x8c, 0xb0]);
            }
        }
    }
    (n, n, px)
}

/// The large icon of the running program `process` ("discord.exe", "Discord"
/// or a PID), or `None`.
#[cfg(windows)]
pub fn icon_rgba(process: &str) -> Option<Rgba> {
    let path = process_path(process)?;
    win::icon_of_file(&path)
}

#[cfg(not(windows))]
pub fn icon_rgba(_process: &str) -> Option<Rgba> {
    None
}

/// The executable of the running process named `process` (case-insensitive,
/// ".exe" optional) or with that PID.
#[cfg(windows)]
fn process_path(process: &str) -> Option<String> {
    let want = process.trim().to_ascii_lowercase();
    let want_exe = if want.ends_with(".exe") { want.clone() } else { format!("{want}.exe") };
    let pid = want.parse::<u32>().ok().or_else(|| win::find_pid(&want_exe))?;
    win::image_path(pid)
}

#[cfg(windows)]
mod win {
    use super::Rgba;
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Shell::ExtractIconExW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a handle this module opened.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub fn find_pid(exe: &str) -> Option<u32> {
        // SAFETY: a process snapshot; closed by `Handle`.
        let snap = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        // SAFETY: `e` is sized for the call.
        let mut ok = unsafe { Process32FirstW(snap.0, &mut e) }.is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase();
            if name == exe {
                return Some(e.th32ProcessID);
            }
            // SAFETY: as above.
            ok = unsafe { Process32NextW(snap.0, &mut e) }.is_ok();
        }
        None
    }

    pub fn image_path(pid: u32) -> Option<String> {
        // SAFETY: query-only access; closed by `Handle`. Denied for some
        // protected processes: then there is simply no icon.
        let h = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` holds `len` characters.
        unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }

    pub fn icon_of_file(path: &str) -> Option<Rgba> {
        let mut large = HICON::default();
        // SAFETY: one large icon into `large`.
        let n = unsafe { ExtractIconExW(&HSTRING::from(path), 0, Some(&mut large), None, 1) };
        if n == 0 || large.is_invalid() {
            return None;
        }
        let rgba = pixels(large);
        // SAFETY: the icon ExtractIconExW gave us.
        unsafe {
            let _ = DestroyIcon(large);
        }
        rgba
    }

    /// An icon's pixels as straight RGBA (the colour bitmap's alpha, or the
    /// mask for icons without one).
    fn pixels(icon: HICON) -> Option<Rgba> {
        let mut info = ICONINFO::default();
        // SAFETY: `info` receives two bitmaps we delete below.
        unsafe { GetIconInfo(icon, &mut info) }.ok()?;
        let free = |info: &ICONINFO| {
            // SAFETY: bitmaps from GetIconInfo.
            unsafe {
                let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
                let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
            }
        };
        if info.hbmColor.is_invalid() {
            free(&info);
            return None;
        }
        let mut bm = BITMAP::default();
        // SAFETY: `bm` is the size GetObjectW is told.
        let got = unsafe {
            GetObjectW(
                HGDIOBJ(info.hbmColor.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut bm as *mut BITMAP).cast()),
            )
        };
        let (w, h) = (bm.bmWidth.max(0) as usize, bm.bmHeight.max(0) as usize);
        if got == 0 || w == 0 || h == 0 || w > 256 || h > 256 {
            free(&info);
            return None;
        }
        let read = |bitmap| -> Option<Vec<u8>> {
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut px = vec![0u8; w * h * 4];
            // SAFETY: a screen DC for the conversion; `px` holds w*h BGRA pixels.
            let lines = unsafe {
                let dc = GetDC(None);
                let n = GetDIBits(dc, bitmap, 0, h as u32, Some(px.as_mut_ptr().cast()), &mut bi, DIB_RGB_COLORS);
                ReleaseDC(None, dc);
                n
            };
            (lines == h as i32).then_some(px)
        };
        let colour = read(info.hbmColor);
        let mask = read(info.hbmMask);
        free(&info);
        let mut px = colour?;
        let has_alpha = px.chunks(4).any(|p| p[3] != 0);
        for (i, p) in px.chunks_mut(4).enumerate() {
            p.swap(0, 2); // BGRA to RGBA
            if !has_alpha {
                let opaque = mask.as_ref().is_none_or(|m| m[i * 4] == 0);
                p[3] = if opaque { 255 } else { 0 };
            }
        }
        Some((w, h, px))
    }
}

/// What a lookup came to.
enum Entry {
    Pending(Receiver<Option<Rgba>>),
    Ready(TextureHandle),
    /// Not found: the fallback is shown, and it is asked again after a while.
    Missing(Instant, TextureHandle),
}

/// Icons by process name, looked up off the UI thread.
#[derive(Default)]
pub struct IconCache {
    entries: HashMap<String, Entry>,
}

/// How long a program whose icon was not found waits before it is asked again.
const RETRY: Duration = Duration::from_secs(10);

fn texture(ctx: &egui::Context, name: &str, icon: &Rgba) -> TextureHandle {
    let (w, h, px) = pad(icon, 2);
    let image = ColorImage::from_rgba_unmultiplied([w, h], &px);
    ctx.load_texture(format!("app-icon-{name}"), image, TextureOptions::LINEAR)
}

impl IconCache {
    /// The icon for `process` (the fallback while looking, or if none).
    pub fn get(&mut self, ctx: &egui::Context, process: &str) -> TextureHandle {
        let key = process.trim().to_ascii_lowercase();
        let fallback = || texture(ctx, "fallback", &fallback_icon());
        let next = match self.entries.remove(&key) {
            None => {
                let (tx, rx) = channel();
                let name = key.clone();
                let _ = std::thread::Builder::new().name("confluence-app-icon".into()).spawn(move || {
                    let _ = tx.send(icon_rgba(&name));
                });
                ctx.request_repaint_after(Duration::from_millis(100));
                Entry::Pending(rx)
            }
            Some(Entry::Pending(rx)) => match rx.try_recv() {
                Ok(Some(icon)) => Entry::Ready(texture(ctx, &key, &icon)),
                Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Entry::Missing(Instant::now(), fallback())
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                    Entry::Pending(rx)
                }
            },
            Some(Entry::Missing(at, _)) if at.elapsed() > RETRY => {
                self.entries.remove(&key);
                return self.get(ctx, process);
            }
            Some(other) => other,
        };
        let tex = match &next {
            Entry::Ready(t) | Entry::Missing(_, t) => t.clone(),
            Entry::Pending(_) => fallback(),
        };
        self.entries.insert(key, next);
        tex
    }
}

/// Where badge pixel `q` samples the icon texture (0..1 inside the badge).
pub fn badge_uv(q: Pos2, center: Pos2, side: f32, angle: f32) -> Pos2 {
    let d = q - center;
    let (s, c) = (-angle).sin_cos();
    let r = Vec2::new(d.x * c - d.y * s, d.x * s + d.y * c);
    Pos2::new(r.x / side + 0.5, r.y / side + 0.5)
}

/// The tint the badge is painted with: 85 % opacity.
pub const BADGE_TINT: Color32 = Color32::from_rgba_premultiplied(217, 217, 217, 217);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_badge_hangs_over_the_top_right_corner() {
        let card = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(240.0, 150.0));
        let (c, side, angle) = badge_rect(card);
        assert!(card.contains(c) && c.x > card.center().x && c.y < card.center().y, "centre inside, top right");
        // Sample the badge: about 40 % of it is outside the card.
        let (mut inside, mut total) = (0, 0);
        for i in 0..40 {
            for j in 0..40 {
                let q = c + Vec2::new((i as f32 / 39.0 - 0.5) * side, (j as f32 / 39.0 - 0.5) * side);
                let uv = badge_uv(q, c, side, angle);
                if (0.0..=1.0).contains(&uv.x) && (0.0..=1.0).contains(&uv.y) {
                    total += 1;
                    inside += usize::from(card.contains(q));
                }
            }
        }
        let outside = 1.0 - inside as f32 / total as f32;
        assert!((0.3..0.6).contains(&outside), "{outside}");
    }

    #[test]
    fn padding_adds_a_transparent_border_and_keeps_the_pixels() {
        let icon = (2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let (w, h, px) = pad(&icon, 1);
        assert_eq!((w, h), (4, 3));
        assert_eq!(&px[(4 + 1) * 4..(4 + 1) * 4 + 8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(px[..16].iter().all(|b| *b == 0), "the top row is transparent");
        let (fw, fh, fpx) = fallback_icon();
        assert_eq!((fw, fh, fpx.len()), (32, 32, 32 * 32 * 4));
    }

    #[test]
    fn a_missing_program_has_no_icon_quickly() {
        let t = Instant::now();
        assert!(icon_rgba("no-such-process-xyz-123").is_none());
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[cfg(windows)]
    #[test]
    fn a_running_programs_icon_is_found() {
        match icon_rgba("explorer.exe") {
            Some((w, h, px)) => {
                assert!(w >= 16 && w == h, "{w}x{h}");
                assert_eq!(px.len(), w * h * 4);
                assert!(px.chunks(4).any(|p| p[3] > 0), "some pixels are opaque");
            }
            None => eprintln!("explorer.exe is not running here (a service session?): skipped"),
        }
    }
}
