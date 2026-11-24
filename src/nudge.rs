//! Keeping Claude's own usage check running while an account stays open.
//!
//! Claude Desktop checks plan usage every 15 minutes, but pauses that check when its
//! tray menu hasn't been opened for 24 hours, and only a right-click on its tray icon
//! counts as opening it (that right-click also fetches usage at once and saves the
//! time, so it lasts across restarts). The app sends
//! that right-click itself, only while he is away from the keyboard. So, once he has
//! been idle for a few minutes, an open account whose "tray opened" time is over 20
//! hours old, or whose usage numbers are over 2 hours stale (at most 6 nudges per account per day), gets the right-click message on
//! its tray icon; any menu it opens is closed and the window that was in front is put
//! back. Two minutes later the account's usage file must have a new sample: the
//! outcome is logged either way, and a message form that didn't work isn't used again
//! until the other one has had its turn.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW, GWL_STYLE,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_RBUTTONDOWN, WM_RBUTTONUP, WS_CAPTION,
};

use crate::config::Config;
use crate::{claude, log, procs, taskbar, time};

/// Away from the keyboard this long before anything is sent.
const IDLE_SECS: u64 = 300;
/// Renew Claude's "tray opened" time once it is this old (Claude pauses at 24 h).
const OPENED_MAX_MS: u64 = 20 * 3600 * 1000;
/// Usage this old means Claude's check has stopped (it runs every 15 minutes).
const STALE_SECS: u64 = 2 * 3600;
/// At most one try per account in this time.
const RETRY_SECS: u64 = 3600;
/// At most this many nudges per account in any 24 hours (2026-11-23).
const MAX_PER_DAY: usize = 6;
/// When the result is checked.
const VERIFY_SECS: u64 = 120;
/// Electron's tray icon callback message.
const NOTIFY_MSG: u32 = WM_APP + 1;

struct Sent {
    account: String,
    at: u64,
    form: usize,
}

struct Cleanup {
    pid: u32,
    before: Vec<HWND>,
    front: HWND,
}

thread_local! {
    static LAST_TRY: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
    /// Unix seconds of each account's nudges in the last 24 hours.
    static SENT_LOG: RefCell<HashMap<String, Vec<u64>>> = RefCell::new(HashMap::new());
    static PENDING: RefCell<Vec<Sent>> = const { RefCell::new(Vec::new()) };
    /// Which message form to send: 0 = (icon id, mouse message), 1 = the newer
    /// (mouse message | icon id << 16) form.
    static FORM: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
    static CLEANUP: RefCell<Option<Cleanup>> = const { RefCell::new(None) };
}

/// Every minute (tray timer): check earlier tries, then maybe nudge one account.
pub fn tick(cfg: &Config) {
    verify(cfg);
    if !wanted_now(&cfg.usage_nudge) || crate::win::idle_secs() < IDLE_SECS || CLEANUP.with(|c| c.borrow().is_some()) {
        return;
    }
    // No input is not the same as away: never during a full-screen app, a presentation
    // or Do not disturb (a video, a game, a call shared on screen).
    if !crate::sound::may_sound() {
        return;
    }
    let mains = procs::main_pids(cfg, &procs::list());
    let now_ms = time::now_ms();
    for a in &cfg.accounts {
        let Some(&pid) = mains.get(&a.name) else { continue };
        if matches!(claude::org(cfg, a), claude::Org::SignedOut) {
            continue; // nothing to refresh
        }
        let tried = LAST_TRY.with(|t| t.borrow().get(&a.name).is_some_and(|at| at.elapsed().as_secs() < RETRY_SECS));
        if tried {
            continue;
        }
        let opened = claude::tray_opened_ms(cfg, a);
        let latest = claude::latest_sample(cfg, a);
        let (old_open, stale) = reasons(now_ms, opened, time::now(), latest);
        if !old_open && !stale {
            continue;
        }
        if !SENT_LOG.with(|l| take_slot(l.borrow_mut().entry(a.name.clone()).or_default(), time::now())) {
            continue;
        }
        LAST_TRY.with(|t| t.borrow_mut().insert(a.name.clone(), Instant::now()));
        let why = match (old_open, stale) {
            (true, true) => "tray last opened over 20 h ago, usage stale",
            (true, false) => "tray last opened over 20 h ago",
            _ => "usage stale",
        };
        let form = FORM.with(|f| f.borrow().get(&a.name).copied().unwrap_or(0));
        if send(pid, form) {
            log(&format!("usage nudge for {} ({why}): right-click sent to its tray icon", a.name));
            PENDING.with(|p| p.borrow_mut().push(Sent { account: a.name.clone(), at: time::now(), form }));
        } else {
            log(&format!("usage nudge for {}: its tray icon wasn't found", a.name));
        }
        return; // one account a minute
    }
}

/// (tray last opened over 20 h ago, usage over 2 h stale).
fn reasons(now_ms: u64, opened_ms: Option<u64>, now_s: u64, latest_s: Option<u64>) -> (bool, bool) {
    let old_open = opened_ms.map_or(true, |o| now_ms.saturating_sub(o) > OPENED_MAX_MS);
    let stale = latest_s.map_or(true, |t| now_s.saturating_sub(t) > STALE_SECS);
    (old_open, stale)
}

/// Record a nudge at `now` unless 6 already happened in the last 24 hours.
fn take_slot(sent: &mut Vec<u64>, now: u64) -> bool {
    sent.retain(|&t| now.saturating_sub(t) < 86_400);
    if sent.len() >= MAX_PER_DAY {
        return false;
    }
    sent.push(now);
    true
}

/// The setting allows a nudge now: "away" any time; a time like "3:00 AM" during the
/// hour after it; "" never.
fn wanted_now(setting: &str) -> bool {
    match setting.trim() {
        "" => false,
        "away" => true,
        at => time::next_daily(at).is_some_and(|next| next.saturating_sub(time::now()) >= 23 * 3600),
    }
}

/// Two minutes on: did Claude save a new usage sample?
fn verify(cfg: &Config) {
    let due: Vec<Sent> = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let (due, wait) = p.drain(..).partition(|s| time::now() >= s.at + VERIFY_SECS);
        *p = wait;
        due
    });
    for s in due {
        let Some(a) = cfg.find(&s.account) else { continue };
        if claude::latest_sample(cfg, a).is_some_and(|t| t + 5 >= s.at) {
            log(&format!("usage nudge for {} worked: new usage sample (message form {})", s.account, s.form));
        } else {
            let next = (s.form + 1) % 2;
            FORM.with(|f| f.borrow_mut().insert(s.account.clone(), next)); // that account only
            log(&format!("usage nudge for {}: no new usage sample after {} s (message form {}); next try uses form {next}", s.account, VERIFY_SECS, s.form));
        }
    }
}

/// Post the right-click to the process's tray icon. Clean-up runs from `after_send`.
fn send(pid: u32, form: usize) -> bool {
    let Some((host, id)) = taskbar::tray_slot(pid) else { return false };
    let front = unsafe { GetForegroundWindow() };
    let before = visible_windows(pid);
    unsafe {
        if form == 0 {
            PostMessageW(host, NOTIFY_MSG, id as usize, WM_RBUTTONDOWN as LPARAM);
            PostMessageW(host, NOTIFY_MSG, id as usize, WM_RBUTTONUP as LPARAM);
        } else {
            PostMessageW(host, NOTIFY_MSG, 0, ((id << 16) | WM_CONTEXTMENU) as LPARAM);
        }
    }
    CLEANUP.with(|c| *c.borrow_mut() = Some(Cleanup { pid, before, front }));
    true
}

/// A moment after sending (tray timer): close a menu the right-click opened and put
/// back the window that was in front.
pub fn after_send() {
    let Some(c) = CLEANUP.with(|c| c.borrow_mut().take()) else { return };
    let mut closed = 0;
    for h in visible_windows(c.pid).into_iter().filter(|h| !c.before.contains(h)) {
        let style = unsafe { GetWindowLongW(h, GWL_STYLE) } as u32;
        let mut r = unsafe { std::mem::zeroed() };
        unsafe { GetWindowRect(h, &mut r) };
        let small = (r.right - r.left) < 700 && (r.bottom - r.top) < 900;
        // A menu: a new small window with no title bar. Never the account's own window.
        if style & WS_CAPTION != WS_CAPTION && small {
            unsafe { PostMessageW(h, WM_CLOSE, 0, 0) };
            closed += 1;
        }
    }
    let moved = !c.front.is_null() && unsafe { IsWindow(c.front) } != 0 && unsafe { GetForegroundWindow() } != c.front;
    if moved {
        crate::win::bring_forward(c.front);
    }
    if closed > 0 || moved {
        log(&format!("usage nudge: closed {closed} menu window(s){}", if moved { ", put the front window back" } else { "" }));
    }
}

pub fn cleanup_pending() -> bool {
    CLEANUP.with(|c| c.borrow().is_some())
}

fn visible_windows(pid: u32) -> Vec<HWND> {
    struct Find {
        pid: u32,
        out: Vec<HWND>,
    }
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> i32 {
        let f = &mut *(lp as *mut Find);
        let mut p = 0u32;
        GetWindowThreadProcessId(h, &mut p);
        if p == f.pid && IsWindowVisible(h) != 0 {
            f.out.push(h);
        }
        1
    }
    let mut f = Find { pid, out: Vec::new() };
    unsafe { EnumWindows(Some(each), &mut f as *mut Find as LPARAM) };
    f.out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_S: u64 = 2_000_000;
    const NOW_MS: u64 = NOW_S * 1000;

    #[test]
    fn stale_over_2h_nudges() {
        let (_, stale) = reasons(NOW_MS, Some(NOW_MS), NOW_S, Some(NOW_S - 2 * 3600 - 60));
        assert!(stale);
    }

    #[test]
    fn stale_1h_does_not() {
        assert_eq!(reasons(NOW_MS, Some(NOW_MS), NOW_S, Some(NOW_S - 3600)), (false, false));
    }

    #[test]
    fn old_tray_open_still_nudges() {
        assert_eq!(reasons(NOW_MS, Some(NOW_MS - 21 * 3600 * 1000), NOW_S, Some(NOW_S)), (true, false));
    }

    #[test]
    fn seventh_in_a_day_is_refused() {
        let mut sent = Vec::new();
        for i in 0..6 {
            assert!(take_slot(&mut sent, NOW_S + i * 3600));
        }
        assert!(!take_slot(&mut sent, NOW_S + 7 * 3600));
        // 24 h after the first, a slot frees up.
        assert!(take_slot(&mut sent, NOW_S + 86_400 + 1));
    }

    #[test]
    fn away_only_rule_kept() {
        assert!(IDLE_SECS >= 300);
        assert!(wanted_now("away"));
        assert!(!wanted_now(""));
    }
}
