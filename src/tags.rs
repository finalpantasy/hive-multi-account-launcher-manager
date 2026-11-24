//! Account name tags: a small colored tab at the top edge of each open Claude
//! window that says which account it is. The tags are our own click-through
//! windows (another app's window can't be recolored). Nothing polls: Windows
//! tells us when a Claude window moves, shows, hides or is covered, through
//! WinEvent hooks limited to those processes.

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetDC, GetMonitorInfoW, GetTextExtentPoint32W, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, SetWindowRgn, InvalidateRect, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::{Config, Slot};
use crate::procs;
use crate::win::wide;

const CLASS: &str = "HiveTag";
/// A tag is this tall at 100% scale.
const TAG_H: f32 = 24.0;
const TAG_ALPHA: u8 = 238;

/// Accounts this app just opened: when their window appears, put it back where
/// the saved layout says. (account, when)
static EXPECT: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
/// Accounts whose window just appeared and should be moved to its saved spot.
static APPLY: Mutex<Vec<String>> = Mutex::new(Vec::new());
static RECHECK_TIMER: AtomicUsize = AtomicUsize::new(0);
static APPLY_TIMER: AtomicUsize = AtomicUsize::new(0);
static TRAY_TIMER: AtomicUsize = AtomicUsize::new(0);

struct Tag {
    name: String,
    rgb: u32,
    pid: u32,
    /// The account's Claude window (null until it shows).
    target: HWND,
    /// Our tag window.
    hwnd: HWND,
    /// Size last given to the tag window: (w, h, dpi).
    size: (i32, i32, u32),
    shown: bool,
    covered: bool,
    /// Its own taskbar button, and the color its icon was drawn in.
    marked: Option<(crate::taskbar::Marked, u32)>,
}

struct State {
    on: bool,
    /// One taskbar button per account.
    separate: bool,
    tracking: bool,
    restore_on_open: bool,
    slots: std::collections::BTreeMap<String, Slot>,
    tags: Vec<Tag>,
    /// Per-process hooks and the process each belongs to.
    hooks: Vec<(u32, HWINEVENTHOOK, HWINEVENTHOOK)>,
    global: Vec<HWINEVENTHOOK>,
    class_ready: bool,
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State {
        on: false,
        separate: false,
        tracking: false,
        restore_on_open: false,
        slots: std::collections::BTreeMap::new(),
        tags: Vec::new(),
        hooks: Vec::new(),
        global: Vec::new(),
        class_ready: false,
    }) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|c| c.try_borrow_mut().ok().map(|mut s| f(&mut s)))
}

// ---- finding windows -------------------------------------------------------

struct Find {
    pid: u32,
    best: HWND,
    best_area: i64,
    best_visible: bool,
}

unsafe extern "system" fn find_cb(h: HWND, lp: LPARAM) -> i32 {
    let f = &mut *(lp as *mut Find);
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, &mut pid);
    if pid != f.pid {
        return 1;
    }
    let style = GetWindowLongW(h, GWL_STYLE) as u32;
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    // The app window: a captioned (or resizable: the Codex app drops its caption
    // when maximized) top-level window that isn't a tool window.
    let framed = style & WS_CAPTION == WS_CAPTION || style & WS_THICKFRAME != 0;
    if !framed || ex & WS_EX_TOOLWINDOW != 0 || !GetWindow(h, GW_OWNER).is_null() {
        return 1;
    }
    let mut r: RECT = std::mem::zeroed();
    GetWindowRect(h, &mut r);
    let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
    let visible = IsWindowVisible(h) != 0;
    if (visible, area) > (f.best_visible, f.best_area) {
        f.best = h;
        f.best_area = area;
        f.best_visible = visible;
    }
    1
}

/// The main window of a Claude process (visible ones first, then the largest).
pub fn window_of(pid: u32) -> Option<HWND> {
    let mut f = Find { pid, best: null_mut(), best_area: -1, best_visible: false };
    unsafe { EnumWindows(Some(find_cb), &mut f as *mut Find as LPARAM) };
    (!f.best.is_null()).then_some(f.best)
}

/// Each open account's name and window (for saving and restoring layouts).
pub fn account_windows(cfg: &Config) -> Vec<(String, HWND)> {
    let mut out: Vec<(String, HWND)> = procs::main_pids(cfg, &procs::list())
        .into_iter()
        .filter_map(|(name, pid)| window_of(pid).map(|h| (name, h)))
        .collect();
    let order = |n: &str| cfg.accounts.iter().position(|a| a.name == n).unwrap_or(usize::MAX);
    out.sort_by_key(|(n, _)| order(n));
    out
}

// ---- geometry ---------------------------------------------------------------

fn cloaked(h: HWND) -> bool {
    let mut v = 0i32;
    unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut v as *mut i32 as *mut _, 4) == 0 && v != 0 }
}

/// The window's visible rectangle (without the invisible resize border).
fn frame(h: HWND) -> RECT {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        let size = std::mem::size_of::<RECT>() as u32;
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut r as *mut RECT as *mut _, size) != 0 {
            GetWindowRect(h, &mut r);
        }
    }
    r
}

fn fullscreen(h: HWND, r: &RECT) -> bool {
    unsafe {
        if GetWindowLongW(h, GWL_STYLE) as u32 & WS_CAPTION == WS_CAPTION {
            return false;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(mon, &mut mi) != 0
            && r.left <= mi.rcMonitor.left
            && r.top <= mi.rcMonitor.top
            && r.right >= mi.rcMonitor.right
            && r.bottom >= mi.rcMonitor.bottom
    }
}

struct Cover {
    target: HWND,
    x: i32,
    y: i32,
    by: HWND,
}

unsafe extern "system" fn cover_cb(h: HWND, lp: LPARAM) -> i32 {
    let c = &mut *(lp as *mut Cover);
    if h == c.target {
        return 0; // reached the Claude window first: nothing is on top of that spot
    }
    if IsWindowVisible(h) == 0 || IsIconic(h) != 0 || is_tag(h) {
        return 1;
    }
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TRANSPARENT != 0 && ex & WS_EX_LAYERED != 0 {
        return 1; // click-through overlay
    }
    if cloaked(h) {
        return 1;
    }
    let mut r: RECT = std::mem::zeroed();
    GetWindowRect(h, &mut r);
    if c.x >= r.left && c.x < r.right && c.y >= r.top && c.y < r.bottom {
        c.by = h;
        return 0;
    }
    1
}

/// Is another window on top of the spot where the tag would sit?
fn covered_at(target: HWND, x: i32, y: i32) -> Option<HWND> {
    let mut c = Cover { target, x, y, by: null_mut() };
    unsafe { EnumWindows(Some(cover_cb), &mut c as *mut Cover as LPARAM) };
    (!c.by.is_null()).then_some(c.by)
}



fn is_tag(h: HWND) -> bool {
    let mut buf = [0u16; 32];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), 32) } as usize;
    String::from_utf16_lossy(&buf[..n]) == CLASS
}

// ---- the tag window ------------------------------------------------------------

fn font(dpi: u32) -> windows_sys::Win32::Graphics::Gdi::HFONT {
    let px = -((12.5 * dpi as f32 / 96.0).round() as i32);
    let face = wide("Segoe UI Semibold");
    unsafe { CreateFontW(px, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr()) }
}

/// Tag size in pixels for this text at this DPI.
fn measure(name: &str, dpi: u32) -> (i32, i32) {
    let scale = dpi as f32 / 96.0;
    let text: Vec<u16> = name.encode_utf16().collect();
    let mut sz = SIZE { cx: 0, cy: 0 };
    unsafe {
        let dc = GetDC(null_mut());
        let f = font(dpi);
        let old = SelectObject(dc, f as _);
        GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut sz);
        SelectObject(dc, old);
        DeleteObject(f as _);
        ReleaseDC(null_mut(), dc);
    }
    ((sz.cx as f32 + 28.0 * scale).round() as i32, (TAG_H * scale).round() as i32)
}

fn color_ref(rgb: u32) -> COLORREF {
    ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)
}

unsafe extern "system" fn tag_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => -1, // HTTRANSPARENT: clicks go to the window underneath
        WM_MOUSEACTIVATE => 3, // MA_NOACTIVATE
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            // Window text is the account name; user data holds dpi << 32 | 0xRRGGBB.
            let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u64;
            let (dpi, rgb) = (((data >> 32) as u32).max(96), (data & 0xFF_FFFF) as u32);
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut r);
            let brush = CreateSolidBrush(color_ref(rgb));
            FillRect(dc, &r, brush);
            DeleteObject(brush as _);
            let mut buf = [0u16; 64];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), 64);
            let f = font(dpi);
            let old = SelectObject(dc, f as _);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00FF_FFFF);
            DrawTextW(dc, buf.as_ptr(), n, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(dc, old);
            DeleteObject(f as _);
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn make_tag_window(name: &str, rgb: u32) -> HWND {
    unsafe {
        let hinst: HMODULE = GetModuleHandleW(null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(tag_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc); // fails harmlessly when already registered
        let title = wide(name);
        let h = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if !h.is_null() {
            SetLayeredWindowAttributes(h, 0, TAG_ALPHA, LWA_ALPHA);
            SetWindowLongPtrW(h, GWLP_USERDATA, rgb as isize);
        }
        h
    }
}

impl Tag {
    /// Give the window its own taskbar button (or take it back), following the window and color.
    fn sync_taskbar(&mut self, separate: bool) {
        let want = separate && !self.target.is_null();
        let current = self.marked.as_ref().map(|(m, rgb)| (m.hwnd, *rgb));
        if want && current == Some((self.target, self.rgb)) {
            return;
        }
        if let Some((m, _)) = self.marked.take() {
            m.undo();
        }
        if want {
            self.marked = crate::taskbar::mark(self.target, &self.name, self.rgb).map(|m| (m, self.rgb));
        }
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.shown = false;
        }
    }

    /// Put the tag on its window's top edge, or hide it. `check_cover` is false
    /// while a window is being dragged (it is in front then).
    fn place(&mut self, on: bool, check_cover: bool) {
        let t = self.target;
        let usable = on && !t.is_null() && unsafe { IsWindow(t) != 0 && IsWindowVisible(t) != 0 && IsIconic(t) == 0 } && !cloaked(t);
        if !usable {
            self.covered = false;
            return self.hide();
        }
        let r = frame(t);
        if fullscreen(t, &r) {
            return self.hide();
        }
        let dpi = match unsafe { GetDpiForWindow(t) } {
            0 => 96,
            d => d,
        };
        let (w, h) = if self.size.2 == dpi && self.size.0 > 0 { (self.size.0, self.size.1) } else { measure(&self.name, dpi) };
        let x = r.left + ((r.right - r.left) - w) / 2;
        let y = r.top;
        if check_cover || self.covered {
            let by = covered_at(t, x + w / 2, y + h / 2);
            self.covered = by.is_some();
        }
        if self.covered {
            return self.hide();
        }
        unsafe {
            if self.size != (w, h, dpi) {
                self.size = (w, h, dpi);
                let rad = (8.0 * dpi as f32 / 96.0).round() as i32;
                // Rounded bottom corners: the rounded rectangle reaches above the window, so its top corners are cut off.
                let rgn = CreateRoundRectRgn(0, -rad, w + 1, h + 1, rad * 2, rad * 2);
                SetWindowRgn(self.hwnd, rgn, 0);
                let data = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as u64 & 0xFF_FFFF;
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, (data | (dpi as u64) << 32) as isize);
                InvalidateRect(self.hwnd, null(), 0);
            }
            SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        self.shown = true;
    }
}

// ---- hooks ---------------------------------------------------------------------

unsafe extern "system" fn win_event(_hook: HWINEVENTHOOK, event: u32, hwnd: HWND, id_object: i32, _id_child: i32, _thread: u32, _time: u32) {
    match event {
        EVENT_SYSTEM_FOREGROUND => {
            if let Some(name) = account_of_window(hwnd) {
                crate::tray::account_focused(&name);
            }
            schedule_recheck();
        }
        EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND => schedule_recheck(),
        EVENT_OBJECT_LOCATIONCHANGE if id_object == OBJID_WINDOW => {
            with(|s| {
                let on = s.on;
                if let Some(t) = s.tags.iter_mut().find(|t| t.target == hwnd) {
                    t.place(on, false);
                }
            });
        }
        EVENT_SYSTEM_MOVESIZEEND => schedule_recheck(),
        EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE | EVENT_OBJECT_DESTROY if id_object == OBJID_WINDOW => {
            let appeared = with(|s| window_event(s, event, hwnd)).flatten();
            if let Some(name) = appeared {
                appeared_window(&name);
            }
            // Any program's top-level window opening or closing (a popup, a screenshot overlay) can cover a tag.
            if event != EVENT_OBJECT_DESTROY && !hwnd.is_null() && GetAncestor(hwnd, GA_PARENT) == GetDesktopWindow() {
                schedule_recheck();
            }
        }
        _ => {}
    }
}

/// Which account a window belongs to: one of the tagged Claude windows, or
/// "Codex" for the Codex app.
fn account_of_window(hwnd: HWND) -> Option<String> {
    if hwnd.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    if let Some(name) = with(|s| s.tags.iter().find(|t| !t.target.is_null() && (t.target == hwnd || t.target == root)).map(|t| t.name.clone())).flatten() {
        return Some(name);
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let exe = procs::image_path(pid)?;
    let stem = std::path::Path::new(&exe).file_stem()?.to_string_lossy().to_ascii_lowercase();
    // The Codex app is ChatGPT.exe (its window shows Codex); codex.exe is the CLI.
    let codex_app = stem == "chatgpt" && exe.to_ascii_lowercase().contains("\\windowsapps\\openai.codex_");
    (stem == "codex" || codex_app).then(|| "Codex".to_string())
}

/// A Claude window showed, hid or closed. Returns the account whose window was just found.
fn window_event(s: &mut State, event: u32, hwnd: HWND) -> Option<String> {
    let on = s.on;
    if let Some(t) = s.tags.iter_mut().find(|t| t.target == hwnd) {
        if event == EVENT_OBJECT_DESTROY {
            t.target = null_mut();
            t.sync_taskbar(false);
        }
        t.place(on, true);
        return None;
    }
    if event != EVENT_OBJECT_SHOW {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let separate = s.separate;
    // No window yet, or the one found first went hidden (the Codex app opens a
    // first window, then shows its real one): take the window that just showed.
    let t = s.tags.iter_mut().find(|t| t.pid == pid && (t.target.is_null() || unsafe { IsWindowVisible(t.target) } == 0))?;
    let found = window_of(pid)?;
    if found == t.target || unsafe { IsWindowVisible(found) } == 0 {
        return None;
    }
    t.target = found;
    crate::log(&format!("window of {} appeared", t.name));
    t.place(on, true);
    t.sync_taskbar(separate);
    Some(t.name.clone())
}

fn schedule_recheck() {
    unsafe {
        let id = SetTimer(null_mut(), RECHECK_TIMER.load(Ordering::Relaxed), 80, Some(recheck_proc));
        RECHECK_TIMER.store(id, Ordering::Relaxed);
    }
}

unsafe extern "system" fn recheck_proc(_h: HWND, _msg: u32, id: usize, _time: u32) {
    KillTimer(null_mut(), id);
    RECHECK_TIMER.store(0, Ordering::Relaxed);
    with(|s| {
        let on = s.on;
        for t in &mut s.tags {
            t.place(on, true);
        }
    });
}

// ---- opening and restoring ------------------------------------------------------

/// This app is opening `name` (it wasn't running): when its window appears, restore its saved spot.
pub fn expect_window(name: &str) {
    let mut e = EXPECT.lock().unwrap();
    e.retain(|(n, at)| n != name && at.elapsed() < Duration::from_secs(180));
    e.push((name.to_string(), Instant::now()));
}

fn appeared_window(name: &str) {
    let expected = {
        let mut e = EXPECT.lock().unwrap();
        let before = e.len();
        e.retain(|(n, at)| n != name || at.elapsed() >= Duration::from_secs(180));
        e.len() < before
    };
    let restore = with(|s| s.restore_on_open && s.slots.contains_key(name)).unwrap_or(false);
    if expected && restore {
        APPLY.lock().unwrap().push(name.to_string());
        unsafe {
            // Claude moves its window to its own remembered spot as it opens: wait for that.
            let id = SetTimer(null_mut(), APPLY_TIMER.load(Ordering::Relaxed), 1500, Some(apply_proc));
            APPLY_TIMER.store(id, Ordering::Relaxed);
        }
    }
}

/// Every 30 s while taskbar buttons are separate: put the badges back on Claude's
/// notification-area icons (Claude or a restarted Explorer may have replaced them).
unsafe extern "system" fn tray_proc(_h: HWND, _msg: u32, _id: usize, _time: u32) {
    with(|s| {
        for t in &mut s.tags {
            if let Some((m, _)) = t.marked.as_mut() {
                m.refresh_tray();
            }
        }
    });
}

unsafe extern "system" fn apply_proc(_h: HWND, _msg: u32, id: usize, _time: u32) {
    KillTimer(null_mut(), id);
    APPLY_TIMER.store(0, Ordering::Relaxed);
    let names: Vec<String> = std::mem::take(&mut *APPLY.lock().unwrap());
    for name in names {
        let job = with(|s| {
            let t = s.tags.iter().find(|t| t.name == name)?;
            Some((t.target, s.slots.get(&name)?.clone()))
        })
        .flatten();
        if let Some((hwnd, slot)) = job {
            if !hwnd.is_null() {
                crate::layout::apply(hwnd, &slot);
                crate::log(&format!("put {name} back where it was saved"));
            }
        }
    }
}

// ---- syncing with the running accounts -------------------------------------------

/// The settings or the set of open accounts changed: add and remove tags, and
/// hook the processes that matter.
pub fn sync(cfg: &Config) {
    let tracking = cfg.show_tags || cfg.restore_on_open || cfg.separate_taskbar;
    let pids = if tracking { procs::main_pids(cfg, &procs::list()) } else { Default::default() };
    let mut appeared: Vec<String> = Vec::new();
    let colors = crate::ui::avatar_palette(cfg.tag_names().into_iter());
    with(|s| {
        s.on = cfg.show_tags;
        s.separate = cfg.separate_taskbar;
        s.restore_on_open = cfg.restore_on_open;
        s.slots = cfg.layout.slots.clone();
        s.tracking = tracking;
        if !s.class_ready && tracking {
            s.class_ready = true;
        }
        // Drop tags for accounts that closed (or all of them when off).
        let mut gone = Vec::new();
        s.tags.retain_mut(|t| {
            let keep = pids.get(&t.name) == Some(&t.pid);
            if !keep {
                unsafe { DestroyWindow(t.hwnd) };
                gone.extend(t.marked.take());
            }
            keep
        });
        for (m, _) in gone {
            m.undo();
        }
        for (name, &pid) in &pids {
            if s.tags.iter().any(|t| &t.name == name) {
                continue;
            }
            let rgb = colors.iter().find(|(n, _)| n == name).map_or(0x888888, |(_, c)| *c);
            let hwnd = make_tag_window(name, rgb);
            if hwnd.is_null() {
                continue;
            }
            let target = window_of(pid).unwrap_or(null_mut());
            crate::log(&format!("tag for {name}: process {pid}, window {}", if target.is_null() { "not found yet".to_string() } else { format!("{target:?}") }));
            if !target.is_null() {
                appeared.push(name.clone());
            }
            s.tags.push(Tag { name: name.clone(), rgb, pid, target, hwnd, size: (0, 0, 0), shown: false, covered: false, marked: None });
        }
        // A renamed account keeps its pid but its tag text must change: recreate on mismatch is rare, so only the colors/names follow the config.
        let (on, separate) = (s.on, s.separate);
        unsafe {
            let id = TRAY_TIMER.load(Ordering::Relaxed);
            if separate && id == 0 {
                TRAY_TIMER.store(SetTimer(null_mut(), 0, 30_000, Some(tray_proc)), Ordering::Relaxed);
            } else if !separate && id != 0 {
                KillTimer(null_mut(), id);
                TRAY_TIMER.store(0, Ordering::Relaxed);
            }
        }
        for t in &mut s.tags {
            // Colors can shift when accounts are added or removed.
            let rgb = colors.iter().find(|(n, _)| *n == t.name).map_or(t.rgb, |(_, c)| *c);
            if rgb != t.rgb {
                t.rgb = rgb;
                unsafe {
                    let data = GetWindowLongPtrW(t.hwnd, GWLP_USERDATA) as u64 & !0xFF_FFFF;
                    SetWindowLongPtrW(t.hwnd, GWLP_USERDATA, (data | rgb as u64) as isize);
                    InvalidateRect(t.hwnd, null(), 0);
                }
            }
            if t.target.is_null() || unsafe { IsWindow(t.target) } == 0 {
                t.target = window_of(t.pid).unwrap_or(null_mut());
            } else if unsafe { IsWindowVisible(t.target) } == 0 {
                // A hidden first window: a visible one from the same app wins.
                if let Some(w) = window_of(t.pid).filter(|&w| unsafe { IsWindowVisible(w) } != 0) {
                    t.target = w;
                }
            }
            t.place(on, true);
            t.sync_taskbar(separate);
            if !separate && t.marked.is_none() && !t.target.is_null() {
                crate::taskbar::clear_stale(t.target);
            }
        }
        set_hooks(s, tracking);
    });
    for name in appeared {
        // Found at sync time (the tray started after the window, or it opened very fast).
        appeared_window(&name);
    }
}

fn set_hooks(s: &mut State, tracking: bool) {
    unsafe {
        let wanted: Vec<u32> = if tracking { s.tags.iter().map(|t| t.pid).collect() } else { Vec::new() };
        s.hooks.retain(|&(pid, a, b)| {
            let keep = wanted.contains(&pid);
            if !keep {
                UnhookWinEvent(a);
                UnhookWinEvent(b);
            }
            keep
        });
        for pid in wanted {
            if s.hooks.iter().any(|h| h.0 == pid) {
                continue;
            }
            let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
            // Closing, showing and hiding; and moving or resizing.
            let a = SetWinEventHook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, null_mut(), Some(win_event), pid, 0, flags);
            let b = SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, null_mut(), Some(win_event), pid, 0, flags);
            s.hooks.push((pid, a, b));
        }
        if tracking && s.global.is_empty() {
            let flags = WINEVENT_OUTOFCONTEXT;
            // Another window came to the front, or one was minimized or restored: tags may be covered or uncovered.
            s.global.push(SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, null_mut(), Some(win_event), 0, 0, flags));
            s.global.push(SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, null_mut(), Some(win_event), 0, 0, flags));
            // A window dropped after a drag, and other programs' windows opening or closing.
            s.global.push(SetWinEventHook(EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND, null_mut(), Some(win_event), 0, 0, flags));
            s.global.push(SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_HIDE, null_mut(), Some(win_event), 0, 0, flags | WINEVENT_SKIPOWNPROCESS));
        } else if !tracking {
            for h in s.global.drain(..) {
                UnhookWinEvent(h);
            }
        }
    }
}

pub fn shutdown() {
    with(|s| {
        for mut t in s.tags.drain(..) {
            unsafe { DestroyWindow(t.hwnd) };
            if let Some((m, _)) = t.marked.take() {
                m.undo();
            }
        }
        set_hooks(s, false);
    });
}
