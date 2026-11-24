//! Saved window layouts: remember which monitor and where each account's Claude
//! window sits, and put them back (after a restart, a dock change, or when the
//! windows got shuffled).

use std::collections::BTreeMap;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, GetWindowRect, IsHungAppWindow, IsIconic, IsWindowVisible, IsZoomed, SetWindowPos, ShowWindow,
    SWP_NOACTIVATE, SWP_NOZORDER, SW_MAXIMIZE, SW_RESTORE, WINDOWPLACEMENT,
};

use crate::config::{Config, Layout, Slot};
use crate::tags;
use crate::win::from_wide;

struct Mon {
    name: String,
    rect: RECT,
    work: RECT,
    primary: bool,
}

unsafe extern "system" fn mon_cb(m: HMONITOR, _dc: HDC, _r: *mut RECT, lp: LPARAM) -> i32 {
    let out = &mut *(lp as *mut Vec<Mon>);
    if let Some(mon) = mon_info(m) {
        out.push(mon);
    }
    1
}

fn mon_info(m: HMONITOR) -> Option<Mon> {
    let mut mi: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe { GetMonitorInfoW(m, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) } != 0;
    ok.then(|| Mon {
        name: from_wide(&mi.szDevice),
        rect: mi.monitorInfo.rcMonitor,
        work: mi.monitorInfo.rcWork,
        primary: mi.monitorInfo.dwFlags & 1 != 0,
    })
}

fn monitors() -> Vec<Mon> {
    let mut v: Vec<Mon> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), std::ptr::null(), Some(mon_cb), &mut v as *mut Vec<Mon> as LPARAM) };
    v
}

// ---- Where alert cards sit: which monitor, which corner (pure math, unit tested) ----

/// A monitor as the card code needs it. Rectangles are [left, top, right, bottom].
#[derive(Clone, Debug, PartialEq)]
pub struct MonInfo {
    pub name: String,
    pub rect: [i32; 4],
    pub work: [i32; 4],
    pub primary: bool,
    pub dpi: u32,
}

unsafe extern "system" fn handle_cb(m: HMONITOR, _dc: HDC, _r: *mut RECT, lp: LPARAM) -> i32 {
    let out = &mut *(lp as *mut Vec<HMONITOR>);
    out.push(m);
    1
}

/// Every monitor, left to right (then top to bottom).
pub fn card_monitors() -> Vec<MonInfo> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), std::ptr::null(), Some(handle_cb), &mut handles as *mut Vec<HMONITOR> as LPARAM) };
    let mut out: Vec<MonInfo> = Vec::new();
    for h in handles {
        if let Some(m) = mon_info(h) {
            let (mut dx, mut dy) = (96u32, 96u32);
            unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForMonitor(h, windows_sys::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            let work = crate::win::usable_area(m.work);
            out.push(MonInfo {
                name: m.name,
                rect: [m.rect.left, m.rect.top, m.rect.right, m.rect.bottom],
                work: [work.left, work.top, work.right, work.bottom],
                primary: m.primary,
                dpi: dx.max(96),
            });
        }
    }
    sort_left_to_right(&mut out);
    out
}

pub fn sort_left_to_right(v: &mut [MonInfo]) {
    v.sort_by_key(|m| (m.rect[0], m.rect[1]));
}

/// Which monitor (index into the left-to-right list) cards go on: the saved device name,
/// else the saved left-to-right index, else the primary (also when nothing was saved).
pub fn choose_monitor(mons: &[MonInfo], name: &str, index: Option<usize>) -> Option<usize> {
    let primary = || mons.iter().position(|m| m.primary).or(if mons.is_empty() { None } else { Some(0) });
    if name.is_empty() {
        return primary();
    }
    if let Some(i) = mons.iter().position(|m| m.name == name) {
        return Some(i);
    }
    match index {
        Some(i) if i < mons.len() => Some(i),
        _ => primary(),
    }
}

/// One label per monitor, left to right. Three monitors with the primary in the middle:
/// Left, Main, Right. Anything else: "Monitor 1".."Monitor n", the primary marked "(main)".
pub fn monitor_labels(mons: &[MonInfo]) -> Vec<String> {
    if mons.len() == 3 && mons[1].primary {
        return vec!["Left".into(), "Main".into(), "Right".into()];
    }
    mons.iter().enumerate().map(|(i, m)| if m.primary { format!("Monitor {} (main)", i + 1) } else { format!("Monitor {}", i + 1) }).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// (label, setting, corner)
pub const CORNERS: [(&str, &str, Corner); 4] = [
    ("Top left", "top_left", Corner::TopLeft),
    ("Top right", "top_right", Corner::TopRight),
    ("Bottom left", "bottom_left", Corner::BottomLeft),
    ("Bottom right", "bottom_right", Corner::BottomRight),
];

impl Corner {
    /// From the saved setting; anything unknown is the bottom right (the old behavior).
    pub fn parse(s: &str) -> Corner {
        CORNERS.iter().find(|c| c.1 == s).map(|c| c.2).unwrap_or(Corner::BottomRight)
    }
    pub fn top(self) -> bool {
        matches!(self, Corner::TopLeft | Corner::TopRight)
    }
    pub fn left(self) -> bool {
        matches!(self, Corner::TopLeft | Corner::BottomLeft)
    }
}

/// Card rectangles ([l, t, r, b]) for a stack, oldest first. The newest card sits in the corner;
/// older ones stack away from it: upward from a bottom corner, downward from a top corner.
pub fn stack_rects(work: [i32; 4], corner: Corner, heights: &[i32], w: i32, margin: i32, gap: i32) -> Vec<[i32; 4]> {
    let x = if corner.left() { work[0] + margin } else { work[2] - margin - w };
    let mut out = vec![[0; 4]; heights.len()];
    if corner.top() {
        let mut y = work[1] + margin;
        for (i, &h) in heights.iter().enumerate().rev() {
            out[i] = [x, y, x + w, y + h];
            y += h + gap;
        }
    } else {
        let mut bottom = work[3] - margin;
        for (i, &h) in heights.iter().enumerate().rev() {
            out[i] = [x, bottom - h, x + w, bottom];
            bottom -= h + gap;
        }
    }
    out
}

/// "\\.\DISPLAY2" -> "monitor 2"
pub fn monitor_label(device: &str) -> String {
    let n: String = device.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    if n.is_empty() { "a monitor".into() } else { format!("monitor {n}") }
}

/// What to remember for one window, or None if it isn't on screen.
fn slot_of(hwnd: HWND) -> Option<Slot> {
    unsafe {
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return None;
        }
        let mon = mon_info(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST))?;
        let maximized = IsZoomed(hwnd) != 0;
        let (x, y, w, h);
        if maximized {
            // Size it comes back to when un-maximized (the position is chosen on restore).
            let mut p: WINDOWPLACEMENT = std::mem::zeroed();
            p.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            GetWindowPlacement(hwnd, &mut p);
            (x, y, w, h) = (0, 0, p.rcNormalPosition.right - p.rcNormalPosition.left, p.rcNormalPosition.bottom - p.rcNormalPosition.top);
        } else {
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut r);
            (x, y, w, h) = (r.left - mon.rect.left, r.top - mon.rect.top, r.right - r.left, r.bottom - r.top);
        }
        Some(Slot {
            monitor: mon.name,
            monitor_rect: [mon.rect.left, mon.rect.top, mon.rect.right, mon.rect.bottom],
            maximized,
            x,
            y,
            w,
            h,
        })
    }
}

/// Remember every open account window. Returns the layout and the accounts that
/// were left out (open but hidden or minimized).
pub fn save(cfg: &Config) -> (Layout, Vec<String>, Vec<String>) {
    let mut slots = BTreeMap::new();
    let mut left_out = Vec::new();
    let mut saved = Vec::new();
    for (name, hwnd) in tags::account_windows(cfg) {
        match slot_of(hwnd) {
            Some(s) => {
                saved.push(format!("{name} on {}", monitor_label(&s.monitor)));
                slots.insert(name, s);
            }
            None => left_out.push(name),
        }
    }
    let saved_at = crate::win::timestamp();
    (Layout { saved_at, slots }, saved, left_out)
}

/// Pick the display a slot belongs on: same name, else the same rectangle, else the primary one.
fn target_monitor<'a>(mons: &'a [Mon], slot: &Slot) -> Option<(&'a Mon, bool)> {
    let r = slot.monitor_rect;
    mons.iter()
        .find(|m| m.name == slot.monitor)
        .map(|m| (m, true))
        .or_else(|| mons.iter().find(|m| [m.rect.left, m.rect.top, m.rect.right, m.rect.bottom] == r).map(|m| (m, true)))
        .or_else(|| mons.iter().find(|m| m.primary).or(mons.first()).map(|m| (m, false)))
}

/// Move one window to its saved spot.
pub fn apply(hwnd: HWND, slot: &Slot) {
    let mons = monitors();
    let Some((mon, _)) = target_monitor(&mons, slot) else { return };
    unsafe {
        if IsHungAppWindow(hwnd) != 0 {
            return;
        }
        let (ww, wh) = (mon.work.right - mon.work.left, mon.work.bottom - mon.work.top);
        // min() first: clamp panics when the lower bound is above the upper one.
        let w = slot.w.clamp(400.min(ww), ww.max(1));
        let h = slot.h.clamp(300.min(wh), wh.max(1));
        // Where a saved position can't be used (maximized, or it no longer fits the
        // monitor, which may have changed resolution or been swapped): centered.
        let cx = mon.work.left + (ww - w) / 2;
        let cy = mon.work.top + (wh - h) / 2;
        let (x, y) = if slot.maximized { (cx, cy) } else { clamp_into(mon, mon.rect.left + slot.x, mon.rect.top + slot.y, w, h, cx, cy) };
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        if slot.maximized {
            let here = mon_info(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST));
            if IsZoomed(hwnd) != 0 && here.is_some_and(|m| m.name == mon.name) {
                return;
            }
        }
        if IsZoomed(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        if slot.maximized {
            ShowWindow(hwnd, SW_MAXIMIZE);
        }
    }
}

/// Keep a window whose monitor changed inside the new monitor's usable area.
fn clamp_into(mon: &Mon, x: i32, y: i32, w: i32, h: i32, fx: i32, fy: i32) -> (i32, i32) {
    let fits = x >= mon.work.left - 16 && y >= mon.work.top - 16 && x.saturating_add(w) <= mon.work.right + 16 && y.saturating_add(h) <= mon.work.bottom + 16;
    if fits { (x, y) } else { (fx, fy) }
}

/// Put every open account's window back. Returns (moved, not open, hidden or minimized).
pub fn restore(cfg: &Config) -> (usize, Vec<String>, Vec<String>) {
    let open = tags::account_windows(cfg);
    let mut moved = 0;
    let mut hidden = Vec::new();
    for (name, hwnd) in &open {
        let Some(slot) = cfg.layout.slots.get(name) else { continue };
        if unsafe { IsWindowVisible(*hwnd) == 0 } {
            hidden.push(name.clone());
            continue;
        }
        apply(*hwnd, slot);
        moved += 1;
    }
    let closed = cfg
        .layout
        .slots
        .keys()
        .filter(|n| !open.iter().any(|(o, _)| o == *n))
        .cloned()
        .collect();
    (moved, closed, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, l: i32, primary: bool) -> Mon {
        let rect = RECT { left: l, top: 0, right: l + 2560, bottom: 1440 };
        Mon { name: name.into(), rect, work: rect, primary }
    }

    #[test]
    fn monitor_is_found_by_name_then_rectangle_then_primary() {
        let mons = vec![mon("\\\\.\\DISPLAY2", 0, true), mon("\\\\.\\DISPLAY3", -2560, false)];
        let mut slot = Slot { monitor: "\\\\.\\DISPLAY3".into(), monitor_rect: [-2560, 0, 0, 1440], ..Slot::default() };
        assert_eq!(target_monitor(&mons, &slot).map(|(m, e)| (m.name.clone(), e)), Some(("\\\\.\\DISPLAY3".into(), true)));
        slot.monitor = "\\\\.\\DISPLAY9".into(); // renamed, same place
        assert_eq!(target_monitor(&mons, &slot).map(|(m, e)| (m.name.clone(), e)), Some(("\\\\.\\DISPLAY3".into(), true)));
        slot.monitor_rect = [9000, 0, 11560, 1440]; // unplugged
        assert_eq!(target_monitor(&mons, &slot).map(|(m, e)| (m.name.clone(), e)), Some(("\\\\.\\DISPLAY2".into(), false)));
    }

    #[test]
    fn labels() {
        assert_eq!(monitor_label("\\\\.\\DISPLAY2"), "monitor 2");
        assert_eq!(monitor_label("weird"), "a monitor");
    }

    fn mi(name: &str, l: i32, primary: bool) -> MonInfo {
        MonInfo { name: name.into(), rect: [l, 0, l + 1920, 1080], work: [l, 0, l + 1920, 1040], primary, dpi: 96 }
    }

    #[test]
    fn monitor_chosen_by_name_then_index_then_primary() {
        let mut m = vec![mi("B", 1920, true), mi("C", 3840, false), mi("A", 0, false)];
        sort_left_to_right(&mut m);
        assert_eq!(m.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["A", "B", "C"]);
        assert_eq!(choose_monitor(&m, "", None), Some(1), "nothing saved: primary");
        assert_eq!(choose_monitor(&m, "C", Some(0)), Some(2), "name wins over index");
        assert_eq!(choose_monitor(&m, "Z", Some(0)), Some(0), "renamed: index");
        assert_eq!(choose_monitor(&m, "Z", Some(7)), Some(1), "index gone too: primary");
        assert_eq!(choose_monitor(&m, "Z", None), Some(1));
        assert_eq!(choose_monitor(&[], "Z", None), None);
    }

    #[test]
    fn monitor_labels_for_three_two_and_four() {
        let three = vec![mi("A", 0, false), mi("B", 1920, true), mi("C", 3840, false)];
        assert_eq!(monitor_labels(&three), ["Left", "Main", "Right"]);
        let two = vec![mi("A", 0, true), mi("B", 1920, false)];
        assert_eq!(monitor_labels(&two), ["Monitor 1 (main)", "Monitor 2"]);
        let four = vec![mi("A", 0, false), mi("B", 1, false), mi("C", 2, true), mi("D", 3, false)];
        assert_eq!(monitor_labels(&four), ["Monitor 1", "Monitor 2", "Monitor 3 (main)", "Monitor 4"]);
        let odd = vec![mi("A", 0, true), mi("B", 1, false), mi("C", 2, false)];
        assert_eq!(monitor_labels(&odd), ["Monitor 1 (main)", "Monitor 2", "Monitor 3"]);
    }

    #[test]
    fn stacks_sit_in_each_corner() {
        let work = [100, 50, 2020, 1090];
        let hs = [80, 120, 100];
        let (w, margin, gap) = (400, 12, 8);
        for c in [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight] {
            let r = stack_rects(work, c, &hs, w, margin, gap);
            assert_eq!(r.len(), 3);
            for (i, q) in r.iter().enumerate() {
                assert!(q[0] >= work[0] && q[2] <= work[2] && q[1] >= work[1] && q[3] <= work[3], "{c:?} inside");
                assert_eq!((q[2] - q[0], q[3] - q[1]), (w, hs[i]));
                assert_eq!(q[0], if c.left() { work[0] + margin } else { work[2] - margin - w });
            }
            // Newest (last) in the corner; 8 px between neighbors; older ones further from the edge.
            if c.top() {
                assert_eq!(r[2][1], work[1] + margin);
                assert_eq!(r[1][1], r[2][3] + gap);
                assert_eq!(r[0][1], r[1][3] + gap);
            } else {
                assert_eq!(r[2][3], work[3] - margin);
                assert_eq!(r[1][3], r[2][1] - gap);
                assert_eq!(r[0][3], r[1][1] - gap);
            }
        }
        assert_eq!(Corner::parse("bottom_left"), Corner::BottomLeft);
        assert_eq!(Corner::parse("nonsense"), Corner::BottomRight);
    }
}
