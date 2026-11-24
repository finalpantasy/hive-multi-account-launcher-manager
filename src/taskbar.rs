//! Separate taskbar buttons. Every account's window is the same Claude app, so
//! Windows stacks them all under one button. Giving each window its own app id
//! splits them into one button per account. Each keeps Claude's own icon with a
//! round badge in the account's tag color holding its initials. Undone when
//! turned off or when this app exits.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GdiFlush,
    GetDC, GetDIBits, GetObjectW, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DT_CENTER,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, TRANSPARENT,
};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Shell::PropertiesSystem::{SHGetPropertyStoreForWindow, PROPERTYKEY};
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Shell::{SHDefExtractIconW, Shell_NotifyIconW, NIF_ICON, NIM_MODIFY, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, FindWindowExW, GetClassLongPtrW, GetIconInfo, GetWindowThreadProcessId, SendMessageTimeoutW,
    GCLP_HICON, HICON, ICONINFO, ICON_BIG, ICON_SMALL, SMTO_ABORTIFHUNG, SM_CXICON, SM_CXSMICON, WM_GETICON,
    WM_SETICON,
};

use crate::win::wide;

const IID_IPROPERTYSTORE: GUID = GUID::from_u128(0x886d8eeb_8cf2_4446_8d02_cdba1dbdcf99);
/// PKEY_AppUserModel_* share this format id.
const FMTID_APPUSERMODEL: GUID = GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3);
const PID_RELAUNCH_COMMAND: u32 = 2;
const PID_RELAUNCH_ICON: u32 = 3;
const PID_RELAUNCH_NAME: u32 = 4;
const PID_ID: u32 = 5;
const VT_EMPTY: u16 = 0;
const VT_LPWSTR: u16 = 31;

/// PROPVARIANT holding a string pointer (or nothing).
#[repr(C)]
struct PropVar {
    vt: u16,
    reserved: [u16; 3],
    ptr: *const u16,
    pad: usize,
}

/// IPropertyStore's vtable (IUnknown first).
#[repr(C)]
struct StoreVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_count: usize,
    get_at: usize,
    get_value: unsafe extern "system" fn(*mut c_void, *const PROPERTYKEY, *mut PropVar) -> i32,
    set_value: unsafe extern "system" fn(*mut c_void, *const PROPERTYKEY, *const PropVar) -> i32,
    commit: unsafe extern "system" fn(*mut c_void) -> i32,
}

/// Set (Some) or clear (None) app-model properties on a window, in order.
fn set_props(hwnd: HWND, props: &[(u32, Option<String>)]) -> bool {
    unsafe {
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32); // once per thread; repeats are harmless
        let mut store: *mut c_void = null_mut();
        if SHGetPropertyStoreForWindow(hwnd, &IID_IPROPERTYSTORE, &mut store) < 0 || store.is_null() {
            return false;
        }
        let vt = &**(store as *const *const StoreVtbl);
        let mut ok = true;
        for (pid, value) in props {
            let text = value.as_deref().map(wide);
            let pv = match &text {
                Some(w) => PropVar { vt: VT_LPWSTR, reserved: [0; 3], ptr: w.as_ptr(), pad: 0 },
                None => PropVar { vt: VT_EMPTY, reserved: [0; 3], ptr: null(), pad: 0 },
            };
            let key = PROPERTYKEY { fmtid: FMTID_APPUSERMODEL, pid: *pid };
            ok &= (vt.set_value)(store, &key, &pv) >= 0;
        }
        (vt.commit)(store);
        (vt.release)(store);
        ok
    }
}

/// Hive's own taskbar id. Without one Windows matches Hive's windows to any saved
/// shortcut that runs this exe, such as the "--open Other" one it keeps for a pinned
/// account button, and shows that account's badge icon on Hive's button. Must not
/// start with "ClaudeAccounts." (that prefix marks Claude windows; see clear_stale).
const OWN_APP_ID: &str = "Hive.App";

/// Give one of Hive's own windows its own taskbar button with Hive's icon.
pub fn own_window(hwnd: HWND) {
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.display().to_string();
    set_props(
        hwnd,
        &[
            (PID_RELAUNCH_COMMAND, Some(format!("\"{exe}\""))),
            (PID_RELAUNCH_NAME, Some(crate::APP_NAME.to_string())),
            (PID_RELAUNCH_ICON, Some(format!("{exe},0"))),
            (PID_ID, Some(OWN_APP_ID.to_string())),
        ],
    );
}

/// An account name as part of an app id: letters and digits kept, anything else
/// written as _hex, so "Team 1" and "Team1" (or non-English names) never collide.
fn app_id_part(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_string() } else { format!("_{:x}", c as u32) }).collect()
}

/// A window still carrying one of our app ids (left by a tray that crashed or was
/// killed): give it back to Claude's own taskbar button and icon.
pub fn clear_stale(hwnd: HWND) {
    unsafe {
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let mut store: *mut c_void = null_mut();
        if SHGetPropertyStoreForWindow(hwnd, &IID_IPROPERTYSTORE, &mut store) < 0 || store.is_null() {
            return;
        }
        let vt = &**(store as *const *const StoreVtbl);
        let key = PROPERTYKEY { fmtid: FMTID_APPUSERMODEL, pid: PID_ID };
        let mut pv = PropVar { vt: VT_EMPTY, reserved: [0; 3], ptr: null(), pad: 0 };
        let ours = (vt.get_value)(store, &key, &mut pv) >= 0 && pv.vt == VT_LPWSTR && !pv.ptr.is_null() && {
            let n = (0..).take_while(|&i| *pv.ptr.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(pv.ptr, n)).starts_with("ClaudeAccounts.")
        };
        if pv.vt == VT_LPWSTR && !pv.ptr.is_null() {
            CoTaskMemFree(pv.ptr as *const c_void);
        }
        (vt.release)(store);
        if ours {
            set_props(hwnd, &[(PID_ID, None), (PID_RELAUNCH_COMMAND, None), (PID_RELAUNCH_ICON, None), (PID_RELAUNCH_NAME, None)]);
            send_icon(hwnd, ICON_BIG, null_mut());
            send_icon(hwnd, ICON_SMALL, null_mut());
        }
    }
}

/// "Team" -> "T", "Work account 2" -> "W2".
pub(crate) fn initials(name: &str) -> String {
    let first = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    match name.trim_end().chars().last() {
        Some(d) if d.is_ascii_digit() && name.trim().len() > 1 => format!("{first}{d}"),
        _ => first,
    }
}

fn send_icon(hwnd: HWND, which: u32, icon: HICON) -> HICON {
    let mut old = 0usize;
    unsafe { SendMessageTimeoutW(hwnd, WM_SETICON, which as usize, icon as isize, SMTO_ABORTIFHUNG, 500, &mut old) };
    old as HICON
}

fn get_icon(hwnd: HWND, which: u32) -> HICON {
    let mut cur = 0usize;
    unsafe { SendMessageTimeoutW(hwnd, WM_GETICON, which as usize, 0, SMTO_ABORTIFHUNG, 500, &mut cur) };
    cur as HICON
}

/// Coverage (0..1) of white text drawn centered in `r` on a size x size canvas.
fn text_mask(text: &str, size: i32, r: RECT, px_h: f32) -> Vec<f32> {
    let mut out = vec![0f32; (size * size) as usize];
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut c_void = null_mut();
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(dc);
            return out;
        }
        let old = SelectObject(dc, dib as _);
        let face = wide("Segoe UI");
        let font = CreateFontW(-(px_h.round() as i32).max(6), 0, 0, 0, 800, 0, 0, 0, 1, 0, 0, 4, 0, face.as_ptr());
        let old_font = SelectObject(dc, font as _);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        let w = wide(text);
        let mut rr = r;
        DrawTextW(dc, w.as_ptr(), (w.len() - 1) as i32, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        GdiFlush();
        let px = std::slice::from_raw_parts(bits as *const u32, (size * size) as usize);
        for (o, p) in out.iter_mut().zip(px) {
            *o = (p & 0xFF) as f32 / 255.0;
        }
        SelectObject(dc, old_font);
        SelectObject(dc, old);
        DeleteObject(font as _);
        DeleteObject(dib as _);
        DeleteDC(dc);
    }
    out
}

/// An icon's pixels (straight-alpha BGRA, top-down) and its width.
fn icon_pixels(icon: HICON) -> Option<(i32, Vec<u32>)> {
    unsafe {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut ii) == 0 {
            return None;
        }
        let mut bm: BITMAP = std::mem::zeroed();
        let ok = !ii.hbmColor.is_null() && GetObjectW(ii.hbmColor as _, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void) != 0;
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut px = vec![0u32; (w.max(0) * h.max(0)) as usize];
        if ok && w > 0 && h > 0 {
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let dc = GetDC(null_mut());
            GetDIBits(dc, ii.hbmColor, 0, h as u32, px.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);
            ReleaseDC(null_mut(), dc);
        }
        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor as _);
        }
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask as _);
        }
        if !ok || w <= 0 || w != h {
            return None;
        }
        if px.iter().all(|p| p >> 24 == 0) {
            for p in &mut px {
                *p |= 0xFF00_0000; // an old icon without alpha
            }
        }
        Some((w, px))
    }
}

/// Shrink a square image to `size` by averaging (alpha-weighted, so edges stay clean).
fn resample(src: &[u32], sw: i32, size: i32) -> Vec<u32> {
    let mut out = vec![0u32; (size * size) as usize];
    let scale = sw as f32 / size as f32;
    for y in 0..size {
        for x in 0..size {
            let (x0, y0) = ((x as f32 * scale) as i32, (y as f32 * scale) as i32);
            let (x1, y1) = ((((x + 1) as f32 * scale).ceil() as i32).min(sw), (((y + 1) as f32 * scale).ceil() as i32).min(sw));
            let (mut a, mut r, mut g, mut b, mut n) = (0f32, 0f32, 0f32, 0f32, 0f32);
            for sy in y0..y1.max(y0 + 1) {
                for sx in x0..x1.max(x0 + 1) {
                    let p = src[(sy.min(sw - 1) * sw + sx.min(sw - 1)) as usize];
                    let pa = (p >> 24) as f32;
                    a += pa;
                    r += ((p >> 16) & 255) as f32 * pa;
                    g += ((p >> 8) & 255) as f32 * pa;
                    b += (p & 255) as f32 * pa;
                    n += 1.0;
                }
            }
            if a > 0.0 {
                let c = |v: f32| (v / a).round().clamp(0.0, 255.0) as u32;
                out[(y * size + x) as usize] = ((a / n).round() as u32) << 24 | c(r) << 16 | c(g) << 8 | c(b);
            }
        }
    }
    out
}

/// Claude's icon (when there is one) with a round badge in the account's color
/// holding its initials. Without a base icon the badge fills the whole square.
pub(crate) fn compose(base: Option<&(i32, Vec<u32>)>, size: i32, text: &str, rgb: u32) -> Vec<u32> {
    let mut px = match base {
        Some((w, src)) => resample(src, *w, size),
        None => vec![0u32; (size * size) as usize],
    };
    let s = size as f32;
    // A large badge in the lower right: readable at 16 px, Claude's mark still shows.
    let (cx, cy, rad) = if base.is_some() { (s * 0.66, s * 0.66, s * 0.34) } else { (s / 2.0, s / 2.0, s / 2.0) };
    let ring = if base.is_some() && size >= 24 { (s * 0.045).max(1.0) } else { 0.0 };
    let two = text.chars().count() > 1;
    let px_h = rad * 2.0 * if two { 0.52 } else { 0.68 };
    let box_ = RECT { left: (cx - rad) as i32, top: (cy - rad) as i32, right: (cx + rad).ceil() as i32, bottom: (cy + rad).ceil() as i32 };
    let mask = text_mask(text, size, box_, px_h);
    let (mut br, mut bg, mut bb) = (((rgb >> 16) & 255) as f32, ((rgb >> 8) & 255) as f32, (rgb & 255) as f32);
    // A badge close to Claude's own orange would vanish into the icon: use a deep shade of it.
    let near = (br - 217.0).abs() + (bg - 119.0).abs() + (bb - 87.0).abs() < 90.0;
    if base.is_some() && near {
        (br, bg, bb) = (br * 0.45, bg * 0.45, bb * 0.45);
    }
    for y in 0..size {
        for x in 0..size {
            let i = (y * size + x) as usize;
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let outer = (rad + ring - d + 0.5).clamp(0.0, 1.0); // badge plus its white ring
            if outer <= 0.0 {
                continue;
            }
            let inner = (rad - d + 0.5).clamp(0.0, 1.0);
            let l = if inner > 0.0 { mask[i] } else { 0.0 };
            // Badge color, white letters, white ring outside the badge.
            let mix = |base: f32| base + (255.0 - base) * l;
            let (mut r, mut g, mut b) = (mix(br) * inner + 255.0 * (1.0 - inner), mix(bg) * inner + 255.0 * (1.0 - inner), mix(bb) * inner + 255.0 * (1.0 - inner));
            // Blend over what is underneath.
            let p = px[i];
            let (ua, ur, ug, ub) = ((p >> 24) as f32 / 255.0, ((p >> 16) & 255) as f32, ((p >> 8) & 255) as f32, (p & 255) as f32);
            let a = outer + ua * (1.0 - outer);
            if a > 0.0 {
                r = (r * outer + ur * ua * (1.0 - outer)) / a;
                g = (g * outer + ug * ua * (1.0 - outer)) / a;
                b = (b * outer + ub * ua * (1.0 - outer)) / a;
            }
            px[i] = ((a * 255.0).round() as u32) << 24 | (r.round() as u32) << 16 | (g.round() as u32) << 8 | b.round() as u32;
        }
    }
    px
}

fn hicon_from(px: &[u32], size: i32) -> HICON {
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut c_void = null_mut();
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(dc);
            return null_mut();
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        let mask = CreateBitmap(size, size, 1, 1, null());
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: dib };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(mask as _);
        DeleteObject(dib as _);
        DeleteDC(dc);
        icon
    }
}

/// A .ico file holding the images (32-bit, with alpha).
fn ico_bytes(images: &[(i32, Vec<u32>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut bodies = Vec::new();
    for (size, px) in images {
        let n = *size as usize;
        let mask_row = n.div_ceil(32) * 4;
        let mut b = Vec::with_capacity(40 + n * n * 4 + mask_row * n);
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&(*size).to_le_bytes());
        b.extend_from_slice(&(size * 2).to_le_bytes()); // color + mask
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(&[0u8; 24]);
        for y in (0..n).rev() {
            for x in 0..n {
                b.extend_from_slice(&px[y * n + x].to_le_bytes());
            }
        }
        b.extend(std::iter::repeat(0u8).take(mask_row * n));
        bodies.push(b);
    }
    let mut offset = 6 + 16 * images.len() as u32;
    for ((size, _), b) in images.iter().zip(&bodies) {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += b.len() as u32;
    }
    for b in bodies {
        out.extend_from_slice(&b);
    }
    out
}

/// Claude's own icon, large, from the program that owns the window.
fn claude_icon(hwnd: HWND) -> Option<(i32, Vec<u32>)> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        let mut path = None;
        if !h.is_null() {
            let mut buf = vec![0u16; 1024];
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0 {
                buf.truncate(len as usize);
                buf.push(0);
                path = Some(buf);
            }
            CloseHandle(h);
        }
        let mut big: HICON = null_mut();
        if let Some(p) = &path {
            SHDefExtractIconW(p.as_ptr(), 0, 0, &mut big, null_mut(), 256);
        }
        let px = if big.is_null() {
            // Fall back to the window class's icon.
            let c = GetClassLongPtrW(hwnd, GCLP_HICON) as HICON;
            if c.is_null() { None } else { icon_pixels(c) }
        } else {
            let px = icon_pixels(big);
            DestroyIcon(big);
            px
        };
        px
    }
}

/// The app's icon in the notification area: its hidden Electron host window and icon id.
/// Claude's host is "Electron_..."; the Codex app's (ChatGPT.exe) is "OwlElectron_...".
pub fn tray_slot(pid: u32) -> Option<(HWND, u32)> {
    let host = ["Electron_NotifyIconHostWindow", "OwlElectron_NotifyIconHostWindow"].iter().find_map(|c| {
        let class = wide(c);
        let mut h: HWND = null_mut();
        unsafe {
            loop {
                h = FindWindowExW(null_mut(), h, class.as_ptr(), null());
                if h.is_null() {
                    return None;
                }
                let mut p = 0u32;
                GetWindowThreadProcessId(h, &mut p);
                if p == pid {
                    return Some(h);
                }
            }
        }
    })?;
    // Electron numbers its icons from 1. A change of nothing finds the live one.
    (1..=32).find(|&id| tray_set(host, id, null_mut())).map(|id| (host, id))
}

/// Swap the icon of another program's notification-area icon (null: change nothing).
fn tray_set(host: HWND, id: u32, icon: HICON) -> bool {
    unsafe {
        let mut n: NOTIFYICONDATAW = std::mem::zeroed();
        n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        n.hWnd = host;
        n.uID = id;
        if !icon.is_null() {
            n.uFlags = NIF_ICON;
            n.hIcon = icon;
        }
        Shell_NotifyIconW(NIM_MODIFY, &n) != 0
    }
}

/// One window given its own taskbar button. Dropping it puts the window back.
pub struct Marked {
    pub hwnd: HWND,
    pid: u32,
    /// Claude's notification-area icon, badged like the taskbar button.
    tray: Option<(HWND, u32)>,
    tray_icon: HICON,
    tray_old: HICON,
    big: HICON,
    small: HICON,
    /// Claude's own icons, restored on undo.
    old_big: HICON,
    old_small: HICON,
}

/// Bump when the icon drawing changes: a new id makes the taskbar drop its cached icon.
const ICON_STYLE: &str = "b2";

/// Where the account icons are saved (the taskbar and pinned buttons read them from disk).
fn icon_dir() -> std::path::PathBuf {
    crate::exe_dir().join("icons")
}

/// Give an account's window its own taskbar button, with Claude's icon and a
/// letter badge in the account's color.
pub fn mark(hwnd: HWND, account: &str, rgb: u32) -> Option<Marked> {
    let id = app_id_part(account);
    let exe = std::env::current_exe().ok()?.display().to_string();
    let text = initials(account);
    let base = claude_icon(hwnd);
    // The icon file the taskbar shows (its own copy beats the window's icon).
    let images: Vec<(i32, Vec<u32>)> = [16, 20, 24, 32, 40, 48, 64, 256].iter().map(|&s| (s, compose(base.as_ref(), s, &text, rgb))).collect();
    let file = icon_dir().join(format!("{id}-{rgb:06x}-{ICON_STYLE}.ico"));
    let _ = std::fs::create_dir_all(icon_dir());
    let icon_ok = std::fs::write(&file, ico_bytes(&images)).is_ok();
    let mut props = vec![
        // Pinning one of these buttons opens that account through this app.
        (PID_RELAUNCH_COMMAND, Some(format!("\"{exe}\" --open \"{account}\""))),
        (PID_RELAUNCH_NAME, Some(account.to_string())),
    ];
    if icon_ok {
        props.push((PID_RELAUNCH_ICON, Some(format!("{},0", file.display()))));
    }
    // The color is part of the id, so a new icon gets a fresh button (the taskbar caches icons per id).
    props.push((PID_ID, Some(format!("ClaudeAccounts.{id}.{rgb:06x}.{ICON_STYLE}"))));
    if !set_props(hwnd, &props) {
        return None;
    }
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        d => d,
    };
    let (bs, ss) = unsafe { (GetSystemMetricsForDpi(SM_CXICON, dpi).max(32), GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16)) };
    let big = hicon_from(&compose(base.as_ref(), bs, &text, rgb), bs);
    let small = hicon_from(&compose(base.as_ref(), ss, &text, rgb), ss);
    let (old_big, old_small) = (get_icon(hwnd, ICON_BIG), get_icon(hwnd, ICON_SMALL));
    if !big.is_null() {
        send_icon(hwnd, ICON_BIG, big);
    }
    if !small.is_null() {
        send_icon(hwnd, ICON_SMALL, small);
    }
    // The same badge on Claude's icon in the notification area (sized for the system DPI it uses).
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let ts = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()).max(16) };
    let tray_icon = hicon_from(&compose(base.as_ref(), ts, &text, rgb), ts);
    let tray_old = base.as_ref().map_or(null_mut(), |(s, px)| hicon_from(&resample(px, *s, ts), ts));
    let mut m = Marked { hwnd, pid, tray: None, tray_icon, tray_old, big, small, old_big, old_small };
    m.refresh_tray();
    Some(m)
}

impl Marked {
    /// Put the badge (back) on Claude's notification-area icon. Claude or a restarted
    /// Explorer can replace it, so this runs again now and then.
    pub fn refresh_tray(&mut self) {
        if self.tray_icon.is_null() {
            return;
        }
        if let Some((h, id)) = self.tray {
            if tray_set(h, id, self.tray_icon) {
                return;
            }
        }
        self.tray = tray_slot(self.pid);
        if let Some((h, id)) = self.tray {
            let ok = tray_set(h, id, self.tray_icon);
            crate::log(&format!("tray badge for process {}: icon {id} {}", self.pid, if ok { "set" } else { "refused" }));
        }
    }

    /// Put the window back under Claude's own button with its own icon.
    pub fn undo(self) {
        if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(self.hwnd) } != 0 {
            set_props(self.hwnd, &[(PID_ID, None), (PID_RELAUNCH_COMMAND, None), (PID_RELAUNCH_ICON, None), (PID_RELAUNCH_NAME, None)]);
            send_icon(self.hwnd, ICON_BIG, self.old_big);
            send_icon(self.hwnd, ICON_SMALL, self.old_small);
        }
        if let (Some((h, id)), false) = (self.tray, self.tray_old.is_null()) {
            tray_set(h, id, self.tray_old);
        }
        self.forget();
    }

    /// The window is gone: just free our icons.
    pub fn forget(self) {
        unsafe {
            if !self.big.is_null() {
                DestroyIcon(self.big);
            }
            if !self.small.is_null() {
                DestroyIcon(self.small);
            }
            for i in [self.tray_icon, self.tray_old] {
                if !i.is_null() {
                    DestroyIcon(i); // the shell keeps its own copy
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_read_well() {
        assert_eq!(initials("Team"), "T");
        assert_eq!(initials("Work account 2"), "W2");
        assert_eq!(initials("other"), "O");
        assert_eq!(initials("7"), "7");
    }

    #[test]
    fn app_ids_dont_collide() {
        assert_ne!(app_id_part("Team 1"), app_id_part("Team1"));
        assert_eq!(app_id_part("Team"), "Team");
        assert!(!app_id_part("\u{00C9}quipe").is_empty());
    }
}
