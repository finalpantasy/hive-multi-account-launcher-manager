//! Small safe wrappers over the Win32 calls the app needs.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{GlobalFree, HWND, RECT, SYSTEMTIME};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, RRF_RT_REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, GetLastInputInfo, KEYEVENTF_KEYUP, LASTINPUTINFO, VK_MENU};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId,
    GetClassNameW, IsIconic, IsWindowVisible, MessageBoxW, SetForegroundWindow, ShowWindow, SwitchToThisWindow, ASFW_ANY,
    GA_ROOT, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL,
};

const CF_UNICODETEXT: u32 = 13;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Copy `s` into a fixed UTF-16 buffer, truncating and NUL-terminating.
pub fn fill(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for (i, c) in s.encode_utf16().take(dst.len() - 1).enumerate() {
        dst[i] = c;
        n = i + 1;
    }
    dst[n] = 0;
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub fn msg_box(hwnd: HWND, text: &str, flags: u32) -> i32 {
    let text = wide(text);
    let title = wide(crate::APP_NAME);
    unsafe { MessageBoxW(hwnd, text.as_ptr(), title.as_ptr(), flags) }
}

pub fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    let data = wide(text);
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        if mem.is_null() {
            CloseClipboard();
            return false;
        }
        let dst = GlobalLock(mem) as *mut u16;
        std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
        GlobalUnlock(mem);
        let ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
        if !ok {
            GlobalFree(mem);
        }
        CloseClipboard();
        ok
    }
}

pub fn get_clipboard(hwnd: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return None;
        }
        let mut out = None;
        let mem = GetClipboardData(CF_UNICODETEXT);
        if !mem.is_null() {
            let p = GlobalLock(mem) as *const u16;
            if !p.is_null() {
                let mut n = 0;
                while *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(mem);
            }
        }
        CloseClipboard();
        out
    }
}

/// Open a file, folder or program through the shell.
pub fn shell_open(file: &str, params: Option<&str>) {
    let verb = wide("open");
    let file = wide(file);
    let params = params.map(wide);
    unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ref().map_or(null(), |p| p.as_ptr()),
            null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Bring a window to the front even when this process is not the foreground one
/// (an alert card never takes focus, so a bare SetForegroundWindow is refused and
/// the taskbar only flashes). Joins the foreground thread's input queue for the
/// call, which lifts the foreground lock. Returns whether `h` ended up in front.
pub fn bring_forward(h: HWND) -> bool {
    bring_forward_why(h).is_ok()
}

/// Window classes of the Windows shell: a launch from the desktop, a folder, the
/// taskbar or the Start menu has one of these in front (no app is being typed in).
pub fn is_shell_class(class: &str) -> bool {
    matches!(
        class,
        "Progman"
            | "WorkerW"
            | "CabinetWClass"
            | "ExploreWClass"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "TopLevelWindowForOverflowXamlIsland"
            | "XamlExplorerHostIslandWindow"
            | "Windows.UI.Core.CoreWindow"
    )
}

/// The foreground window's class name; empty when nothing is in front.
pub fn foreground_class() -> String {
    class_of(unsafe { GetForegroundWindow() })
}

fn class_of(h: HWND) -> String {
    if h.is_null() {
        return String::new();
    }
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Should a start that nobody asked to see take the foreground? Only when the
/// user is not in an app: nothing in front, or the shell (a double click on the
/// exe). A deploy or script start lands with their editor or chat in front, and
/// that keeps the keyboard.
pub fn start_may_take_foreground(front_class: &str) -> bool {
    front_class.is_empty() || is_shell_class(front_class)
}

/// What a failed `bring_forward` saw, for the log (pure, so it can be tested).
pub fn front_failure(visible: bool, iconic: bool, attached: bool, front_class: &str, front_pid_same: bool) -> String {
    format!(
        "window {}{}, input queue {}, foreground is {}{}",
        if visible { "visible" } else { "hidden" },
        if iconic { " and minimized" } else { "" },
        if attached { "joined" } else { "not joined" },
        if front_class.is_empty() { "nothing" } else { front_class },
        if front_pid_same { " (same app)" } else { "" },
    )
}

/// `bring_forward` with the reason when the window still is not in front.
/// A window hidden to its tray icon (Claude closed to the tray) is shown first:
/// SetForegroundWindow cannot raise a window that is not visible.
pub fn bring_forward_why(h: HWND) -> Result<(), String> {
    if h.is_null() {
        return Err("no window".into());
    }
    unsafe {
        let is_front = |h: HWND| {
            let f = GetForegroundWindow();
            !f.is_null() && (f == h || GetAncestor(f, GA_ROOT) == GetAncestor(h, GA_ROOT))
        };
        // The switch can land a moment after the call returns (Electron apps).
        let settled = |h: HWND| {
            for _ in 0..6 {
                if is_front(h) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            is_front(h)
        };
        AllowSetForegroundWindow(ASFW_ANY);
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
        } else if IsWindowVisible(h) == 0 {
            ShowWindow(h, SW_SHOW);
        }
        if is_front(h) {
            return Ok(());
        }
        let me = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let fg_thread = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, null_mut()) };
        let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, 1) != 0;
        BringWindowToTop(h);
        SetForegroundWindow(h);
        if attached {
            AttachThreadInput(me, fg_thread, 0);
        }
        if settled(h) {
            return Ok(());
        }
        // Last resort: a synthetic Alt tap marks this process as having had input.
        keybd_event(VK_MENU as u8, 0, 0, 0);
        keybd_event(VK_MENU as u8, 0, KEYEVENTF_KEYUP, 0);
        SetForegroundWindow(h);
        if settled(h) {
            return Ok(());
        }
        SwitchToThisWindow(h, 1);
        if settled(h) {
            return Ok(());
        }
        let f = GetForegroundWindow();
        let (mut p_f, mut p_h) = (0u32, 0u32);
        GetWindowThreadProcessId(f, &mut p_f);
        GetWindowThreadProcessId(h, &mut p_h);
        Err(front_failure(IsWindowVisible(h) != 0, IsIconic(h) != 0, attached, &class_of(f), p_f != 0 && p_f == p_h))
    }
}

/// Seconds since the last keyboard or mouse input anywhere in this session.
pub fn idle_secs() -> u64 {
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if GetLastInputInfo(&mut li) == 0 {
            return u64::MAX;
        }
        (GetTickCount().wrapping_sub(li.dwTime) / 1000) as u64
    }
}

/// A monitor's work area minus any auto-hide taskbar on its edges. An auto-hide
/// taskbar leaves the work area full size, then slides up over whatever sits there.
pub fn usable_area(wk: RECT) -> RECT {
    use windows_sys::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA};
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetWindowRect};
    let mut r = wk;
    unsafe {
        let mut abd: APPBARDATA = std::mem::zeroed();
        abd.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
        if (SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32) & ABS_AUTOHIDE == 0 {
            return r;
        }
        for class in ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
            let class = wide(class);
            let mut h: HWND = null_mut();
            loop {
                h = FindWindowExW(null_mut(), h, class.as_ptr(), null());
                if h.is_null() {
                    break;
                }
                let mut t: RECT = std::mem::zeroed();
                GetWindowRect(h, &mut t);
                let (w, th) = (t.right - t.left, t.bottom - t.top);
                if w >= th {
                    if t.right <= wk.left || t.left >= wk.right || t.bottom < wk.top - th || t.top > wk.bottom + th {
                        continue; // another monitor
                    }
                    if t.top >= (wk.top + wk.bottom) / 2 { r.bottom = r.bottom.min(wk.bottom - th) } else { r.top = r.top.max(wk.top + th) }
                } else {
                    if t.bottom <= wk.top || t.top >= wk.bottom || t.right < wk.left - w || t.left > wk.right + w {
                        continue;
                    }
                    if t.left >= (wk.left + wk.right) / 2 { r.right = r.right.min(wk.right - w) } else { r.left = r.left.max(wk.left + w) }
                }
            }
        }
    }
    r
}

/// For checking the layout at another scale (HIVE_SCALE=1 draws at 100%
/// on a 150% screen). Not a setting: only read from the environment.
pub fn debug_scale() -> Option<f32> {
    static S: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *S.get_or_init(|| std::env::var("HIVE_SCALE").or_else(|_| std::env::var("SWITCHBOARD_SCALE")).or_else(|_| std::env::var("CLAUDE_ACCOUNTS_SCALE")).ok().and_then(|v| v.parse().ok()).filter(|s: &f32| (0.5..=4.0).contains(s)))
}

pub fn timestamp() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

// ---- HKCU registry -------------------------------------------------------

fn open_key(subkey: &str, access: u32) -> Option<HKEY> {
    let sub = wide(subkey);
    let mut key: HKEY = null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut key) };
    (rc == 0).then_some(key)
}

/// Read a string value under HKCU. `value` "" reads the key's default value.
pub fn reg_get(subkey: &str, value: &str) -> Option<String> {
    let key = open_key(subkey, KEY_QUERY_VALUE)?;
    let name = wide(value);
    let mut out = None;
    unsafe {
        let mut ty = 0u32;
        let mut size = 0u32;
        if RegQueryValueExW(key, name.as_ptr(), null(), &mut ty, null_mut(), &mut size) == 0 {
            let mut buf = vec![0u16; size as usize / 2 + 1];
            if RegQueryValueExW(key, name.as_ptr(), null(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut size) == 0 {
                out = Some(from_wide(&buf));
            }
        }
        RegCloseKey(key);
    }
    out
}

pub fn reg_get_dword(subkey: &str, value: &str) -> Option<u32> {
    let sub = wide(subkey);
    let name = wide(value);
    let mut data = 0u32;
    let mut size = 4u32;
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, null_mut(), &mut data as *mut u32 as *mut _, &mut size)
    };
    (rc == 0).then_some(data)
}

/// Write a string value under HKCU. With `create` false the key must already exist.
pub fn reg_set(subkey: &str, value: &str, data: &str, create: bool) -> bool {
    let key = if create {
        let sub = wide(subkey);
        let mut key: HKEY = null_mut();
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER, sub.as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE, null(), &mut key, null_mut(),
            )
        };
        if rc != 0 {
            return false;
        }
        key
    } else {
        match open_key(subkey, KEY_SET_VALUE) {
            Some(k) => k,
            None => return false,
        }
    };
    let name = wide(value);
    let data = wide(data);
    let rc = unsafe {
        let rc = RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32);
        RegCloseKey(key);
        rc
    };
    rc == 0
}

pub fn reg_delete(subkey: &str, value: &str) {
    if let Some(key) = open_key(subkey, KEY_SET_VALUE) {
        let name = wide(value);
        unsafe {
            RegDeleteValueW(key, name.as_ptr());
            RegCloseKey(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{BOOL, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;

    #[test]
    fn start_keeps_the_keyboard_unless_the_shell_is_in_front() {
        // A deploy or script start: the user is typing in an editor or a chat.
        for app in ["Chrome_WidgetWin_1", "Notepad", "CASCADIA_HOSTING_WINDOW_CLASS", "ConsoleWindowClass"] {
            assert!(!start_may_take_foreground(app), "{app} must keep the keyboard");
        }
        // A double click on the exe: the desktop, a folder, the taskbar or Start is in front.
        for shell in ["", "Progman", "WorkerW", "CabinetWClass", "Shell_TrayWnd", "Windows.UI.Core.CoreWindow"] {
            assert!(start_may_take_foreground(shell), "{shell:?} should show the flyout");
        }
    }

    #[test]
    fn front_failure_names_the_cause() {
        let s = front_failure(false, false, false, "Chrome_WidgetWin_1", false);
        assert!(s.contains("hidden") && s.contains("not joined") && s.contains("Chrome_WidgetWin_1"), "{s}");
        let s = front_failure(true, true, true, "", true);
        assert!(s.contains("visible and minimized") && s.contains("joined") && s.contains("nothing") && s.contains("same app"), "{s}");
    }

    /// A hidden top-level window (an app closed to its tray icon) must be shown and
    /// raised by bring_forward; it is the alert-card failure seen on 11-22 and 11-23.
    /// Creates a window of its own and takes the foreground: needs a desktop session.
    #[test]
    #[ignore]
    fn bring_forward_shows_a_hidden_window() {
        let mut a = std::process::Command::new("winver.exe").spawn().unwrap();
        let h = window_of(a.id());
        assert!(!h.is_null(), "probe window did not appear");
        unsafe { ShowWindow(h, windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE) };
        assert_eq!(unsafe { IsWindowVisible(h) }, 0);
        let r = bring_forward_why(h);
        let vis = unsafe { IsWindowVisible(h) } != 0;
        let _ = a.kill();
        assert!(vis, "window stayed hidden");
        assert!(r.is_ok(), "{r:?}");
    }

    fn window_of(pid: u32) -> HWND {
        struct Find {
            pid: u32,
            hwnd: HWND,
        }
        unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
            let f = &mut *(lp as *mut Find);
            let mut p = 0u32;
            GetWindowThreadProcessId(h, &mut p);
            if p == f.pid && IsWindowVisible(h) != 0 {
                f.hwnd = h;
                return 0;
            }
            1
        }
        for _ in 0..50 {
            let mut f = Find { pid, hwnd: null_mut() };
            unsafe { EnumWindows(Some(each), &mut f as *mut Find as LPARAM) };
            if !f.hwnd.is_null() {
                return f.hwnd;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        null_mut()
    }

    /// Probe for the alert-card bug: this process is not in front, another window is,
    /// and bring_forward must still put the target in front. Needs a desktop session.
    #[test]
    #[ignore]
    fn bring_forward_from_background() {
        let mut a = std::process::Command::new("charmap.exe").spawn().unwrap();
        let mut b = std::process::Command::new("winver.exe").spawn().unwrap();
        let (ha, hb) = (window_of(a.id()), window_of(b.id()));
        assert!(!ha.is_null() && !hb.is_null(), "probe windows did not appear");
        let root = |h: HWND| unsafe { GetAncestor(h, GA_ROOT) };
        let front = || unsafe { root(GetForegroundWindow()) };
        assert!(bring_forward(hb), "could not put the first probe window in front");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(front(), root(hb));
        // Control: a bare SetForegroundWindow from this background process.
        unsafe { SetForegroundWindow(ha) };
        let bare = front() == root(ha);
        println!("bare SetForegroundWindow worked: {bare}");
        assert!(bring_forward(hb));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(front(), root(hb));
        let ok = bring_forward(ha);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let got = front() == root(ha);
        let _ = a.kill();
        let _ = b.kill();
        assert!(ok && got, "target did not come to the front");
    }
}
