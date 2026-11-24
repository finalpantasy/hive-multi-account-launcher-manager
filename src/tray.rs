//! The tray icon, its right-click menu, the Google Drive wait and the
//! keep-running watchdog. Nothing polls: the app wakes on tray clicks, the
//! hotkey, and file change notifications from watch.rs.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::claude;
use crate::card;
use crate::chatmove;
use crate::codex;
use crate::doctor;
use crate::events;
use crate::inbox;
use crate::procs;
use crate::time;
use crate::config::{Account, Config};
use crate::launcher::{self, WM_NOTE};
use crate::layout;
use crate::tags;
use crate::ui;
use crate::watch::{self, CHANGED_CONFIG, CHANGED_EVENT, CHANGED_LOCK, CHANGED_USAGE, WM_WATCH};
use crate::win::{fill, msg_box, reg_delete, reg_get, reg_set, set_clipboard, shell_open, wide};
use crate::{config_path, log, log_path};

pub const CLASS: &str = "HiveTray";
const WM_TRAY: u32 = WM_APP + 1;
pub const WM_SHOWMENU: u32 = WM_APP + 2;
const WM_DRIVE: u32 = WM_APP + 3;
pub const WM_SHOWADD: u32 = WM_APP + 5;
/// `--dashboard` from another process: open the usage dashboard.
pub const WM_SHOWDASH: u32 = WM_APP + 40;
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;
const TIMER_ICON: usize = 2;
const TIMER_DEBOUNCE: usize = 3;
const TIMER_WATCHDOG: usize = 4;
const TIMER_NAG: usize = 5;
const TIMER_SUMMARY: usize = 6;
const TIMER_DOCTOR: usize = 7;
/// After opening a Codex window: look for it a few times (Codex folders are not watched).
const TIMER_CODEX_FIND: usize = 8;
/// After opening Codex from an alert: bring its window forward once it shows.
const TIMER_CODEX_FRONT: usize = 9;
/// Keeping Claude's usage check running (nudge.rs): every minute, and a moment after a send.
const TIMER_NUDGE: usize = 10;
const TIMER_NUDGE_CLEAN: usize = 11;
/// Close chat cards whose chat you opened yourself.
const TIMER_CARD_SEEN: usize = 12;
/// Add new history to the shared memory vault's digests (no AI involved).
const TIMER_MEMORY: usize = 13;
/// Look again at goal chat events held for the goal check's verdict.
const TIMER_GOAL: usize = 14;
static CODEX_FIND_LEFT: AtomicU32 = AtomicU32::new(0);
const WM_POWERBROADCAST_: u32 = 0x0218;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const HOTKEY_ID: i32 = 1;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Hive";
/// The startup entry's name before the app was renamed.
const OLD_RUN_VALUES: [&str; 2] = ["Switchboard", "ClaudeAccounts"];

const ID_SKIP_WAIT: usize = 10;
const ID_OPEN_ALL: usize = 11;
const ID_OPEN_STARTUP: usize = 12;
const ID_ADD: usize = 13;
const ID_START_WIN: usize = 14;
const ID_WAIT_DRIVE: usize = 15;
const ID_EDIT_CFG: usize = 16;
const ID_OPEN_ROOT: usize = 17;
const ID_VIEW_LOG: usize = 18;
const ID_HELP: usize = 19;
const ID_EXIT: usize = 20;
const ID_SHOW: usize = 21;
const ID_CODEX_OPEN: usize = 22;
const ID_CODEX_FOLDER: usize = 23;
const ID_CODEX_TOGGLE: usize = 24;
const ID_SUMMARY: usize = 25;
const ID_TAGS: usize = 26;
const ID_LAYOUT_SAVE: usize = 27;
const ID_LAYOUT_RESTORE: usize = 28;
const ID_LAYOUT_AUTO: usize = 29;
const ID_NOTIFY: usize = 30;
const ID_NOTIFY_WAITING: usize = 31;
const ID_TASKBAR: usize = 32;
const ID_CHECK: usize = 33;
const ID_CODEX_ADD: usize = 34;
const ID_CODEX_REMOVE: usize = 35;
const ID_CODEX_STARTUP: usize = 36;
const ID_DASH: usize = 37;
const ID_MEMORY: usize = 38;
const ID_OBSIDIAN: usize = 39;
/// "Use next" threshold choices: ID_USE_NEXT + index into USE_NEXT_CHOICES.
const ID_USE_NEXT: usize = 40;
const ID_MOVE_UNDO: usize = 41;
/// Move chats: ID_MOVE + from * 16 + to (account positions).
const ID_MOVE: usize = 5000;
const USE_NEXT_CHOICES: [u8; 4] = [90, 95, 97, 99];
const ID_OPEN: usize = 1000;
const ID_ACT: usize = 2000;
const ACTS: usize = 20;

// Per-account actions.
const A_OPEN: usize = 0;
const A_EMAIL: usize = 1;
const A_CHROME: usize = 2;
const A_EDIT: usize = 3;
const A_FOLDER: usize = 4;
const A_STARTUP: usize = 5;
const A_KEEP: usize = 6;
const A_UP: usize = 7;
const A_DOWN: usize = 8;
const A_REMOVE: usize = 9;
const A_QUIT: usize = 10;
const A_TIDY: usize = 11;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static SKIP_WAIT: AtomicBool = AtomicBool::new(false);

pub enum Start {
    Normal,
    Startup,
    AddDialog,
}

struct App {
    hwnd: HWND,
    icon: HICON,
    icon_added: bool,
    cfg: Config,
    cfg_mtime: Option<SystemTime>,
    cfg_broken: bool,
    waiting_since: Option<Instant>,
    seen_running: HashSet<String>,
    closed_at: HashMap<String, Instant>,
    /// When each chat last finished a turn (for dropping idle nudges).
    finished_at: HashMap<String, Instant>,
    /// Goal chat events waiting for the goal check to say whether the chat goes on by itself.
    goal_hold: events::GoalHold,
    /// Accounts being quit on purpose: Keep running leaves them closed.
    quitting: HashMap<String, Instant>,
    last_relaunch: HashMap<String, Instant>,
    shutting_down: bool,
    menu_names: Vec<String>,
    pending: usize,
    hotkey: String,
    /// Shortcuts as last registered: (key, combo). Re-registered only when this changes.
    hotkeys_on: Vec<(String, String)>,
    /// Registered extra shortcuts: hotkey id -> key ("account:..", "use_next", "open_all").
    hotkey_ids: HashMap<i32, String>,
    /// Why a shortcut isn't working, by key.
    hotkey_errors: HashMap<String, String>,
    /// Last seen (week %, 5h %) per account, for limit and reset alerts.
    levels: HashMap<String, (u8, u8)>,
    best: Option<String>,
    /// Account to open if the latest notification is clicked (out-of-usage switch).
    pending_open: Option<(String, Instant)>,
    /// Chat to open if the latest notification is clicked: (account, chat id, title, when).
    pending_chat: Option<(String, String, String, Instant)>,
    /// Where we have read up to in the chat events file.
    events_at: u64,
    /// Last alert per chat (or account): (kind, when), to collapse repeats.
    alerted: HashMap<String, (String, Instant)>,
    balloon_at: std::cell::Cell<Option<Instant>>,
    /// Day (y, m, d) the daily summary last showed.
    summary_day: Option<(u16, u16, u16)>,
    /// Latest usage per Claude account, with whether it is a work account (for "Continue in").
    usage_now: Vec<Standing>,
    inbox_watch: card::InboxWatch,
    inbox_checked: Option<Instant>,
    /// The coordinator chat found last, and when (cached for 60 s).
    coordinator_seen: std::cell::RefCell<Option<(Instant, Option<(String, String, String)>)>>,
    /// What happened while you were idle, for the "While you were away" card.
    away: card::Away,
    /// What the context alarm and worker watchdog already raised (kept in hive-alarms.json).
    alarms: card::AlarmState,
    /// When the last alarm scan started (the first one runs two minutes after the tray starts).
    alarm_checked: Instant,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` on the app state. Returns None if the state is busy (re-entrant call).
fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn mtime() -> Option<SystemTime> {
    std::fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

fn exe_path() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

pub fn startup_enabled() -> bool {
    reg_get(RUN_KEY, RUN_VALUE).is_some() || OLD_RUN_VALUES.iter().any(|v| reg_get(RUN_KEY, v).is_some())
}

/// An old "Switchboard" or "ClaudeAccounts" startup entry becomes a "Hive" one for this exe.
fn move_startup_entry() {
    if OLD_RUN_VALUES.iter().any(|v| reg_get(RUN_KEY, v).is_some()) {
        reg_set(RUN_KEY, RUN_VALUE, &format!("\"{}\" --startup", exe_path()), true);
        for v in OLD_RUN_VALUES {
            reg_delete(RUN_KEY, v);
        }
        log("startup entry moved to Hive");
    }
}

pub fn set_startup(on: bool) {
    if on {
        reg_set(RUN_KEY, RUN_VALUE, &format!("\"{}\" --startup", exe_path()), true);
    } else {
        reg_delete(RUN_KEY, RUN_VALUE);
        for v in OLD_RUN_VALUES {
            reg_delete(RUN_KEY, v);
        }
    }
}

/// The tray mark: the full-colour logo at the notification area's size.
fn tray_icon() -> Option<HICON> {
    let res = 1usize;
    unsafe {
        let dpi = GetDpiForSystem();
        let (cx, cy) = (GetSystemMetricsForDpi(SM_CXSMICON, dpi), GetSystemMetricsForDpi(SM_CYSMICON, dpi));
        let h = LoadImageW(GetModuleHandleW(null()), res as *const u16, IMAGE_ICON, cx, cy, 0) as HICON;
        (!h.is_null()).then_some(h)
    }
}

/// Every shortcut the Shortcuts page lists: (key, label, section).
fn hotkey_targets(cfg: &Config) -> Vec<(String, String, u8)> {
    let mut v = vec![("show".to_string(), "Show this window".to_string(), 0u8)];
    for a in &cfg.accounts {
        v.push((format!("account:{}", a.name), a.name.clone(), 1));
    }
    if cfg.codex.enabled || !cfg.codex.accounts.is_empty() {
        v.push(("account:Codex".to_string(), "Codex".to_string(), 1));
        for a in &cfg.codex.accounts {
            v.push((format!("account:{}", a.name), a.name.clone(), 1));
        }
    }
    v.push(("use_next".to_string(), "Use-next account".to_string(), 2));
    v.push(("open_all".to_string(), "Open all accounts".to_string(), 2));
    v
}

/// Set (or clear, with "") one shortcut.
pub fn set_hotkey(key: &str, combo: &str) {
    let combo = combo.trim().to_string();
    update_settings(|c| {
        if key == "show" {
            c.hotkey = combo.clone();
        } else if combo.is_empty() {
            c.hotkeys.remove(key);
        } else {
            c.hotkeys.insert(key.to_string(), combo.clone());
        }
    });
}

/// A shortcut was pressed (not the flyout's own).
fn run_hotkey(id: i32) {
    let Some(key) = with(|a| a.hotkey_ids.get(&id).cloned()).flatten() else { return };
    log(&format!("shortcut: {key}"));
    match key.as_str() {
        "open_all" => open_all(),
        "use_next" => match with(|a| a.best.clone()).flatten() {
            Some(name) => focus_or_open(&name),
            None => balloon("No account to use next", "Every account is past the use-next line, or usage isn't known yet."),
        },
        k => {
            if let Some(name) = k.strip_prefix("account:") {
                focus_or_open(name);
            }
        }
    }
}

/// Bring an account's window to the front, or open it when it isn't showing.
fn focus_or_open(name: &str) {
    let Some(cfg) = with(|a| a.cfg.clone()) else { return };
    let shown = tags::account_windows(&cfg)
        .into_iter()
        .find(|(n, h)| n == name && unsafe { IsWindowVisible(*h) } != 0)
        .map(|(_, h)| h);
    match shown {
        Some(h) => {
            crate::win::bring_forward(h);
        }
        None => open_account(name),
    }
}

/// "Ctrl+Alt+Shift+C" -> (modifiers, virtual key)
fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "windows" => mods |= MOD_WIN,
            "space" => vk = Some(0x20),
            k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => vk = Some(k.to_ascii_uppercase().as_bytes()[0] as u32),
            k if k.starts_with('f') => vk = k[1..].parse::<u32>().ok().filter(|n| (1..=24).contains(n)).map(|n| 0x6F + n),
            _ => return None,
        }
    }
    vk.map(|v| (mods, v))
}

fn account_dirs(cfg: &Config) -> Vec<PathBuf> {
    let mut dirs = vec![crate::exe_dir()];
    for a in &cfg.accounts {
        for d in claude::profile_dirs(cfg, a) {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs
}

impl App {
    /// Re-read the settings file if it changed. Returns true when it was reloaded.
    fn reload_if_changed(&mut self) -> bool {
        let m = mtime();
        if m == self.cfg_mtime {
            return false;
        }
        self.cfg_mtime = m;
        match Config::load(&config_path()) {
            Ok(c) => {
                self.cfg = c;
                self.cfg_broken = false;
                self.settings_applied();
                true
            }
            Err(e) => {
                self.cfg_broken = true;
                log(&format!("settings file not loaded: {e}"));
                self.alert("Settings file has an error", "Still using the last good settings. See the log for details.", None, card::Action::Show);
                false
            }
        }
    }

    /// Settings changed: watch the right folders and use the right hotkey.
    fn settings_applied(&mut self) {
        watch::set_dirs(account_dirs(&self.cfg));
        if self.hotkey != self.cfg.hotkey {
            unsafe { UnregisterHotKey(self.hwnd, HOTKEY_ID) };
            let old = std::mem::replace(&mut self.hotkey, self.cfg.hotkey.clone());
            self.hotkey_errors.remove("show");
            if let Some((mods, vk)) = parse_hotkey(&self.hotkey) {
                if unsafe { RegisterHotKey(self.hwnd, HOTKEY_ID, mods, vk) } == 0 {
                    log(&format!("hotkey {} is taken by another app", self.hotkey));
                    self.hotkey_errors.insert("show".into(), "Taken by another app".into());
                    // A new shortcut that another app owns: keep the one that worked.
                    if let Some((m, v)) = parse_hotkey(&old) {
                        if unsafe { RegisterHotKey(self.hwnd, HOTKEY_ID, m, v) } != 0 {
                            let taken = std::mem::replace(&mut self.hotkey, old.clone());
                            self.cfg.hotkey = old.clone();
                            let _ = self.save();
                            self.hotkey_errors.remove("show");
                            self.alert("Shortcut not changed", &format!("{taken} is already used by another app. Still using {old}."), None, card::Action::Show);
                        }
                    }
                }
            } else if !self.hotkey.trim().is_empty() {
                log(&format!("hotkey \"{}\" not understood", self.hotkey));
                self.hotkey_errors.insert("show".into(), "Not a key this app can use".into());
            }
        }
        self.register_hotkeys();
        self.arm_summary();
        tags::sync(&self.cfg);
    }

    /// The other shortcuts (the flyout's own is HOTKEY_ID above): register what is
    /// set, and note any that can't work (taken by another app, a duplicate, unknown).
    fn register_hotkeys(&mut self) {
        let want: Vec<(String, String)> = hotkey_targets(&self.cfg)
            .into_iter()
            .filter(|(k, _, _)| k != "show")
            .filter_map(|(k, _, _)| self.cfg.hotkeys.get(&k).filter(|c| !c.trim().is_empty()).map(|c| (k, c.clone())))
            .collect();
        let show = ("show".to_string(), self.hotkey.clone());
        let mut state = want.clone();
        state.push(show);
        if state == self.hotkeys_on {
            return;
        }
        for id in self.hotkey_ids.keys() {
            unsafe { UnregisterHotKey(self.hwnd, *id) };
        }
        self.hotkey_ids.clear();
        self.hotkey_errors.retain(|k, _| k == "show"); // the flyout's own is checked above
        let labels: HashMap<String, String> = hotkey_targets(&self.cfg).into_iter().map(|(k, l, _)| (k, l)).collect();
        let mut taken: Vec<((u32, u32), String)> = Vec::new();
        if let Some(p) = parse_hotkey(&self.hotkey) {
            taken.push((p, "Show this window".to_string()));
        }
        for (i, (key, combo)) in want.iter().enumerate() {
            let Some(p) = parse_hotkey(combo) else {
                self.hotkey_errors.insert(key.clone(), "Not a key this app can use".into());
                log(&format!("hotkey {combo} for {key} not understood"));
                continue;
            };
            let strip = |p: (u32, u32)| (p.0 & !MOD_NOREPEAT, p.1);
            if let Some((_, other)) = taken.iter().find(|(q, _)| strip(*q) == strip(p)) {
                self.hotkey_errors.insert(key.clone(), format!("Same keys as {other}"));
                log(&format!("hotkey {combo} for {key} is already used for {other}"));
                continue;
            }
            let id = 100 + i as i32;
            if unsafe { RegisterHotKey(self.hwnd, id, p.0, p.1) } == 0 {
                self.hotkey_errors.insert(key.clone(), "Taken by another app".into());
                log(&format!("hotkey {combo} for {key} is taken by another app"));
                continue;
            }
            self.hotkey_ids.insert(id, key.clone());
            taken.push((p, labels.get(key).cloned().unwrap_or_default()));
        }
        self.hotkeys_on = state;
    }

    fn arm_summary(&self) {
        match time::next_daily(&self.cfg.daily_summary) {
            Some(at) => {
                let ms = (at.saturating_sub(time::now()) * 1000 + 500).min(u32::MAX as u64) as u32;
                unsafe { SetTimer(self.hwnd, TIMER_SUMMARY, ms, None) };
            }
            None => unsafe {
                KillTimer(self.hwnd, TIMER_SUMMARY);
            },
        }
    }

    fn today() -> (u16, u16, u16) {
        let st = time::local(time::now());
        (st.wYear, st.wMonth, st.wDay)
    }

    /// One notification with every account's weekly usage and reset date.
    fn show_summary(&mut self) {
        let mut lines = Vec::new();
        for a in &self.cfg.accounts {
            let org = match claude::org(&self.cfg, a) {
                claude::Org::Id(id) => Some(id),
                _ => None,
            };
            if let Some(u) = claude::usage(&self.cfg, a, org.as_deref()) {
                let reset = u.week_reset.map(|(at, unc)| format!(", resets {}", time::when(at, unc))).unwrap_or_default();
                lines.push(format!("{}: {}%{reset}", a.name, u.week));
            }
        }
        if let Some(cx) = codex::read(&self.cfg) {
            if let Some(w) = cx.windows.iter().max_by_key(|w| w.minutes) {
                lines.push(format!("Codex: {}%, resets {}", w.pct, time::when(w.resets_at, 0)));
            }
        }
        self.summary_day = Some(Self::today());
        if !lines.is_empty() {
            log("daily summary shown");
            self.alert("Claude usage today", &lines.join("\n"), None, card::Action::Show);
        }
    }

    /// After sleep: show a summary that was due in the last 4 hours and missed.
    fn catch_up_summary(&mut self) {
        if let Some(next) = time::next_daily(&self.cfg.daily_summary) {
            let due = next.saturating_sub(time::DAY);
            let now = time::now();
            if due <= now && now - due < 4 * time::HOUR && self.summary_day != Some(Self::today()) {
                self.show_summary();
            }
        }
        self.arm_summary();
    }

    fn take_pending_chat(&mut self) -> Option<(String, String, String)> {
        let (acc, id, title, at) = self.pending_chat.take()?;
        let latest = self.balloon_at.get().map_or(true, |b| at >= b);
        (latest && at.elapsed() < Duration::from_secs(1800)).then_some((acc, id, title))
    }

    /// New lines in the chat events file: one alert for the newest worth showing.
    fn events_changed(&mut self) {
        if crate::demo::on() {
            return; // no real alerts while showing sample data
        }
        let (events, end) = events::read_since(self.events_at);
        self.events_at = end;
        if !self.cfg.alerts_finished() && !self.cfg.alerts_needs_me() && !self.cfg.codex.alerts {
            return;
        }
        for ev in events {
            self.chat_event(ev, false);
        }
    }

    /// Tray start: bring back the questions and needs-you cards a restart closed (once each).
    fn replay_unanswered(&mut self) {
        if crate::demo::on() || !self.cfg.alerts_needs_me() {
            return;
        }
        let events = events::read_recent();
        // Only what you closed yourself is remembered; a shown card is not, so it returns every restart.
        let replayed = events::load_replayed();
        let cfg = self.cfg.clone();
        let opened = |ev: &events::Event| {
            cfg.find(&ev.account).and_then(|a| claude::focused_at(&cfg, a, &ev.session)).is_some_and(|ms| ms / 1000 > ev.at)
        };
        let now = time::now();
        let mut due = events::unanswered(&events, now, &replayed, opened);
        // A goal chat's question that the goal answered by itself (or has not settled) is not waiting.
        due.retain(|ev| events::goal_shown(ev, now));
        if due.is_empty() {
            return;
        }
        for ev in due {
            log(&format!("re-showing {} from {} ({})", ev.kind, ev.account, events::chat_key(&ev)));
            self.chat_event(ev, true);
        }
    }

    /// A chat event. One from a goal chat waits for the goal check first: when the goal
    /// keeps the chat going by itself, there is nothing to tell you.
    fn chat_event(&mut self, ev: events::Event, replay: bool) {
        let now = time::now();
        if !replay && now.saturating_sub(ev.at) > 300 {
            return; // stale (the tray was off)
        }
        if replay {
            self.goal_hold.forget(&ev);
        } else {
            match self.goal_hold.offer(&ev, now) {
                events::Offer::Held => {
                    unsafe { SetTimer(self.hwnd, TIMER_GOAL, 2000, None) };
                    return;
                }
                events::Offer::Dropped => {
                    log(&format!("goal chat continued, no card: {} from {}", ev.kind, if ev.account.is_empty() { "Claude" } else { &ev.account }));
                    return;
                }
                events::Offer::Pass => {}
            }
        }
        self.chat_event_now(ev, replay);
    }

    /// Held goal events whose verdict has come, or whose 90 s ran out.
    fn release_goal_held(&mut self) {
        for (ev, dropped) in self.goal_hold.due(time::now()) {
            if dropped {
                log(&format!("goal chat continued, no card: {} from {}", ev.kind, if ev.account.is_empty() { "Claude" } else { &ev.account }));
            } else {
                self.chat_event_now(ev, false);
            }
        }
        if self.goal_hold.is_empty() {
            unsafe { KillTimer(self.hwnd, TIMER_GOAL) };
        }
    }

    fn chat_event_now(&mut self, mut ev: events::Event, replay: bool) {
        // "ask": a finished turn whose reply ends with a question for you. With
        // "needs me" alerts off it is just a finished turn.
        if ev.kind == "ask" && !self.cfg.alerts_needs_me() {
            ev.kind = "stop".into();
        }
        // One chat: its Code tab or Codex id, else Claude Code's own session id; without
        // either (older Codex), its account and folder, so two chats of one account
        // asking at once are two alerts.
        let key = events::chat_key(&ev);
        if ev.kind == "stop" || ev.kind == "ask" {
            self.finished_at.insert(key.clone(), Instant::now());
        }
        if ev.kind == "stop" || ev.kind == "codex" {
            // The chat ran a turn: a mute set while it waited ends (even if no card follows).
            card::chat_ran(&ev.account, if !ev.session.is_empty() { &ev.session } else { &ev.chat });
        }
        let waiting = ev.kind == "notify" || ev.kind == "ask";
        // Codex only reports finished turns. With Codex alerts on, this app replaces
        // Codex's own turn notifications, so a finished Codex turn always gets a card
        // (2026-10-15), whatever "A chat finishes" says for Claude.
        let codex_turn = ev.kind == "codex" && self.cfg.codex.alerts;
        let wanted = if waiting { self.cfg.alerts_needs_me() } else { codex_turn || self.cfg.alerts_finished() };
        if !wanted {
            return;
        }
        // Claude's "waiting for your input" nudge a minute after a turn ended is not news.
        let idle_nudge = ev.kind == "notify" && ev.message.to_ascii_lowercase().contains("waiting for your input");
        if idle_nudge && self.finished_at.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
            return;
        }
        // Nor when the chat's card is still up: it already says what the chat wants.
        let card_chat = if !ev.session.is_empty() { ev.session.clone() } else { ev.chat.clone() };
        if idle_nudge && card::chat_kind(&ev.account, &card_chat).is_some() {
            return;
        }
        // One card per chat: a chat that already has one gets it updated in place.
        let carded = card::chat_kind(&ev.account, &card_chat);
        // One alert of a kind per chat per 30 s (its card, if up, still takes the newest words).
        if carded.is_none() && self.alerted.get(&key).is_some_and(|(kind, at)| *kind == ev.kind && at.elapsed() < Duration::from_secs(30)) {
            return;
        }
        if !replay {
            let chat_id = if !ev.session.is_empty() { ev.session.as_str() } else if !ev.chat.is_empty() { ev.chat.as_str() } else { ev.cwd.as_str() };
            self.away.chat(&format!("{}\u{1}{chat_id}", ev.account), waiting, crate::win::idle_secs());
        }
        // The account's window is in front. A finished turn is not news then, but a
        // question may come from another chat in that window (its title never says
        // which chat is open), so it still shows: quietly while you are at the keyboard.
        let in_front = !ev.account.is_empty() && ev.account != "Codex" && account_in_front(&self.cfg, &ev.account);
        if in_front && !waiting {
            return;
        }
        // An update to a chat's card sounds only when what happened changed (finished became needs-you).
        let want_chime = !(in_front && crate::win::idle_secs() < 60) && carded.as_deref() != Some(ev.kind.as_str());
        // Snoozed or muted chats stay quiet; a chat chimes at most once per kind per 10 minutes.
        let chime = match card::gate_alert(&ev.account, &card_chat, &ev.kind, want_chime) {
            card::Verdict::Hidden { log: say } => {
                if say {
                    log(&format!("chat alert held back (snoozed or muted): {} from {}", ev.kind, if ev.account.is_empty() { "Claude" } else { &ev.account }));
                }
                return;
            }
            card::Verdict::Show { chime } => chime,
        };
        self.alerted.insert(key, (ev.kind.clone(), Instant::now()));
        let who = if ev.account.is_empty() { "Claude".to_string() } else { ev.account.clone() };
        let title = events::chat_title(&self.cfg, &ev.account, &ev.session)
            .or_else(|| std::path::Path::new(&ev.cwd).file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "a chat".into());
        // The chat's name on the first line, then what it said as plain text.
        let short = events::cut_at_word(&title, 48);
        let said = events::card_text(&ev.message, 170);
        let line2 = |none: &str| if said.is_empty() { none.to_string() } else { said.clone() };
        let (head, body) = match ev.kind.as_str() {
            "notify" => (format!("{who} needs you"), line2("Waiting for you.")),
            "ask" => (format!("{who} has a question"), line2("Asked you something.")),
            "codex" => (format!("{who} finished"), line2("Done.")),
            _ => (format!("{who} finished"), "Done.".to_string()),
        };
        let body = format!("{short}\n{body}");
        log(&format!("chat alert: {head} ({short})"));
        let badge = self.badge_for(&ev.account);
        let action = match self.continue_choice(&ev, &card_chat) {
            Some((target, cli)) => card::Action::ContinueIn(ev.account.clone(), ev.session.clone(), title, target, cli),
            None => card::Action::Chat(ev.account.clone(), ev.session.clone(), title),
        };
        self.alert_in(&head, &body, badge, action, chime, Some((card_chat, ev.kind.clone())));
    }

    /// The account this chat's alert card offers to continue in: set when its own account is at
    /// 95% or more of the 5-hour window or the week. Also the chat's Claude Code id to resume.
    fn continue_choice(&self, ev: &events::Event, card_chat: &str) -> Option<(String, String)> {
        if ev.account.is_empty() || ev.account == "Codex" || self.usage_now.is_empty() {
            return None;
        }
        let target = continue_target(&ev.account, &self.usage_now, self.cfg.use_next_below)?;
        let cli = if is_cli_id(&ev.chat) {
            ev.chat.clone()
        } else {
            let acc = self.cfg.find(&ev.account)?;
            claude::chats(&self.cfg, acc).into_iter().find(|c| c.id == card_chat)?.cli
        };
        is_cli_id(&cli).then_some((target, cli))
    }

    /// Every couple of seconds: new inbox posts (at most every 30 s, and a size and time check
    /// when nothing changed), and the "While you were away" card.
    fn poll_inbox_and_away(&mut self) {
        let idle = crate::win::idle_secs();
        if self.inbox_checked.is_none_or(|t| t.elapsed() >= Duration::from_secs(30)) {
            self.inbox_checked = Some(Instant::now());
            let found = self.inbox_watch.poll(&inbox::dir(), time::now());
            self.away.posts(found.fresh, idle);
            for post in found.raise {
                self.inbox_card(&post);
            }
        }
        if let Some((f, w, i)) = self.away.tick(idle) {
            log(&format!("away summary: {f} finished, {w} waiting, {i} inbox posts"));
            self.alert_with("While you were away", &card::away_text(f, w, i), None, card::Action::Away, false);
        }
    }

    /// One card for a blocked or question post, merged per recipient, chiming once per 10 minutes.
    fn inbox_card(&mut self, post: &card::InboxPost) {
        let coord = if post.to == "coordinator" { self.coordinator_chat() } else { None };
        let account = coord.as_ref().map(|c| c.0.clone()).unwrap_or_default();
        let chat = format!("inbox:{}", post.to);
        let chime = match card::gate_alert(&account, &chat, "inbox", true) {
            card::Verdict::Hidden { .. } => return,
            card::Verdict::Show { chime } => chime,
        };
        let title = post.title();
        let said = events::card_text(&post.text, 170);
        let body = if said.is_empty() { "No details.".to_string() } else { said };
        log(&format!("inbox card: {title}"));
        let action = match coord {
            Some((a, id, t)) => card::Action::Coordinator(a, id, t),
            None => card::Action::Show,
        };
        let badge = self.badge_for(&account);
        self.alert_in(&title, &body, badge, action, chime, Some((chat, "inbox".into())));
    }

    /// Every couple of seconds: pick up a finished alarm scan, and start the next one when two
    /// minutes have passed. The scan (index, chat list, inbox) runs on its own thread; only the
    /// cards are made here.
    fn poll_alarms(&mut self) {
        if crate::demo::on() {
            return;
        }
        let scan = ALARM_SCAN.lock().ok().and_then(|mut s| s.take());
        if let Some(scan) = scan {
            self.raise_alarms(scan);
        }
        if self.alarm_checked.elapsed() < Duration::from_secs(card::ALARM_EVERY_SECS) {
            return;
        }
        self.alarm_checked = Instant::now();
        if !(self.cfg.alerts_finished() || self.cfg.alerts_needs_me()) || ALARM_BUSY.swap(true, Ordering::SeqCst) {
            return;
        }
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            struct Done;
            impl Drop for Done {
                fn drop(&mut self) {
                    ALARM_BUSY.store(false, Ordering::SeqCst);
                }
            }
            let _done = Done;
            let scan = scan_alarms(&cfg);
            if let Ok(mut s) = ALARM_SCAN.lock() {
                *s = Some(scan);
            }
        });
    }

    /// One card per chat per threshold: "hand off soon" past 250k, "hand off now" past 300k, and
    /// "card <id> worker went quiet". A snoozed or muted chat gets nothing and is tried again later.
    fn raise_alarms(&mut self, scan: AlarmScan) {
        let now = time::now();
        let mut changed = false;
        for (key, level) in self.alarms.context_due(&scan.ctx, now) {
            let (account, title) = scan.info.get(&key).cloned().unwrap_or_default();
            // The alarm is keyed by Claude Code's session id; the Code tab id (when the chat has
            // one) is what Open chat, snooze and mute use.
            let chat = scan.local.get(&key).cloned().unwrap_or_else(|| key.clone());
            if !card::gate_alarm(&account, &chat) {
                self.alarms.defer_ctx(&key);
                changed = true;
                continue;
            }
            let (head, line) = if level >= 2 {
                ("Chat context passed 300k", "Hand off now: write a handoff and continue in a fresh chat.")
            } else {
                ("Chat context passed 250k", "Hand off soon.")
            };
            log(&format!("context alarm: {head} ({})", events::cut_at_word(&title, 48)));
            self.alarm_card(head, &events::cut_at_word(&title, 48), line, account, &chat, title, "context");
            self.alarms.mark_ctx(&key, level);
            changed = true;
        }
        for key in self.alarms.quiet_due(&scan.workers, &scan.posts, now) {
            let (account, title) = scan.info.get(&key).cloned().unwrap_or_default();
            let Some(w) = scan.workers.iter().find(|w| w.key == key) else { continue };
            if !card::gate_alarm(&account, &key) {
                continue;
            }
            let head = format!("card {} worker went quiet", w.card);
            let line = format!("No turn for {} min and no inbox post from this card.", now.saturating_sub(w.last_turn) / 60);
            log(&format!("watchdog: {head}"));
            self.alarm_card(&head, &events::cut_at_word(&title, 48), &line, account, &key, title, "quiet");
            self.alarms.mark_quiet(&key);
            changed = true;
        }
        if changed {
            self.alarms.save();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn alarm_card(&mut self, head: &str, short: &str, line: &str, account: String, chat: &str, title: String, kind: &str) {
        let badge = self.badge_for(&account);
        let action = if account.is_empty() { card::Action::Show } else { card::Action::Chat(account, chat.to_string(), title) };
        self.alert_in(head, &format!("{short}\n{line}"), badge, action, true, Some((chat.to_string(), kind.into())));
    }

    /// The Hive coordinator chat, if one can be found: the newest chat of any account titled
    /// "Hive coordinator" (the old ones are renamed "Old Hive coordinator <n>").
    fn coordinator_chat(&self) -> Option<(String, String, String)> {
        if let Some((at, found)) = &*self.coordinator_seen.borrow() {
            if at.elapsed() < Duration::from_secs(60) {
                return found.clone();
            }
        }
        let found = self.find_coordinator_chat();
        *self.coordinator_seen.borrow_mut() = Some((Instant::now(), found.clone()));
        found
    }

    fn find_coordinator_chat(&self) -> Option<(String, String, String)> {
        let mut best: Option<(u64, String, String, String)> = None;
        for a in &self.cfg.accounts {
            for c in claude::chats(&self.cfg, a) {
                if !c.archived && c.title.trim().eq_ignore_ascii_case("Hive coordinator") && best.as_ref().is_none_or(|b| c.last > b.0) {
                    best = Some((c.last, a.name.clone(), c.id, c.title));
                }
            }
        }
        best.map(|(_, a, id, t)| (a, id, t))
    }

    /// A chat card has done its job once you open that chat yourself after it showed.
    fn close_seen_cards(&mut self) {
        if !self.cfg.close_card_on_open {
            return;
        }
        // Only now and then, and cheaply: what is older than 10 minutes is no longer needed.
        let ten = Duration::from_secs(600);
        self.alerted.retain(|_, (_, at)| at.elapsed() < ten);
        self.finished_at.retain(|_, at| at.elapsed() < ten);
        // A muted chat speaks again once you open it.
        for (account, chat, at) in card::muted_chats() {
            if let Some(acc) = self.cfg.find(&account) {
                if chat.starts_with("local_") && claude::focused_at(&self.cfg, acc, &chat).is_some_and(|f| f > at) {
                    card::unmute_chat(&account, &chat);
                }
            }
        }
        for (account, chat, at) in card::chat_cards() {
            // Desktop chats only (Codex threads and terminal chats keep no focus time).
            if !chat.starts_with("local_") {
                continue;
            }
            let Some(acc) = self.cfg.find(&account) else { continue };
            if claude::focused_at(&self.cfg, acc, &chat).is_some_and(|f| f > at) {
                log(&format!("{account}: you opened the chat, closed its card"));
                card::close_chat(&account, &chat);
            }
        }
    }

    fn take_pending_open(&mut self) -> Option<String> {
        let (name, at) = self.pending_open.take()?;
        let latest = self.balloon_at.get().map_or(true, |b| at >= b);
        (latest && at.elapsed() < Duration::from_secs(900)).then_some(name)
    }

    fn save(&mut self) -> Result<(), String> {
        // Never overwrite a hand edit that has a typo in it: make them fix it first.
        if self.cfg_broken {
            let msg = "The settings file has an error, so this change wasn't saved. Fix the file (Settings > Edit settings file) and try again.";
            self.balloon("Change not saved", msg);
            return Err(msg.into());
        }
        let r = self.cfg.save(&config_path());
        self.cfg_mtime = mtime();
        if let Err(e) = &r {
            log(&format!("could not save settings: {e}"));
        }
        self.settings_applied();
        r
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        n.hWnd = self.hwnd;
        n.uID = 1;
        n
    }

    fn tooltip(&self) -> String {
        if self.waiting_since.is_some() {
            return format!("{}\nWaiting for Google Drive", crate::APP_NAME);
        }
        let open = self.cfg.accounts.iter().filter(|a| claude::is_running(&self.cfg, a)).count();
        let quiet = match card::quiet_count() {
            0 => String::new(),
            n => format!("\n{n} chat{} snoozed", if n == 1 { "" } else { "s" }),
        };
        match &self.best {
            Some(b) => format!("{}\n{open} of {} open, use next: {b}{quiet}", crate::APP_NAME, self.cfg.accounts.len()),
            None => format!("{}\n{open} of {} open{quiet}", crate::APP_NAME, self.cfg.accounts.len()),
        }
    }

    fn add_icon(&mut self) -> bool {
        let mut n = self.nid();
        n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        n.uCallbackMessage = WM_TRAY;
        n.hIcon = self.icon;
        fill(&mut n.szTip, &self.tooltip());
        // NIM_ADD fails if the icon already exists (it can also time out yet succeed):
        // a working NIM_MODIFY means the icon is there.
        self.icon_added = unsafe { Shell_NotifyIconW(NIM_ADD, &n) != 0 || Shell_NotifyIconW(NIM_MODIFY, &n) != 0 };
        self.icon_added
    }

    fn update_tip(&self) {
        if !self.icon_added {
            return;
        }
        let mut n = self.nid();
        n.uFlags = NIF_TIP;
        fill(&mut n.szTip, &self.tooltip());
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
    }

    fn balloon(&self, title: &str, text: &str) {
        self.balloon_sound(title, text, true);
    }

    fn balloon_sound(&self, title: &str, text: &str, chime: bool) {
        self.balloon_at.set(Some(Instant::now()));
        let mut n = self.nid();
        n.uFlags = NIF_INFO;
        // Our own soft chime (or nothing) replaces the Windows sound unless "windows" is chosen.
        let sound = if chime { self.cfg.alert_sound.as_str() } else { "none" };
        n.dwInfoFlags = NIIF_INFO | if sound == "windows" { 0 } else { NIIF_NOSOUND };
        fill(&mut n.szInfoTitle, title);
        fill(&mut n.szInfo, text);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
        if sound == "soft" {
            crate::sound::chime(false);
        }
    }

    /// An alert: something that happened while you may not be looking. Shown on
    /// this app's card (which stays up as long as Settings says), or as a Windows
    /// notification when "Brief" is chosen. Clicking it runs `action`.
    fn alert(&mut self, title: &str, text: &str, badge: Option<(String, u32)>, action: card::Action) {
        self.alert_with(title, text, badge, action, true);
    }

    /// `alert`, with the chime left out when `chime` is false.
    fn alert_with(&mut self, title: &str, text: &str, badge: Option<(String, u32)>, action: card::Action, chime: bool) {
        self.alert_in(title, text, badge, action, chime, None);
    }

    /// `alert_with` for a chat: `chat` is (chat id, what happened), and the chat keeps one card.
    fn alert_in(&mut self, title: &str, text: &str, badge: Option<(String, u32)>, action: card::Action, chime: bool, chat: Option<(String, String)>) {
        let stay = match (self.cfg.alert_style.as_str(), self.cfg.alert_card.as_str()) {
            ("windows", _) | (_, "windows") => None,
            (_, "30") => Some(30),
            (_, "120") => Some(120),
            _ => Some(0),
        };
        let Some(stay) = stay else {
            let clickable = !matches!(action, card::Action::Show);
            let text = if clickable { format!("{text} Click to open it.") } else { text.to_string() };
            self.balloon_sound(title, &text, chime);
            self.pending_chat = None;
            self.pending_open = None;
            match action {
                card::Action::Chat(a, id, t) => self.pending_chat = Some((a, id, t, Instant::now())),
                card::Action::Open(n) => self.pending_open = Some((n, Instant::now())),
                card::Action::ContinueIn(a, id, t, ..) | card::Action::Coordinator(a, id, t) => self.pending_chat = Some((a, id, t, Instant::now())),
                card::Action::Show | card::Action::Preview | card::Action::Waiting | card::Action::Away => {}
            }
            return;
        };
        match if chime { self.cfg.alert_sound.as_str() } else { "none" } {
            "soft" => crate::sound::chime(false),
            "windows" => crate::sound::windows_default(),
            _ => {}
        }
        match chat {
            Some((id, kind)) => card::show_chat(&id, &kind, title, text, badge, action, stay),
            None => card::show(title, text, badge, action, stay),
        }
    }

    /// The badge for an account's alert card: its initials in its tag color.
    fn badge_for(&self, account: &str) -> Option<(String, u32)> {
        if account.is_empty() {
            return None;
        }
        if account == "Codex" {
            return Some(("Cx".into(), 0x10A37F));
        }
        let colors = ui::avatar_palette(self.cfg.tag_names().into_iter());
        let rgb = colors.iter().find(|(n, _)| n == account).map(|(_, c)| *c)?;
        Some((crate::taskbar::initials(account), rgb))
    }

    /// Take down the Windows notification about `account`, if that is what's showing.
    fn clear_balloon_for(&mut self, account: &str) {
        let chat = self.pending_chat.as_ref().is_some_and(|p| p.0 == account);
        let open = self.pending_open.as_ref().is_some_and(|p| p.0 == account);
        if !(chat || open) {
            return;
        }
        self.pending_chat = None;
        self.pending_open = None;
        let mut n = self.nid();
        n.uFlags = NIF_INFO; // empty text removes the notification that is up
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
    }

    fn remove_icon(&mut self) {
        if self.icon_added {
            let n = self.nid();
            unsafe { Shell_NotifyIconW(NIM_DELETE, &n) };
            self.icon_added = false;
        }
    }

    /// A window opened or closed. Keep-running accounts that closed get a
    /// 30 second grace period before they are reopened.
    fn running_changed(&mut self) {
        let now = Instant::now();
        let quiet = self.shutting_down || self.waiting_since.is_some();
        // A quit that didn't finish within a minute failed: stop waiting for it.
        self.quitting.retain(|_, t| t.elapsed() < Duration::from_secs(60));
        for a in &self.cfg.accounts {
            if self.quitting.contains_key(&a.name) {
                if !claude::is_running(&self.cfg, a) {
                    self.quitting.remove(&a.name);
                }
                self.seen_running.remove(&a.name);
                self.closed_at.remove(&a.name);
            } else if claude::is_running(&self.cfg, a) {
                self.seen_running.insert(a.name.clone());
                self.closed_at.remove(&a.name);
            } else if a.keep_running && self.seen_running.contains(&a.name) && !quiet {
                if !self.closed_at.contains_key(&a.name) {
                    log(&format!("{} closed: Keep running will reopen it in about 30 s", a.name));
                }
                self.closed_at.entry(a.name.clone()).or_insert(now);
            } else {
                self.closed_at.remove(&a.name);
            }
        }
        if !self.closed_at.is_empty() {
            unsafe { SetTimer(self.hwnd, TIMER_WATCHDOG, 31_000, None) };
        }
        self.update_tip();
        tags::sync(&self.cfg);
    }

    fn watchdog(&mut self) {
        unsafe { KillTimer(self.hwnd, TIMER_WATCHDOG) };
        self.running_changed();
        let due: Vec<Account> = self
            .cfg
            .accounts
            .iter()
            .filter(|a| self.closed_at.get(&a.name).is_some_and(|t| t.elapsed() >= Duration::from_secs(30)))
            .filter(|a| self.last_relaunch.get(&a.name).map_or(true, |t| t.elapsed() > Duration::from_secs(300)))
            .cloned()
            .collect();
        if due.is_empty() {
            return;
        }
        for a in &due {
            self.closed_at.remove(&a.name);
            self.last_relaunch.insert(a.name.clone(), Instant::now());
        }
        let names: Vec<_> = due.iter().map(|a| a.name.as_str()).collect();
        log(&format!("keep running: reopening {}", names.join(", ")));
        self.alert(&format!("Reopened {}", names.join(", ")), "Keep running is on. Turn it off in the account's menu.", None, card::Action::Show);
        launcher::enqueue(&self.cfg, due);
    }

    /// New usage samples: alert when an account crosses the limit or its week resets,
    /// and keep the "most room" tooltip current. The first look only records levels.
    fn usage_changed(&mut self) {
        if crate::demo::on() {
            return;
        }
        let lim = self.cfg.alert_percent;
        let mut scores = Vec::new();
        let mut alerts: Vec<(String, String)> = Vec::new();
        let mut out: Option<(String, String)> = None;
        let mut limit_inputs = Vec::new();
        let mut now_all: Vec<Standing> = Vec::new();
        for a in &self.cfg.accounts {
            let org = match claude::org(&self.cfg, a) {
                claude::Org::Id(id) => Some(id),
                claude::Org::SignedOut => continue,
                claude::Org::Unknown => None,
            };
            let Some(u) = claude::usage(&self.cfg, a, org.as_deref()) else { continue };
            limit_inputs.push(crate::stats::LimitInput { account: a.name.clone(), samples: claude::usage_samples(&self.cfg, a, org.as_deref()), five_reset: u.five_reset, week_reset: u.week_reset.map(|w| w.0) });
            scores.push(standing(&a.name, &u));
            now_all.push(standing(&a.name, &u));
            let five = u.five_hour.unwrap_or(0);
            let reset = u.week_reset.map(|(at, unc)| format!("Resets {}.", time::when(at, unc))).unwrap_or_default();
            if let Some((pw, pf)) = self.levels.insert(a.name.clone(), (u.week, five)) {
                if pw < 100 && u.week >= 100 {
                    out = Some((format!("{} is out of weekly usage", a.name), reset.clone()));
                } else if pf < 100 && five >= 100 {
                    let when = u.five_reset.map(|t| format!("Resets ~{}.", time::clock_at(t))).unwrap_or_default();
                    out = Some((format!("{} is out of its 5-hour usage", a.name), when));
                }
                if lim > 0 && pw < lim && u.week >= lim {
                    alerts.push((format!("{} is at {}% for the week", a.name, u.week), reset.clone()));
                }
                if lim > 0 && pf < lim && five >= lim {
                    let when = u.five_reset.map(|t| format!("Resets ~{}.", time::clock_at(t))).unwrap_or_default();
                    alerts.push((format!("{} is at {}% of its 5-hour limit", a.name, five), when));
                }
                if pw >= 50 && u.week + 30 <= pw {
                    alerts.push((format!("{}'s weekly limit reset", a.name), format!("Back to {}% used.", u.week)));
                }
            }
        }
        // Log 80/95/100% crossings (limits.jsonl) even when the dashboard is closed.
        crate::stats::record_and_log(&crate::exe_dir(), &limit_inputs, time::now(), &log);
        self.usage_now = now_all;
        self.best = pick_best(&scores, self.cfg.use_next_below);
        let is_out = out.is_some();
        if let Some((title, mut text)) = out.or_else(|| alerts.into_iter().next()) {
            if let Some(b) = &self.best {
                if is_out {
                    text.push_str(&format!(" Click to open {b}: it resets soonest and still has room."));
                } else {
                    text.push_str(&format!(" Use next: {b}."));
                }
            }
            log(&format!("alert: {title}"));
            let action = match (is_out, self.best.clone()) {
                (true, Some(b)) => card::Action::Open(b),
                _ => card::Action::Show,
            };
            self.alert(&title, text.trim(), None, action);
        }
        self.update_tip();
    }

    fn account_submenu(&self, i: usize, a: &Account) -> HMENU {
        let cfg = &self.cfg;
        let last = cfg.accounts.len().saturating_sub(1);
        let id = |act: usize| ID_ACT + i * ACTS + act;
        unsafe {
            let sub = CreatePopupMenu();
            if !a.purpose.is_empty() {
                add(sub, MF_STRING | MF_GRAYED, 0, &a.purpose);
                add(sub, MF_SEPARATOR, 0, "");
            }
            add(sub, MF_STRING, id(A_OPEN), "Open");
            let email = if a.email.is_empty() { "Set sign-in email...".to_string() } else { format!("Copy {}", a.email) };
            add(sub, MF_STRING, id(A_EMAIL), &email);
            if !claude::chrome_shared() {
                add(sub, MF_STRING | checked(claude::chrome_is(cfg, a)), id(A_CHROME), "Use Claude in Chrome here");
            }
            add(sub, MF_STRING, id(A_EDIT), "Edit...");
            add(sub, MF_STRING, id(A_FOLDER), "Open data folder");
            add(sub, MF_STRING, id(A_TIDY), "Tidy up chats...");
            add(sub, MF_SEPARATOR, 0, "");
            add(sub, MF_STRING | checked(a.startup), id(A_STARTUP), "Open when Windows starts");
            add(sub, MF_STRING | checked(a.keep_running), id(A_KEEP), "Keep running (reopen if it closes)");
            add(sub, MF_SEPARATOR, 0, "");
            add(sub, MF_STRING | grayed(i == 0), id(A_UP), "Move up");
            add(sub, MF_STRING | grayed(i == last), id(A_DOWN), "Move down");
            if claude::is_running(cfg, a) {
                add(sub, MF_STRING, id(A_QUIT), "Quit this window...");
            }
            add(sub, MF_STRING | grayed(a.default_profile), id(A_REMOVE), "Remove from list...");
            sub
        }
    }

    fn settings_submenu(&self) -> HMENU {
        unsafe {
            let s = CreatePopupMenu();
            add(s, MF_STRING | checked(startup_enabled()), ID_START_WIN, "Start with Windows");
            add(s, MF_STRING | checked(self.cfg.wait_for_drive.enabled), ID_WAIT_DRIVE, "Wait for Google Drive at startup");
            add(s, MF_STRING | checked(self.cfg.codex.enabled), ID_CODEX_TOGGLE, "Show Codex usage");
            let summary = if self.cfg.daily_summary.trim().is_empty() { "Daily usage summary".to_string() } else { format!("Daily usage summary at {}", self.cfg.daily_summary) };
            add(s, MF_STRING | checked(!self.cfg.daily_summary.trim().is_empty()), ID_SUMMARY, &summary);
            let next = CreatePopupMenu();
            for (i, p) in USE_NEXT_CHOICES.iter().enumerate() {
                add(next, MF_STRING | checked(self.cfg.use_next_below == *p), ID_USE_NEXT + i, &format!("Under {p}% used"));
            }
            let label = format!("Use next: soonest reset under {}%", self.cfg.use_next_below);
            AppendMenuW(s, MF_POPUP, next as usize, menu_text(&label).as_ptr());
            add(s, MF_STRING | checked(self.cfg.alerts_finished()), ID_NOTIFY, "Alert when a chat finishes");
            add(s, MF_STRING | checked(self.cfg.alerts_needs_me()), ID_NOTIFY_WAITING, "Alert when a chat needs me");
            add(s, MF_STRING | checked(self.cfg.show_tags), ID_TAGS, "Show account name tags on windows");
            add(s, MF_STRING | checked(self.cfg.separate_taskbar), ID_TASKBAR, "Separate taskbar button per account");
            add(s, MF_STRING, ID_LAYOUT_SAVE, "Save window layout");
            add(s, MF_STRING | grayed(self.cfg.layout.slots.is_empty()), ID_LAYOUT_RESTORE, "Restore window layout");
            add(s, MF_STRING | checked(self.cfg.restore_on_open), ID_LAYOUT_AUTO, "Restore layout when an account opens");
            if !self.cfg.hotkey.trim().is_empty() {
                add(s, MF_STRING | MF_GRAYED, 0, &format!("Shortcut: {}", self.cfg.hotkey));
            }
            add(s, MF_SEPARATOR, 0, "");
            add(s, MF_STRING, ID_EDIT_CFG, "Edit settings file");
            add(s, MF_STRING, ID_OPEN_ROOT, "Open profiles folder");
            add(s, MF_STRING, ID_VIEW_LOG, "View log");
            add(s, MF_STRING, ID_CHECK, "Check setup");
            add(s, MF_STRING, ID_HELP, "Help");
            s
        }
    }

    /// The classic right-click menu: everything, reachable without the flyout.
    fn build_menu(&mut self) -> HMENU {
        self.menu_names = self.cfg.accounts.iter().map(|a| a.name.clone()).collect();
        let cfg = &self.cfg;
        unsafe {
            let m = CreatePopupMenu();
            add(m, MF_STRING, ID_SHOW, &format!("Show {}", crate::APP_NAME));
            add(m, MF_STRING, ID_DASH, "Usage dashboard");
            add(m, MF_STRING, ID_MEMORY, "Memory");
            add(m, MF_STRING, ID_OBSIDIAN, "Open memory in Obsidian");
            SetMenuDefaultItem(m, ID_SHOW as u32, 0);
            if self.waiting_since.is_some() {
                add(m, MF_STRING | MF_GRAYED, 0, &format!("Waiting for {} ...", cfg.wait_for_drive.path));
                add(m, MF_STRING, ID_SKIP_WAIT, "Open now without waiting");
            }
            add(m, MF_SEPARATOR, 0, "");
            for (i, a) in cfg.accounts.iter().enumerate() {
                let (running, status) = claude::status(cfg, a);
                let dot = if running { "\u{25CF}" } else { "\u{25CB}" };
                add(m, MF_STRING, ID_OPEN + i, &format!("{dot}  {}\t{status}", a.name));
            }
            add(m, MF_SEPARATOR, 0, "");
            add(m, MF_STRING, ID_OPEN_ALL, "Open all");
            if cfg.accounts.iter().any(|a| !a.startup) {
                add(m, MF_STRING, ID_OPEN_STARTUP, "Open startup accounts");
            }
            add(m, MF_STRING, ID_ADD, "Add account...");
            let manage = CreatePopupMenu();
            for (i, a) in cfg.accounts.iter().enumerate() {
                AppendMenuW(manage, MF_POPUP, self.account_submenu(i, a) as usize, menu_text(&a.name).as_ptr());
            }
            AppendMenuW(m, MF_POPUP | grayed(cfg.accounts.is_empty()), manage as usize, wide("Manage accounts").as_ptr());
            let mv = CreatePopupMenu();
            for (i, from) in cfg.accounts.iter().enumerate().take(16) {
                for (j, to) in cfg.accounts.iter().enumerate().take(16) {
                    if i != j {
                        add(mv, MF_STRING, ID_MOVE + i * 16 + j, &format!("Copy {} chats to {}...", from.name, to.name));
                    }
                }
            }
            add(mv, MF_SEPARATOR, 0, "");
            add(mv, MF_STRING, ID_MOVE_UNDO, "Undo last move");
            AppendMenuW(m, MF_POPUP | grayed(cfg.accounts.len() < 2), mv as usize, wide("Move chats").as_ptr());
            AppendMenuW(m, MF_POPUP, self.settings_submenu() as usize, wide("Settings").as_ptr());
            add(m, MF_SEPARATOR, 0, "");
            add(m, MF_STRING, ID_EXIT, "Exit");
            m
        }
    }
}

/// Menu text: "&" would otherwise underline the next letter and vanish.
fn menu_text(text: &str) -> Vec<u16> {
    wide(&text.replace('&', "&&"))
}

unsafe fn add(menu: HMENU, flags: u32, id: usize, text: &str) {
    let t = menu_text(text);
    AppendMenuW(menu, flags, id, if flags & MF_SEPARATOR != 0 { null() } else { t.as_ptr() });
}

fn checked(on: bool) -> u32 {
    if on { MF_CHECKED } else { MF_UNCHECKED }
}

fn grayed(on: bool) -> u32 {
    if on { MF_GRAYED } else { 0 }
}

pub fn main_hwnd() -> HWND {
    with(|a| a.hwnd).unwrap_or(null_mut())
}

fn balloon(title: &str, text: &str) {
    with(|a| a.balloon(title, text));
}

/// A copy of one account, for the edit form.
pub fn account(name: &str) -> Option<Account> {
    with(|a| a.cfg.find(name).cloned()).flatten()
}

// ---- what the flyout calls -------------------------------------------------

/// "61,971"
fn thousands(v: f64) -> String {
    let s = format!("{:.0}", v);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One account whose usage is clearly lowest, if there is a choice.
/// The account to use next, with why (for --status).
pub fn use_next(cfg: &Config) -> Option<String> {
    let all: Vec<Standing> = cfg
        .accounts
        .iter()
        .filter_map(|a| {
            let org = match claude::org(cfg, a) {
                claude::Org::Id(id) => Some(id),
                claude::Org::SignedOut => return None,
                claude::Org::Unknown => None,
            };
            claude::usage(cfg, a, org.as_deref()).map(|u| standing(&a.name, &u))
        })
        .collect();
    if cfg.use_next_below == 0 {
        return Some("off".into());
    }
    let best = pick_best(&all, cfg.use_next_below)?;
    let s = all.iter().find(|s| s.name == best)?;
    let reset = s.reset.map(|t| format!(", week resets {}", time::when(t, s.unc))).unwrap_or_default();
    Some(format!("{best} ({}% of week used{reset})", s.week))
}

/// 95% or more of the 5-hour window or the week: a chat on that account is told where to go on.
const NEARLY_OUT: u8 = 95;

/// For a chat on `from`: the account its alert card offers to continue in, or None. Only when
/// `from` is at 95% or more (5-hour or week); the pick is what "Use next" would pick, leaving
/// out the chat's own account. Any account can be offered for any chat (2026-11-23): the
/// button is a click he makes himself and nothing moves automatically.
fn continue_target(from: &str, all: &[Standing], below: u8) -> Option<String> {
    if below == 0 {
        return None; // "Use next" is off
    }
    let me = all.iter().find(|s| s.name == from)?;
    if me.week.max(me.five) < NEARLY_OUT {
        return None;
    }
    let rest: Vec<Standing> = all.iter().filter(|s| s.name != from).cloned().collect();
    // pick_best wants a choice of at least two; here one candidate is a choice too.
    rest.iter()
        .filter(|s| s.week < below && s.five < below)
        .min_by_key(|s| (s.reset.unwrap_or(u64::MAX), s.week.max(s.five)))
        .map(|s| s.name.clone())
}

/// One account's standing for "Use next".
#[derive(Clone, Debug)]
struct Standing {
    name: String,
    week: u8,
    five: u8,
    /// When its week resets (None if not known yet).
    reset: Option<u64>,
    /// How sure the reset date is (seconds either way).
    unc: u64,
}

fn standing(name: &str, u: &claude::Usage) -> Standing {
    Standing { name: name.to_string(), week: u.week, five: u.five_hour.unwrap_or(0), reset: u.week_reset.map(|(at, _)| at), unc: u.week_reset.map_or(0, |(_, unc)| unc) }
}

/// The account to use next: of those still below `below`% used (week and
/// 5-hour), the one whose week resets soonest, so hours that would expire get
/// used first. Ties and unknown reset dates fall back to the most room.
fn pick_best(all: &[Standing], below: u8) -> Option<String> {
    if all.len() < 2 {
        return None;
    }
    all.iter()
        .filter(|s| s.week < below && s.five < below)
        .min_by_key(|s| (s.reset.unwrap_or(u64::MAX), s.week.max(s.five)))
        .map(|s| s.name.clone())
}

/// Bars and the reset line for a Claude account: when the weekly limit resets.
fn claude_view(u: &claude::Usage, nudge_on: bool) -> (Vec<(String, Option<u8>)>, Option<(String, ui::Tone)>) {
    let bars = vec![("5h".to_string(), u.five_hour), ("Week".to_string(), Some(u.week))];
    let tone = if u.week >= 100 {
        ui::Tone::Bad
    } else if u.week >= 80 {
        ui::Tone::Warn
    } else {
        ui::Tone::Normal
    };
    let mut note = match u.week_reset {
        Some((at, unc)) => format!("Resets {} (in {})", time::when(at, unc), time::span(at.saturating_sub(time::now()))),
        None => "Reset date not known yet (set it in Edit)".to_string(),
    };
    // Claude stopped checking (its poll pauses; see claude::keep_usage_fresh): say how old the numbers are.
    if time::now().saturating_sub(u.as_of) > 30 * 60 {
        let at = time::when(u.as_of, 0);
        let at = at.strip_prefix("today, ").or_else(|| at.strip_prefix("Today, ")).unwrap_or(&at).to_string();
        note = if nudge_on {
            format!("As of {at}  \u{00B7}  {note}")
        } else {
            // Nothing will wake Claude's check: say how (Settings > Usage can do it for you).
            format!("As of {at}  \u{00B7}  right-click its Claude tray icon to update")
        };
    }
    (bars, Some((note, tone)))
}

fn codex_row(name: &str, cx: &codex::Codex) -> ui::Row {
    let label = |m: u64| match m {
        300 => "5h".to_string(),
        10080 => "Week".to_string(),
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m => format!("{}h", m / 60),
    };
    let now = time::now();
    let mut tone = ui::Tone::Normal;
    let parts: Vec<String> = cx
        .windows
        .iter()
        .map(|w| {
            if w.pct >= 100 {
                tone = ui::Tone::Bad;
            } else if w.pct >= 80 && tone == ui::Tone::Normal {
                tone = ui::Tone::Warn;
            }
            format!("{} resets {} (in {})", label(w.minutes), time::when(w.resets_at, 0), time::span(w.resets_at.saturating_sub(now)))
        })
        .collect();
    let mut status = if cx.plan.is_empty() { "Codex".to_string() } else { format!("{} plan", cx.plan) };
    if let Some(c) = cx.credits {
        status.push_str(&format!("  \u{00B7}  {} credits", thousands(c)));
    }
    ui::Row {
        kind: ui::Kind::Codex,
        name: name.into(),
        status,
        running: cx.running,
        signed_out: false,
        chrome: false,
        bars: cx.windows.iter().map(|w| (label(w.minutes), Some(w.pct))).collect(),
        note: (!parts.is_empty()).then(|| (parts.join("  \u{00B7}  "), tone)),
        best: false,
        trend: Vec::new(),
    }
}

/// Everything the flyout shows. Reads each profile's log and usage file, so it
/// runs only when the flyout opens or something changed while it is open.
/// A copy of the current settings (None while they're being changed).
pub fn config() -> Option<Config> {
    with(|a| a.cfg.clone())
}

pub fn snapshot() -> ui::Snapshot {
    let s = real_snapshot();
    if crate::demo::on() {
        return crate::demo::snapshot(s.opts, s.pinned, s.pinned_at);
    }
    s
}

fn real_snapshot() -> ui::Snapshot {
    with(|app| {
        app.reload_if_changed();
        let cfg = &app.cfg;
        let mut scores = Vec::new();
        // One process scan serves both the memory figures and "is Codex running".
        let plist = procs::list();
        let mem = procs::memory_in(cfg, &plist);
        let mut rows: Vec<ui::Row> = cfg
            .accounts
            .iter()
            .map(|a| {
                let running = claude::is_running(cfg, a);
                let org = claude::org(cfg, a);
                let (org_id, signed_out) = match &org {
                    claude::Org::Id(id) => (Some(id.clone()), false),
                    claude::Org::SignedOut => (None, true),
                    claude::Org::Unknown => (None, false),
                };
                let label = org_id.as_deref().map(|id| claude::org_label(cfg, id));
                let status = match (running, signed_out, &label) {
                    (true, true, _) => "Not signed in".to_string(),
                    (true, false, Some(l)) => l.clone(),
                    (true, false, None) => "Open".to_string(),
                    (false, _, Some(l)) => format!("Closed  \u{00B7}  {l}"),
                    (false, _, None) => "Closed".to_string(),
                };
                let status = match mem.get(&a.name) {
                    Some(&b) if running => format!("{status}  \u{00B7}  {}", procs::gb(b)),
                    _ => status,
                };
                let usage = if signed_out { None } else { claude::usage(cfg, a, org_id.as_deref()) };
                if let Some(u) = &usage {
                    scores.push(standing(&a.name, u));
                }
                let (bars, note) = usage.as_ref().map(|u| claude_view(u, !cfg.usage_nudge.is_empty())).unwrap_or_default();
                let trend = usage.as_ref().map(|u| u.trend.clone()).unwrap_or_default();
                ui::Row {
                    kind: ui::Kind::Claude,
                    name: a.name.clone(),
                    status,
                    running,
                    signed_out: running && signed_out,
                    chrome: claude::chrome_is(cfg, a),
                    bars,
                    note,
                    best: false,
                    trend,
                }
            })
            .collect();
        let best = pick_best(&scores, cfg.use_next_below);
        for r in &mut rows {
            r.best = best.as_deref() == Some(r.name.as_str());
        }
        let mains = if cfg.codex.accounts.is_empty() { Default::default() } else { procs::main_pids(cfg, &plist) };
        match codex::read_in(cfg, &plist) {
            Some(mut cx) => {
                if !cfg.codex.accounts.is_empty() {
                    // An extra sign-in's own codex.exe must not light the main row.
                    cx.running = mains.contains_key("Codex");
                }
                rows.push(codex_row("Codex", &cx));
            }
            // No usage yet: still a row, so "Add another Codex sign-in" can be reached.
            None if cfg.codex.enabled && codex::app_exe(cfg).is_some() => rows.push(ui::Row {
                kind: ui::Kind::Codex,
                name: "Codex".into(),
                status: "No Codex usage yet".into(),
                running: mains.contains_key("Codex"),
                signed_out: false,
                chrome: false,
                bars: Vec::new(),
                note: None,
                best: false,
                trend: Vec::new(),
            }),
            None => {}
        }
        // Extra Codex sign-ins: a row each, even before they have any usage.
        if !cfg.codex.accounts.is_empty() {
            for a in &cfg.codex.accounts {
                let running = mains.contains_key(&a.name);
                let mut row = match codex::read_account(cfg, &a.name, running) {
                    Some(cx) => codex_row(&a.name, &cx),
                    None => ui::Row {
                        kind: ui::Kind::Codex,
                        name: a.name.clone(),
                        status: match (running, codex::account_home(cfg, &a.name).join("auth.json").is_file()) {
                            (true, _) => "Open  \u{00B7}  no Codex use yet".into(),
                            (false, true) => "Closed  \u{00B7}  no Codex use yet".into(),
                            (false, false) => "Closed  \u{00B7}  sign in when it opens".into(),
                        },
                        running,
                        signed_out: false,
                        chrome: false,
                        bars: Vec::new(),
                        note: None,
                        best: false,
                        trend: Vec::new(),
                    },
                };
                if let Some(&b) = mem.get(&a.name).filter(|_| running) {
                    row.status = format!("{}  \u{00B7}  {}", row.status, procs::gb(b));
                }
                rows.push(row);
            }
        }
        let w = &cfg.wait_for_drive;
        ui::Snapshot {
            rows,
            waiting: app.waiting_since.map(|_| w.path.clone()),
            drive: (w.enabled && !w.path.trim().is_empty()).then(|| Path::new(&w.path).is_dir()),
            best,
            pinned: cfg.pinned,
            pinned_at: cfg.pinned_at,
            palette: ui::avatar_palette(cfg.tag_names().into_iter()),
            opts: opts_of(cfg, &app.hotkey_errors),
        }
    })
    .unwrap_or_default()
}

/// What the Settings screen shows, from the settings (and any shortcut problems).
pub fn opts_of(cfg: &Config, hotkey_errors: &HashMap<String, String>) -> ui::Opts {
    let w = &cfg.wait_for_drive;
    ui::Opts {
        hotkeys: hotkey_targets(cfg)
            .into_iter()
            .map(|(key, label, section)| ui::KeyRow {
                combo: if key == "show" { cfg.hotkey.clone() } else { cfg.hotkeys.get(&key).cloned().unwrap_or_default() },
                error: hotkey_errors.get(&key).cloned(),
                key,
                label,
                section,
            })
            .collect(),
        start_with_windows: startup_enabled(),
        wait_drive: w.enabled,
        drive_path: w.path.clone(),
        tags: cfg.show_tags,
        taskbar: cfg.separate_taskbar,
        layout_auto: cfg.restore_on_open,
        codex: cfg.codex.enabled,
        alert_percent: cfg.alert_percent,
        use_next: cfg.use_next_below,
        usage_bars: cfg.usage_bars.clone(),
        alert_sound: cfg.alert_sound.clone(),
        alert_card: cfg.alert_card.clone(),
        alert_style: cfg.alert_style.clone(),
        daily_summary: cfg.daily_summary.clone(),
        usage_nudge: cfg.usage_nudge.clone(),
        notify: cfg.alerts_finished(),
        notify_waiting: cfg.alerts_needs_me(),
        questions: cfg.alert_questions,
        codex_alerts: codex::alerts_on(cfg),
        card_seen: cfg.close_card_on_open,
        card_monitor: cfg.card_monitor.clone(),
        card_monitor_index: cfg.card_monitor_index,
        card_corner: cfg.card_corner.clone(),
    }
}

/// Change settings from the Settings screen: saved and applied at once, like the menu.
pub fn update_settings(f: impl FnOnce(&mut Config)) {
    with(|a| {
        f(&mut a.cfg);
        let _ = a.save();
    });
}

/// (monitor device name, its left-to-right index, corner) for alert cards. None while the app state is busy.
pub fn card_position() -> Option<(String, Option<usize>, String)> {
    with(|a| (a.cfg.card_monitor.clone(), a.cfg.card_monitor_index, a.cfg.card_corner.clone()))
}

/// The Settings tab to open on.
pub fn settings_tab() -> u8 {
    with(|a| a.cfg.settings_tab).unwrap_or(0)
}

/// (usage alert %, use-next %) for the Settings screen's number boxes; 0 = off.
pub fn number_settings() -> (u8, u8) {
    with(|a| (a.cfg.alert_percent, a.cfg.use_next_below)).unwrap_or((90, 95))
}

/// The buttons at the bottom of the Settings screen.
pub fn settings_action(owner: HWND, which: u8) {
    let id = match which {
        0 => ID_CHECK,
        1 => ID_EDIT_CFG,
        2 => ID_HELP,
        _ => ID_EXIT,
    };
    unsafe { command(owner, id) };
}

pub fn set_pinned(on: bool) {
    with(|a| {
        a.cfg.pinned = on;
        let _ = a.save();
    });
}

pub fn set_pinned_at(x: i32, y: i32) {
    with(|a| {
        if a.cfg.pinned_at != Some([x, y]) {
            a.cfg.pinned_at = Some([x, y]);
            let _ = a.save();
        }
    });
}

/// Is this account's window the one in front right now?
fn account_in_front(cfg: &Config, name: &str) -> bool {
    let front = unsafe { GetForegroundWindow() };
    !front.is_null() && tags::account_windows(cfg).iter().any(|(n, h)| n == name && *h == front)
}

/// A chat alert was clicked: bring that account's window forward, then open the chat in it.
/// You brought an account's window (or Codex) to the front yourself: close its
/// alerts, so you don't have to click them too.
pub fn account_focused(name: &str) {
    card::dismiss_account(name);
    with(|a| a.clear_balloon_for(name));
}

/// Codex alerts on or off from Settings: edits Codex's notify setting (with a backup).
pub fn set_codex_alerts(on: bool) {
    let Some(cfg) = with(|a| a.cfg.clone()) else { return };
    match codex::set_alerts(&cfg, on) {
        Ok(()) => {
            log(&format!("codex alerts {}", if on { "on" } else { "off" }));
            update_settings(|c| c.codex.alerts = on);
        }
        Err(e) => balloon("Codex alerts", &e),
    }
}

/// A clicked alert card.
pub fn card_action(a: card::Action) {
    match a {
        card::Action::Chat(account, id, title) => open_from_alert(&account, &id, &title),
        card::Action::Open(name) => open_account(&name),
        card::Action::Show | card::Action::Away => ui::show(),
        card::Action::ContinueIn(_, _, title, target, cli) => continue_in(&target, &cli, &title),
        card::Action::Coordinator(account, id, title) => open_from_alert(&account, &id, &title),
        card::Action::Preview | card::Action::Waiting => {}
    }
}

fn open_from_alert(account: &str, id: &str, title: &str) {
    card::unmute_chat(account, id);
    if account == "Codex" || with(|a| a.cfg.find_codex(account).is_some()).unwrap_or(false) {
        // The chat itself: Codex's own thread link switches the window to it.
        if codex::is_thread_id(id) {
            let shown = codex_to_front(account, false);
            if let Some(cfg) = with(|a| a.cfg.clone()) {
                let running = shown || procs::main_pids(&cfg, &procs::list()).contains_key(account);
                match codex::open_thread(&cfg, account, id) {
                    Ok(()) => {
                        log(&format!("opened Codex thread {id} in {account}"));
                        // The link may have started that Codex: track its window like any open.
                        codex_opened(&cfg, account, running);
                    }
                    Err(e) => log(&format!("Codex thread {id}: {e}")),
                }
            }
            if !shown {
                CODEX_FRONT.with(|c| *c.borrow_mut() = Some((account.to_string(), 0)));
                unsafe { SetTimer(main_hwnd(), TIMER_CODEX_FRONT, 500, None) };
            }
            return;
        }
        if !codex_to_front(account, false) {
            // Not showing (closed, or hidden to its tray icon): open it, then
            // bring it forward once its window is up.
            open_codex_named(account);
            CODEX_FRONT.with(|c| *c.borrow_mut() = Some((account.to_string(), 0)));
            unsafe { SetTimer(main_hwnd(), TIMER_CODEX_FRONT, 500, None) };
            log(&format!("opened {account} from its alert"));
        }
        return;
    }
    if let Some(cfg) = with(|a| a.cfg.clone()) {
        if let Some((_, h)) = tags::account_windows(&cfg).into_iter().find(|(n, _)| n == account) {
            if let Err(why) = crate::win::bring_forward_why(h) {
                log(&format!("could not bring {account} to the front: {why}"));
            }
        }
    }
    if account.is_empty() {
        ui::show();
    } else if id.is_empty() {
        open_account(account);
    } else {
        open_chat(account, id, title);
    }
}

/// Bring a Codex window (main "Codex" or an extra sign-in) to the front, if one is showing.
/// A failure is logged only when `last` (the retry timer logs its final try, not each one).
fn codex_to_front(account: &str, last: bool) -> bool {
    let Some(cfg) = with(|a| a.cfg.clone()) else { return false };
    let pid = procs::codex_pids(&cfg, &procs::list()).get(account).copied();
    let Some(h) = pid.and_then(tags::window_of) else { return false };
    if unsafe { IsWindowVisible(h) } == 0 {
        return false;
    }
    let r = crate::win::bring_forward_why(h);
    let ok = r.is_ok();
    if ok || last {
        match &r {
            Ok(()) => log(&format!("{account} alert: brought to the front")),
            Err(why) => log(&format!("{account} alert: could not bring it to the front: {why}")),
        }
    }
    ok
}

thread_local! {
    /// A Codex window being opened from an alert: (account, ticks waited).
    static CODEX_FRONT: RefCell<Option<(String, u32)>> = const { RefCell::new(None) };
}

/// Remember where each open Claude window is.
pub fn save_layout() {
    let cfg = with(|a| a.cfg.clone());
    let Some(cfg) = cfg else { return };
    let (lay, saved, left_out) = layout::save(&cfg);
    if saved.is_empty() {
        balloon("Nothing to save", "Open an account's window (not minimized) first.");
        return;
    }
    let mut text = saved.join(", ");
    if !left_out.is_empty() {
        text.push_str(&format!(". Left out (hidden or minimized): {}.", left_out.join(", ")));
    }
    with(|a| {
        // Keep older spots for accounts that were closed now.
        let mut merged = a.cfg.layout.slots.clone();
        merged.extend(lay.slots.clone());
        a.cfg.layout.slots = merged;
        a.cfg.layout.saved_at = lay.saved_at.clone();
        let _ = a.save();
    });
    log(&format!("layout saved: {text}"));
    balloon(&format!("Layout saved ({} windows)", saved.len()), &text);
}

/// Put every open Claude window back where it was saved.
pub fn restore_layout() {
    let cfg = with(|a| a.cfg.clone());
    let Some(cfg) = cfg else { return };
    if cfg.layout.slots.is_empty() {
        balloon("No saved layout yet", "Arrange the windows, then choose Save layout.");
        return;
    }
    let (moved, closed, hidden) = layout::restore(&cfg);
    let mut text = format!("Moved {moved} window{}.", if moved == 1 { "" } else { "s" });
    if !closed.is_empty() {
        text.push_str(&format!(" Not open: {}.", closed.join(", ")));
    }
    if !hidden.is_empty() {
        text.push_str(&format!(" Hidden in the tray: {}.", hidden.join(", ")));
    }
    log(&format!("layout restored: {text}"));
    balloon("Layout restored", &text);
}

pub fn open_codex() {
    with(|a| codex::open(&a.cfg));
}

/// Open the main Codex ("Codex") or one of the extra Codex sign-ins.
pub fn open_codex_named(name: &str) {
    let Some(cfg) = with(|a| a.cfg.clone()) else { return };
    let name = if name.is_empty() { "Codex" } else { name };
    let running = procs::main_pids(&cfg, &procs::list()).contains_key(name);
    if name == "Codex" {
        open_codex();
    } else {
        let Some(acc) = cfg.find_codex(name) else { return };
        match codex::open_account(&cfg, &acc.name) {
            Ok(()) => log(&format!("opened Codex sign-in {}", acc.name)),
            Err(e) => {
                balloon(&format!("Couldn't open {}", acc.name), &e);
                return;
            }
        }
    }
    codex_opened(&cfg, name, running);
}

/// A Codex window is opening: restore its saved spot when it shows (if it wasn't
/// open) and find it for tags and taskbar buttons over the next 20 s.
fn codex_opened(cfg: &Config, name: &str, was_running: bool) {
    if cfg.codex.accounts.is_empty() {
        return; // only extra sign-ins make Codex windows tracked
    }
    if !was_running {
        tags::expect_window(name);
    }
    CODEX_FIND_LEFT.store(5, Ordering::SeqCst);
    unsafe { SetTimer(main_hwnd(), TIMER_CODEX_FIND, 4000, None) };
}

/// Which Codex row the "..." menu was opened for.
static CODEX_MENU_FOR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn codex_menu_target() -> String {
    CODEX_MENU_FOR.lock().map(|s| s.clone()).unwrap_or_default()
}

/// A Codex row's "..." menu.
pub fn codex_menu(owner: HWND, x: i32, y: i32, name: &str) {
    if let Ok(mut s) = CODEX_MENU_FOR.lock() {
        *s = name.to_string();
    }
    let extra = with(|a| a.cfg.find_codex(name).cloned()).flatten();
    let menu = unsafe {
        let m = CreatePopupMenu();
        add(m, MF_STRING | MF_DEFAULT, ID_CODEX_OPEN, &format!("Open {name}"));
        add(m, MF_STRING, ID_CODEX_FOLDER, "Open Codex folder");
        add(m, MF_SEPARATOR, 0, "");
        match &extra {
            Some(acc) => {
                add(m, MF_STRING | checked(acc.startup), ID_CODEX_STARTUP, "Open when Windows starts");
                add(m, MF_STRING, ID_CODEX_REMOVE, "Remove from list...");
            }
            None => {
                add(m, MF_STRING, ID_CODEX_ADD, "Add another Codex sign-in...");
                add(m, MF_STRING, ID_CODEX_TOGGLE, "Hide Codex usage");
            }
        }
        m
    };
    popup(owner, menu, x, y);
}

/// Add an extra Codex sign-in: its own window, Codex folder and sign-in.
pub fn add_codex_account(name: &str, note: &str, startup: bool, open_now: bool) -> Result<(), String> {
    let name = name.trim().to_string();
    validate_name(&name)?;
    if name.eq_ignore_ascii_case("Codex") {
        return Err("\"Codex\" is the main Codex. Pick another name, like Codex 2.".into());
    }
    let cfg = with(|app| -> Result<Config, String> {
        app.reload_if_changed();
        if app.cfg.find(&name).is_some() || app.cfg.find_codex(&name).is_some() {
            return Err(format!("There is already an account called {name}."));
        }
        let dir = codex::account_dir(&app.cfg, &name);
        if dir.exists() && std::fs::read_dir(&dir).map_or(false, |mut d| d.next().is_some()) && !dir.join("codex").is_dir() {
            return Err(format!("{} already exists and isn't a Codex folder. Pick another name.", dir.display()));
        }
        app.cfg.codex.accounts.push(crate::config::CodexAccount { name: name.clone(), purpose: note.trim().into(), startup, ..Default::default() });
        if let Err(e) = app.save() {
            app.cfg.codex.accounts.pop();
            return Err(e);
        }
        log(&format!("added Codex sign-in {name} ({})", dir.display()));
        Ok(app.cfg.clone())
    })
    .unwrap_or_else(|| Err("Busy, try again.".into()))?;
    if open_now {
        match codex::open_account(&cfg, &name) {
            Ok(()) => {
                codex_opened(&cfg, &name, false);
                balloon(&format!("Opening {name}"), "Sign in to the account you want in the new Codex window. Your main Codex is not touched.");
            }
            Err(e) => balloon(&format!("Added {name}, but couldn't open it"), &e),
        }
    } else {
        balloon(&format!("Added {name}"), "Open it from its row when you need it.");
    }
    Ok(())
}

/// Code-tab chats from every account, newest first.
/// Chats from the last read, newest first (None before the first read finishes).
/// The alarm scan: its result waits here for the UI thread, the chat index stays in memory
/// between scans, and only one scan runs at a time.
static ALARM_SCAN: std::sync::Mutex<Option<AlarmScan>> = std::sync::Mutex::new(None);
static ALARM_INDEX: std::sync::Mutex<Option<crate::stats::ChatIndex>> = std::sync::Mutex::new(None);
static ALARM_BUSY: AtomicBool = AtomicBool::new(false);

/// What one alarm scan found. `info` maps an alarm key to (account, title); an empty account
/// means a chat outside Claude Desktop. A context alarm is keyed by Claude Code's session id
/// (the same whether or not the chat list was readable), a worker alarm by its Code tab id;
/// `local` maps a session id to its Code tab id.
#[derive(Default)]
struct AlarmScan {
    ctx: Vec<card::CtxChat>,
    workers: Vec<card::Worker>,
    posts: Vec<card::PostSeen>,
    info: HashMap<String, (String, String)>,
    local: HashMap<String, String>,
}

/// Read what the context alarm and the worker watchdog need (worker thread, low priority):
/// the token index (from the files the dashboard keeps, then its own copy), every account's
/// chat list, the chat events and the inbox. Nothing here writes to Claude's files.
fn scan_alarms(cfg: &Config) -> AlarmScan {
    let _bg = crate::stats::low_priority();
    let now = time::now();
    let own = crate::data_file("hive-ctx-index.json");
    let mut idx = ALARM_INDEX.lock().ok().and_then(|mut i| i.take()).unwrap_or_else(|| {
        let top = crate::data_file("hive-top-chats.json");
        crate::stats::ChatIndex::load(if own.exists() { &own } else { &top })
    });
    idx.update(&claude::projects_dir(), now, Duration::from_secs(8));
    if idx.take_dirty() {
        idx.save(&own);
    }
    let tops: HashMap<String, crate::stats::TopChat> = idx.top(now, usize::MAX).into_iter().map(|c| (c.session.clone(), c)).collect();
    if let Ok(mut i) = ALARM_INDEX.lock() {
        *i = Some(idx);
    }
    let mut turn_at: HashMap<String, u64> = HashMap::new();
    for ev in events::read_recent() {
        let e = turn_at.entry(ev.session).or_insert(0);
        *e = (*e).max(ev.at);
    }
    let mut out = AlarmScan { posts: card::read_posts(&inbox::dir()), ..AlarmScan::default() };
    let mut cache = claude::ChatCache::load(&crate::data_file("hive-chats.json"));
    let mut seen: HashSet<String> = HashSet::new();
    let mut all_listed = true;
    for a in &cfg.accounts {
        let chats = claude::chats_cached(cfg, a, &mut cache);
        all_listed &= !chats.is_empty();
        for c in chats {
            let top = tops.get(&c.cli);
            let last_turn = c.last.max(top.map_or(0, |t| t.last)).max(turn_at.get(&c.id).copied().unwrap_or(0));
            if !c.archived {
                if let Some(card) = card::card_id(&c.title) {
                    let started = card::transcript_created(&claude::projects_dir(), &c.cli).unwrap_or(0);
                    out.workers.push(card::Worker { key: c.id.clone(), card, last_turn, started });
                    out.info.insert(c.id.clone(), (a.name.clone(), c.title.clone()));
                }
            }
            if let Some(t) = top {
                if t.context > card::CTX_SOON && seen.insert(c.cli.clone()) {
                    out.ctx.push(card::CtxChat { key: c.cli.clone(), context: t.context, last: t.last });
                    out.info.insert(c.cli.clone(), (a.name.clone(), c.title.clone()));
                    out.local.insert(c.cli.clone(), c.id.clone());
                }
            }
        }
    }
    // Chats outside Claude Desktop (a terminal): the same context alarm, no chat to open. Only
    // when every account's chat list came back: a list that failed to read would make a Desktop
    // chat look like a terminal one.
    let terminal = if all_listed { tops.values().filter(|t| t.context > card::CTX_SOON && !seen.contains(&t.session)).collect() } else { Vec::new() };
    for t in terminal {
        out.ctx.push(card::CtxChat { key: t.session.clone(), context: t.context, last: t.last });
        out.info.insert(t.session.clone(), (String::new(), format!("Terminal chat {}", t.session.chars().take(8).collect::<String>())));
    }
    out
}

static CHATS: std::sync::Mutex<Option<Vec<ui::ChatItem>>> = std::sync::Mutex::new(None);
static READING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Window and message to post when a read finishes (the latest caller's).
static READ_FOR: std::sync::Mutex<(usize, u32)> = std::sync::Mutex::new((0, 0));
/// Someone is waiting for the chat list: a background warm-up should hurry.
static URGENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Chat records already read: later searches only re-read files that changed.
static CHAT_CACHE: std::sync::Mutex<Option<claude::ChatCache>> = std::sync::Mutex::new(None);

pub fn cached_chats() -> Option<Vec<ui::ChatItem>> {
    if crate::demo::on() {
        return Some(crate::demo::chats());
    }
    CHATS.lock().ok()?.clone()
}

/// Read every account's chat files on a worker thread (thousands of files can
/// take seconds on a cold disk), then post `msg` to `hwnd`.
pub fn read_chats(hwnd: usize, msg: u32) {
    if crate::demo::on() {
        return; // sample chats are already there
    }
    let Some(cfg) = with(|a| a.cfg.clone()) else {
        log("search: settings busy, chats not read");
        return;
    };
    if hwnd != 0 {
        if let Ok(mut t) = READ_FOR.lock() {
            *t = (hwnd, msg);
        }
        URGENT.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    if READING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return; // a read is already running; it posts when done
    }
    let background = hwnd == 0;
    std::thread::spawn(move || {
        if background {
            // Warming up: low disk and CPU priority, so it never slows anything you're doing.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
        }
        let started = Instant::now();
        let cache_file = crate::data_file("hive-chats.json");
        let mut cache = CHAT_CACHE.lock().ok().and_then(|mut c| c.take()).unwrap_or_else(|| claude::ChatCache::load(&cache_file));
        let mut all: Vec<ui::ChatItem> = Vec::new();
        let mut slow = background;
        // A search opened while warming up: finish at normal speed, from the next file on
        // (a busy disk can hold a background read of one account for minutes).
        let mut hurry = || {
            if slow && URGENT.load(std::sync::atomic::Ordering::SeqCst) {
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
                slow = false;
            }
        };
        for a in &cfg.accounts {
            all.extend(claude::chats_cached_with(&cfg, a, &mut cache, &mut hurry).into_iter().map(|c| ui::ChatItem {
                account: a.name.clone(),
                id: c.id,
                title: c.title,
                last: c.last,
                archived: c.archived,
                cli: c.cli,
            }));
        }
        if slow {
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
        }
        URGENT.store(false, std::sync::atomic::Ordering::SeqCst);
        all.sort_by(|a, b| b.last.cmp(&a.last));
        let took = started.elapsed();
        cache.save(&cache_file);
        if let Ok(mut c) = CHAT_CACHE.lock() {
            *c = Some(cache);
        }
        log(&format!("search index: {} chats in {} ms{}", all.len(), took.as_millis(), if background { " (warm-up)" } else { "" }));
        if let Ok(mut c) = CHATS.lock() {
            *c = Some(all);
        }
        READING.store(false, std::sync::atomic::Ordering::SeqCst);
        let (hwnd, msg) = READ_FOR.lock().map(|t| *t).unwrap_or((0, 0));
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd as HWND, msg, 0, 0) };
        }
    });
}

/// The last Tidy up read: (account, chats to offer, why the rest stay).
static TIDY: std::sync::Mutex<Option<(String, Vec<claude::TidyChat>, claude::TidyHeld)>> = std::sync::Mutex::new(None);

/// Read an account's finished chats for Tidy up on a worker thread, then post `msg`.
pub fn read_tidy(account: &str, hwnd: usize, msg: u32) {
    let Some(cfg) = with(|a| a.cfg.clone()) else { return };
    let Some(acc) = cfg.find(account).cloned() else { return };
    // Chats with an alert card up are waiting on you: read on this (the UI) thread.
    let carded: std::collections::HashSet<String> = card::chats_with_cards().into_iter().filter(|(a, _)| *a == acc.name).map(|(_, id)| id).collect();
    std::thread::spawn(move || {
        let started = Instant::now();
        let (chats, held) = claude::tidy_candidates(&cfg, &acc, crate::time::now(), &|id| carded.contains(id));
        log(&format!("tidy up: {} offers {} chats ({} ms)", acc.name, chats.len(), started.elapsed().as_millis()));
        if let Ok(mut t) = TIDY.lock() {
            *t = Some((acc.name.clone(), chats, held));
        }
        unsafe { PostMessageW(hwnd as HWND, msg, 0, 0) };
    });
}

pub fn take_tidy() -> Option<(String, Vec<claude::TidyChat>, claude::TidyHeld)> {
    TIDY.lock().ok()?.take()
}

/// Open a new chat in the account with the archive request filled in (not sent).
pub fn tidy_archive(account: &str, chats: &[claude::TidyChat]) {
    let Some(acc) = self::account(account) else { return };
    let link = claude::new_chat_link(&claude::tidy_prompt(&acc.name, chats));
    match with(|a| claude::launch_with(&a.cfg, &acc, &[link])) {
        Some(Ok(())) => log(&format!("tidy up: asked {} to archive {} chats (prompt filled in, not sent)", acc.name, chats.len())),
        Some(Err(e)) => balloon(&format!("Couldn't open {}", acc.name), &e),
        None => {}
    }
}

/// Open a chat in its account's window (claude://code/continue). The title goes
/// on the clipboard too, in case the window can't jump to it.
pub fn open_chat(account: &str, id: &str, title: &str) {
    card::unmute_chat(account, id);
    let Some(acc) = self::account(account) else { return };
    let link = format!("claude://code/continue?session={id}");
    let sent = crate::time::now_ms();
    let r = with(|a| claude::launch_with(&a.cfg, &acc, std::slice::from_ref(&link)));
    set_clipboard(main_hwnd(), title);
    match r {
        // No notification on success: the window coming forward is the answer, and
        // a second toast right after clicking an alert looked like the alert again.
        Some(Ok(())) => {
            log(&format!("opened chat in {account}"));
            if let Some(cfg) = with(|a| a.cfg.clone()) {
                check_landing(cfg, acc, id.to_string(), link, sent);
            }
        }
        Some(Err(e)) => balloon(&format!("Couldn't open {account}"), &e),
        None => {}
    }
}

/// A moment after a chat link goes out, check that Claude showed that chat (its
/// record's focus time moves). If another chat took focus instead, send the link
/// once more. Logs the outcome either way, so a miss is never silent.
fn check_landing(cfg: Config, acc: Account, id: String, link: String, sent: u64) {
    std::thread::spawn(move || {
        let wait = || std::thread::sleep(Duration::from_millis(2500));
        wait();
        let what = match claude::link_landed(&claude::focus_times(&cfg, &acc), &id, sent) {
            Some(true) => "landed".to_string(),
            None => "no chat took focus (it was most likely already showing)".to_string(),
            Some(false) => {
                let again = crate::time::now_ms();
                let _ = claude::launch_with(&cfg, &acc, std::slice::from_ref(&link));
                wait();
                match claude::link_landed(&claude::focus_times(&cfg, &acc), &id, again) {
                    Some(true) => "another chat took focus; landed after sending it again".to_string(),
                    _ => "did NOT land, even after sending it again".to_string(),
                }
            }
        };
        log(&format!("chat link {id} in {}: {what}", acc.name));
    });
}

/// A Claude Code conversation id: 8-4-4-4-12 hex digits.
fn is_cli_id(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// What to do with a search result picked by right-click.
pub enum ChatChoice {
    Open,
    ContinueIn(String),
}

/// Right-click on a search result: open it, or continue it in another account
/// (handy when its own account is out of usage).
pub fn chat_menu(owner: HWND, account: &str, cli: &str, x: i32, y: i32) -> Option<ChatChoice> {
    let others: Vec<(String, String)> = with(|a| {
        a.cfg
            .accounts
            .iter()
            .filter(|x| x.name != account)
            .map(|x| {
                let org = match claude::org(&a.cfg, x) {
                    claude::Org::Id(id) => Some(id),
                    _ => None,
                };
                let used = claude::usage(&a.cfg, x, org.as_deref()).map(|u| format!("  ({}% of week used)", u.week)).unwrap_or_default();
                (x.name.clone(), used)
            })
            .collect()
    })
    .unwrap_or_default();
    const OPEN: usize = 1;
    const FIRST: usize = 10;
    let cmd = unsafe {
        let m = CreatePopupMenu();
        add(m, MF_STRING | MF_DEFAULT, OPEN, &format!("Open in {account}"));
        add(m, MF_SEPARATOR, 0, "");
        if is_cli_id(cli) {
            for (i, (name, used)) in others.iter().enumerate() {
                add(m, MF_STRING, FIRST + i, &format!("Continue in {name}{used}"));
            }
        } else {
            add(m, MF_STRING | MF_GRAYED, 0, "This chat can't be continued elsewhere");
        }
        SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, x, y, 0, owner, null()) as usize;
        DestroyMenu(m);
        cmd
    };
    match cmd {
        0 => None,
        OPEN => Some(ChatChoice::Open),
        c => others.get(c - FIRST).map(|(n, _)| ChatChoice::ContinueIn(n.clone())),
    }
}

/// Bring a chat into another account's window (claude://resume). The
/// conversation is shared on disk, so it picks up where it left off; the
/// original account keeps its own copy in its sidebar.
pub fn continue_in(target: &str, cli: &str, title: &str) {
    if !is_cli_id(cli) {
        return;
    }
    let Some(acc) = account(target) else { return };
    let link = format!("claude://resume?session={cli}");
    let r = with(|a| claude::launch_with(&a.cfg, &acc, &[link]));
    set_clipboard(main_hwnd(), title);
    match r {
        Some(Ok(())) => log(&format!("continued a chat in {target}")),
        Some(Err(e)) => balloon(&format!("Couldn't open {target}"), &e),
        None => {}
    }
}

pub fn open_account(name: &str) {
    if let Some(acc) = account(name) {
        with(|a| launcher::enqueue(&a.cfg, vec![acc]));
    } else if name == "Codex" || with(|a| a.cfg.find_codex(name).is_some()).unwrap_or(false) {
        open_codex_named(name);
    }
}

/// Extra Codex sign-ins marked "Open when Windows starts", a little after Claude's.
/// Runs inside `with` (the app is borrowed), so the tray window comes in as `hwnd`.
fn open_codex_startup(cfg: &Config, hwnd: HWND) {
    let names: Vec<String> = cfg.codex.accounts.iter().filter(|x| x.startup).map(|x| x.name.clone()).collect();
    if names.is_empty() {
        return;
    }
    let wait = cfg.launch_delay_seconds as u64 * (cfg.accounts.iter().filter(|x| x.startup).count() as u64 + 1);
    // Find the new windows (tags, taskbar buttons, saved spots) once they are up.
    for n in &names {
        tags::expect_window(n);
    }
    let total = wait + names.len() as u64 * cfg.launch_delay_seconds.max(2) as u64;
    CODEX_FIND_LEFT.store((total / 4 + 6) as u32, Ordering::SeqCst);
    unsafe { SetTimer(hwnd, TIMER_CODEX_FIND, 4000, None) };
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(wait));
        for n in names {
            if let Err(e) = codex::open_account(&cfg, &n) {
                log(&format!("startup: couldn't open {n}: {e}"));
            }
            std::thread::sleep(Duration::from_secs(cfg.launch_delay_seconds.max(2) as u64));
        }
    });
}

pub fn open_all() {
    with(|a| launcher::enqueue(&a.cfg, a.cfg.accounts.clone()));
}

pub fn skip_wait() {
    SKIP_WAIT.store(true, Ordering::SeqCst);
}

pub fn chrome_here(name: &str) {
    unsafe { account_action(main_hwnd(), name, A_CHROME) };
}

/// The per-account "..." menu from the flyout.
pub fn account_menu(owner: HWND, name: &str, x: i32, y: i32) {
    let Some(menu) = with(|a| {
        a.reload_if_changed();
        a.menu_names = a.cfg.accounts.iter().map(|x| x.name.clone()).collect();
        let i = a.cfg.accounts.iter().position(|x| x.name == name)?;
        Some(a.account_submenu(i, &a.cfg.accounts[i]))
    })
    .flatten() else {
        return;
    };
    popup(owner, menu, x, y);
}

fn popup(owner: HWND, menu: HMENU, x: i32, y: i32) {
    unsafe {
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, x, y, 0, owner, null());
        DestroyMenu(menu);
        if cmd > 0 {
            command(main_hwnd(), cmd as usize);
        }
        ui::refresh();
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Type a name for the account.".into());
    }
    if name.chars().count() > 40 {
        return Err("Keep the name under 40 characters.".into());
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_') {
        return Err("Use letters, numbers, spaces, - or _ only (it is also the folder name).".into());
    }
    if name.eq_ignore_ascii_case("All") {
        return Err("\"All\" is reserved. Pick another name.".into());
    }
    if name.starts_with('-') {
        return Err("Start the name with a letter or number.".into());
    }
    Ok(())
}

pub fn add_account(name: &str, email: &str, note: &str, weekly_reset: &str, startup: bool, keep: bool, open_now: bool) -> Result<(), String> {
    let name = name.trim().to_string();
    validate_name(&name)?;
    let acc = with(|app| -> Result<Account, String> {
        app.reload_if_changed();
        if app.cfg.find(&name).is_some() || app.cfg.find_codex(&name).is_some() || name.eq_ignore_ascii_case("Codex") {
            return Err(format!("There is already an account called {name}."));
        }
        let acc = Account {
            name: name.clone(),
            purpose: note.trim().into(),
            email: email.trim().into(),
            default_profile: false,
            startup,
            keep_running: keep,
            weekly_reset: weekly_reset.trim().into(),
            ..Account::default()
        };
        let dir = claude::data_dir(&app.cfg, &acc);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
        app.cfg.accounts.push(acc.clone());
        if let Err(e) = app.save() {
            app.cfg.accounts.pop();
            return Err(e);
        }
        log(&format!("added account {} ({})", acc.name, dir.display()));
        if open_now {
            launcher::enqueue(&app.cfg, vec![acc.clone()]);
        }
        Ok(acc)
    })
    .unwrap_or_else(|| Err("Busy, try again.".into()))?;

    if !open_now {
        balloon(&format!("Added {}", acc.name), "Open it from the tray when you need it.");
    } else if !acc.email.is_empty() && set_clipboard(main_hwnd(), &acc.email) {
        balloon(
            &format!("Opening {}", acc.name),
            &format!("{} is copied. In the new window choose Continue with email, paste it, then type the emailed code.", acc.email),
        );
    } else {
        balloon(&format!("Opening {}", acc.name), "Sign in with Continue with email and the emailed code, not Google.");
    }
    Ok(())
}

pub fn edit_account(name: &str, email: &str, note: &str, weekly_reset: &str, startup: bool, keep: bool) -> Result<(), String> {
    with(|app| {
        app.reload_if_changed();
        let a = app.cfg.find_mut(name).ok_or_else(|| format!("{name} is no longer in the list."))?;
        a.email = email.trim().into();
        a.purpose = note.trim().into();
        a.weekly_reset = weekly_reset.trim().into();
        a.startup = startup;
        a.keep_running = keep;
        app.save()
    })
    .unwrap_or_else(|| Err("Busy, try again.".into()))
}

// ---- menu commands ----------------------------------------------------------

unsafe fn show_menu(hwnd: HWND) {
    let Some(menu) = with(|a| {
        a.reload_if_changed();
        a.build_menu()
    }) else {
        return;
    };
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    if cmd > 0 {
        command(hwnd, cmd as usize);
    }
}

unsafe fn command(hwnd: HWND, id: usize) {
    let names = with(|a| a.menu_names.clone()).unwrap_or_default();
    match id {
        ID_EXIT => {
            DestroyWindow(hwnd);
        }
        ID_SHOW => ui::show(),
        ID_CODEX_OPEN => open_codex_named(&codex_menu_target()),
        ID_CODEX_FOLDER => {
            let name = codex_menu_target();
            let home = with(|a| if a.cfg.find_codex(&name).is_some() { codex::account_home(&a.cfg, &name) } else { codex::home(&a.cfg) });
            if let Some(h) = home {
                let _ = std::fs::create_dir_all(&h);
                shell_open(&h.display().to_string(), None);
            }
        }
        ID_CODEX_ADD => ui::open_codex_form(),
        ID_CODEX_STARTUP => {
            let name = codex_menu_target();
            with(|a| {
                if let Some(x) = a.cfg.codex.accounts.iter_mut().find(|x| x.name == name) {
                    x.startup = !x.startup;
                    let _ = a.save();
                }
            });
        }
        ID_CODEX_REMOVE => {
            let name = codex_menu_target();
            let folder = with(|a| codex::account_dir(&a.cfg, &name)).unwrap_or_default();
            let ask = format!("Remove {name} from the list?\n\nIts window is not closed and its folder stays:\n{}", folder.display());
            if msg_box(hwnd, &ask, MB_OKCANCEL | MB_ICONQUESTION) == IDOK {
                with(|a| {
                    codex::unhook_account(&a.cfg, &name);
                    a.cfg.codex.accounts.retain(|x| x.name != name);
                    a.cfg.hotkeys.remove(&format!("account:{name}"));
                    let _ = a.save();
                });
                log(&format!("removed Codex sign-in {name} from the list"));
            }
        }
        i if (ID_USE_NEXT..ID_USE_NEXT + USE_NEXT_CHOICES.len()).contains(&i) => {
            with(|a| {
                a.cfg.use_next_below = USE_NEXT_CHOICES[i - ID_USE_NEXT];
                let _ = a.save();
            });
        }
        ID_SUMMARY => {
            with(|a| {
                a.cfg.daily_summary = if a.cfg.daily_summary.trim().is_empty() { "9:00 AM".into() } else { String::new() };
                let _ = a.save();
            });
        }
        ID_NOTIFY | ID_NOTIFY_WAITING => {
            with(|a| {
                a.cfg.toggle_alert(id == ID_NOTIFY);
                let _ = a.save();
            });
        }
        ID_TAGS => {
            with(|a| {
                a.cfg.show_tags = !a.cfg.show_tags;
                let _ = a.save();
            });
        }
        ID_TASKBAR => {
            with(|a| {
                a.cfg.separate_taskbar = !a.cfg.separate_taskbar;
                let _ = a.save();
            });
        }
        ID_LAYOUT_SAVE => save_layout(),
        ID_LAYOUT_RESTORE => restore_layout(),
        ID_LAYOUT_AUTO => {
            with(|a| {
                a.cfg.restore_on_open = !a.cfg.restore_on_open;
                let _ = a.save();
            });
        }
        ID_CODEX_TOGGLE => {
            with(|a| {
                a.cfg.codex.enabled = !a.cfg.codex.enabled;
                let _ = a.save();
            });
        }
        ID_SKIP_WAIT => skip_wait(),
        ID_OPEN_ALL => open_all(),
        ID_OPEN_STARTUP => {
            with(|a| {
                launcher::enqueue(&a.cfg, a.cfg.accounts.iter().filter(|x| x.startup).cloned().collect());
                open_codex_startup(&a.cfg, a.hwnd);
            });
        }
        ID_ADD => ui::open_form(None),
        ID_START_WIN => set_startup(!startup_enabled()),
        ID_WAIT_DRIVE => {
            with(|a| {
                a.cfg.wait_for_drive.enabled = !a.cfg.wait_for_drive.enabled;
                let _ = a.save();
            });
        }
        ID_EDIT_CFG => notepad(config_path()),
        ID_OPEN_ROOT => {
            if let Some(root) = with(|a| claude::profiles_root(&a.cfg)) {
                shell_open(&root.display().to_string(), None);
            }
        }
        ID_VIEW_LOG => notepad(log_path()),
        ID_DASH => ui::open_dashboard(),
        ID_MEMORY => ui::open_dashboard_on(1),
        ID_OBSIDIAN => {
            if let Some(note) = crate::memory::open_in_obsidian() {
                msg_box(hwnd, &note, MB_OK | MB_ICONINFORMATION);
            }
        }
        ID_CHECK => {
            let found = with(|a| doctor::all(&a.cfg)).unwrap_or_default();
            let text = if found.is_empty() {
                "No problems found.".to_string()
            } else {
                found.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("

")
            };
            msg_box(hwnd, &text, MB_OK | MB_ICONINFORMATION);
        }
        ID_HELP => notepad(crate::exe_dir().join("claude-accounts").join("README.md")),
        i if (ID_OPEN..ID_ACT).contains(&i) => {
            if let Some(name) = names.get(i - ID_OPEN) {
                account_action(hwnd, name, A_OPEN);
            }
        }
        ID_MOVE_UNDO => {
            let text = with(|a| chatmove::undo(&a.cfg)).map_or_else(|| "Not ready.".into(), |r| r.unwrap_or_else(|e| e));
            msg_box(hwnd, &text, MB_OK | MB_ICONINFORMATION);
        }
        i if (ID_MOVE..ID_MOVE + 256).contains(&i) => {
            let k = i - ID_MOVE;
            let (from, to) = (names.get(k / 16).cloned(), names.get(k % 16).cloned());
            if let (Some(from), Some(to)) = (from, to) {
                move_chats(hwnd, &from, &to);
            }
        }
        i if i >= ID_ACT => {
            if let Some(name) = names.get((i - ID_ACT) / ACTS) {
                account_action(hwnd, name, (i - ID_ACT) % ACTS);
            }
        }
        _ => {}
    }
}

/// Ask, then copy one account's chat list entries into another's (with a backup and an undo list).
unsafe fn move_chats(hwnd: HWND, from: &str, to: &str) {
    let Some(cfg) = with(|a| a.cfg.clone()) else { return };
    let (Some(f), Some(t)) = (cfg.find(from), cfg.find(to)) else { return };
    let mut include_work = false;
    if chatmove::is_work(f) && !chatmove::is_work(t) {
        let q = format!("{from} chats may hold client data. Copy them to {to} anyway?");
        if msg_box(hwnd, &q, MB_YESNO | MB_ICONWARNING) != IDYES {
            return;
        }
        include_work = true;
    }
    let plan = match chatmove::plan(&cfg, f, t, include_work, None) {
        Ok(p) => p,
        Err(e) => {
            msg_box(hwnd, &e, MB_OK | MB_ICONINFORMATION);
            return;
        }
    };
    let q = format!(
        "Copy {from} chats to {to}?\n\n{}.\n\nYour {to} list is backed up first, nothing there is changed or replaced, and \"Undo last move\" removes just the copies.",
        chatmove::describe(&plan)
    );
    if plan.add.is_empty() || msg_box(hwnd, &q, MB_YESNO | MB_ICONQUESTION) != IDYES {
        if plan.add.is_empty() {
            msg_box(hwnd, &format!("Nothing to copy: {}.", chatmove::describe(&plan)), MB_OK | MB_ICONINFORMATION);
        }
        return;
    }
    let text = chatmove::apply(&plan).unwrap_or_else(|e| e);
    msg_box(hwnd, &text, MB_OK | MB_ICONINFORMATION);
}

fn notepad(path: PathBuf) {
    shell_open("notepad.exe", Some(&format!("\"{}\"", path.display())));
}

unsafe fn account_action(hwnd: HWND, name: &str, act: usize) {
    let Some(acc) = account(name) else { return };
    match act {
        A_OPEN => open_account(name),
        A_EMAIL => {
            if acc.email.is_empty() {
                ui::open_form(Some(name));
            } else if set_clipboard(hwnd, &acc.email) {
                balloon(&format!("Copied {}", acc.email), "Paste it in the sign-in screen, then type the emailed code.");
            }
        }
        A_CHROME => {
            if claude::chrome_shared() {
                balloon("Claude in Chrome", claude::CHROME_SHARED_NOTE);
                return;
            }
            let r = with(|a| {
                a.cfg.chrome_account = acc.name.clone();
                let _ = a.save();
                claude::set_chrome(&a.cfg, &acc)
            });
            match r {
                Some(Ok(m)) => balloon("Claude in Chrome", &m),
                Some(Err(e)) => balloon("Claude in Chrome", &e),
                None => {}
            }
        }
        A_EDIT => ui::open_form(Some(name)),
        A_TIDY => ui::open_tidy(name),
        A_FOLDER => {
            if let Some(dir) = with(|a| claude::data_dir(&a.cfg, &acc)) {
                shell_open(&dir.display().to_string(), None);
            }
        }
        A_STARTUP | A_KEEP => {
            with(|a| {
                if let Some(x) = a.cfg.find_mut(name) {
                    if act == A_STARTUP { x.startup = !x.startup } else { x.keep_running = !x.keep_running }
                }
                let _ = a.save();
            });
        }
        A_UP | A_DOWN => {
            with(|a| {
                let Some(i) = a.cfg.accounts.iter().position(|x| x.name == acc.name) else { return };
                let j = if act == A_UP { i.checked_sub(1) } else { Some(i + 1).filter(|&j| j < a.cfg.accounts.len()) };
                if let Some(j) = j {
                    a.cfg.accounts.swap(i, j);
                    let _ = a.save();
                }
            });
        }
        A_QUIT => {
            let text = format!(
                "Quit {}?\n\nIts window closes completely (not just to the tray) and frees its memory. Any Claude Code chats running in it stop. Open it again from {} any time.",
                acc.name,
                crate::APP_NAME
            );
            if msg_box(hwnd, &text, MB_YESNO | MB_ICONQUESTION) == IDYES {
                // Quitting on purpose: Keep running must not bring it back.
                let cfg = with(|a| {
                    a.quitting.insert(acc.name.clone(), Instant::now());
                    a.seen_running.remove(&acc.name);
                    a.closed_at.remove(&acc.name);
                    a.cfg.clone()
                });
                if let Some(cfg) = cfg {
                    let acc = acc.clone();
                    std::thread::spawn(move || match procs::quit(&cfg, &acc) {
                        Ok(()) => log(&format!("quit {}", acc.name)),
                        Err(e) => launcher::note(format!("Couldn't quit {}", acc.name), e),
                    });
                }
            }
        }
        A_REMOVE => {
            let dir = with(|a| claude::data_dir(&a.cfg, &acc)).unwrap_or_default();
            let text = format!(
                "Remove {} from the list?\n\nIts window and sign-in stay in\n{}\nso adding it again with the same name brings it back.",
                acc.name,
                dir.display()
            );
            if msg_box(hwnd, &text, MB_YESNO | MB_ICONQUESTION) == IDYES {
                with(|a| {
                    a.cfg.accounts.retain(|x| x.name != acc.name);
                    a.cfg.hotkeys.remove(&format!("account:{}", acc.name));
                    if a.cfg.chrome_account.eq_ignore_ascii_case(&acc.name) {
                        a.cfg.chrome_account.clear();
                    }
                    let _ = a.save();
                    log(&format!("removed account {}", acc.name));
                });
            }
        }
        _ => {}
    }
}

// ---- startup -------------------------------------------------------------------

/// Wait for the Drive folder on a background thread, then open startup accounts.
fn begin_startup(app: &mut App) {
    let w = app.cfg.wait_for_drive.clone();
    let hwnd = app.hwnd as usize;
    // The Drive check itself runs on the thread below: while Drive is mounting,
    // reading K: can block, and the tray must stay responsive.
    if !w.enabled || w.path.trim().is_empty() {
        log("startup: opening accounts (Drive wait is off)");
        unsafe { PostMessageW(hwnd as HWND, WM_DRIVE, 0, 0) };
        return;
    }
    log(&format!("startup: waiting for {}", w.path));
    app.waiting_since = Some(Instant::now());
    app.update_tip();
    unsafe { SetTimer(app.hwnd, TIMER_NAG, 180_000, None) };
    std::thread::spawn(move || {
        let started = Instant::now();
        let limit = Duration::from_secs(w.timeout_minutes as u64 * 60);
        let code = loop {
            if SKIP_WAIT.load(Ordering::SeqCst) {
                break 1;
            }
            if claude::drive_ready(&w.path) {
                break 0;
            }
            if w.timeout_minutes > 0 && started.elapsed() > limit {
                break 2;
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        unsafe { PostMessageW(hwnd as HWND, WM_DRIVE, code, 0) };
    });
}

fn on_drive_ready(code: usize) {
    with(|a| {
        unsafe { KillTimer(a.hwnd, TIMER_NAG) };
        let waited = a.waiting_since.take().map(|t| t.elapsed().as_secs()).unwrap_or(0);
        match code {
            1 => log(&format!("startup: Drive wait skipped after {waited}s")),
            2 => {
                log(&format!("startup: Drive not ready after {waited}s, opening anyway"));
                a.alert("Google Drive isn't ready", "Opened Claude anyway after the wait limit.", None, card::Action::Show);
            }
            _ => log(&format!("startup: Drive ready after {waited}s")),
        }
        let list: Vec<Account> = a.cfg.accounts.iter().filter(|x| x.startup).cloned().collect();
        launcher::enqueue(&a.cfg, list);
        open_codex_startup(&a.cfg, a.hwnd);
        a.update_tip();
    });
    ui::refresh();
}

// ---- window -----------------------------------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP => ui::toggle(),
                WM_RBUTTONUP => {
                    ui::hide();
                    show_menu(hwnd);
                }
                NIN_BALLOONUSERCLICK => {
                    if let Some((account, id, title)) = with(|a| a.take_pending_chat()).flatten() {
                        open_from_alert(&account, &id, &title);
                    } else {
                        match with(|a| a.take_pending_open()).flatten() {
                            Some(name) => open_account(&name),
                            None => ui::show(),
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_SHOWMENU => {
            ui::show();
            0
        }
        WM_SHOWADD => {
            ui::open_form(None);
            0
        }
        WM_HOTKEY => {
            if wp as i32 == HOTKEY_ID {
                ui::toggle();
            } else {
                run_hotkey(wp as i32);
            }
            0
        }
        WM_DRIVE => {
            on_drive_ready(wp);
            0
        }
        WM_NOTE => {
            if let Some((title, text)) = launcher::next_note() {
                balloon(&title, &text);
            }
            0
        }
        procs::WM_QUITTING => {
            // `--quit` from the command line (or our own Quit): don't reopen it.
            with(|a| {
                if let Some(acc) = a.cfg.accounts.get(wp) {
                    let name = acc.name.clone();
                    a.seen_running.remove(&name);
                    a.closed_at.remove(&name);
                    a.quitting.insert(name, Instant::now());
                }
            });
            1
        }
        WM_WATCH => {
            // Coalesce bursts (a save is several file events) into one refresh.
            with(|a| a.pending |= wp);
            SetTimer(hwnd, TIMER_DEBOUNCE, 250, None);
            0
        }
        WM_TIMER => {
            match wp {
                TIMER_NUDGE => {
                    if let Some(cfg) = with(|a| a.cfg.clone()) {
                        crate::nudge::tick(&cfg);
                        if crate::nudge::cleanup_pending() {
                            SetTimer(hwnd, TIMER_NUDGE_CLEAN, 800, None);
                        }
                    }
                }
                TIMER_GOAL => {
                    with(|a| a.release_goal_held());
                }
                TIMER_MEMORY => {
                    if let Some(cfg) = with(|a| a.cfg.clone()) {
                        std::thread::spawn(move || {
                            crate::memory::harvest(&cfg);
                        });
                    }
                }
                TIMER_CARD_SEEN => {
                    with(|a| {
                        a.close_seen_cards();
                        a.poll_inbox_and_away();
                        a.poll_alarms();
                    });
                }
                TIMER_NUDGE_CLEAN => {
                    KillTimer(hwnd, TIMER_NUDGE_CLEAN);
                    crate::nudge::after_send();
                }
                TIMER_DEBOUNCE => {
                    KillTimer(hwnd, TIMER_DEBOUNCE);
                    let kinds = with(|a| std::mem::take(&mut a.pending)).unwrap_or(0);
                    with(|a| {
                        if kinds & CHANGED_CONFIG != 0 {
                            a.reload_if_changed();
                        }
                        if kinds & (CHANGED_LOCK | CHANGED_CONFIG) != 0 {
                            a.running_changed();
                        }
                        if kinds & (CHANGED_USAGE | CHANGED_CONFIG) != 0 {
                            a.usage_changed();
                        }
                        if kinds & CHANGED_EVENT != 0 {
                            a.events_changed();
                        }
                    });
                    ui::refresh();
                }
                TIMER_WATCHDOG => {
                    with(|a| a.watchdog());
                }
                TIMER_NAG => {
                    KillTimer(hwnd, TIMER_NAG);
                    with(|a| {
                        let text = format!("Claude opens once {} is ready. Click the tray icon and choose Open now to skip.", a.cfg.wait_for_drive.path);
                        a.alert("Still waiting for Google Drive", &text, None, card::Action::Show);
                    });
                }
                TIMER_CODEX_FRONT => {
                    let job = CODEX_FRONT.with(|c| {
                        let mut c = c.borrow_mut();
                        let (name, n) = c.as_mut()?;
                        *n += 1;
                        Some((name.clone(), *n))
                    });
                    match job {
                        Some((name, n)) if n <= 20 => {
                            if codex_to_front(&name, n == 20) {
                                CODEX_FRONT.with(|c| *c.borrow_mut() = None);
                                KillTimer(hwnd, TIMER_CODEX_FRONT);
                            }
                        }
                        _ => {
                            CODEX_FRONT.with(|c| *c.borrow_mut() = None);
                            KillTimer(hwnd, TIMER_CODEX_FRONT);
                        }
                    }
                }
                TIMER_CODEX_FIND => {
                    if CODEX_FIND_LEFT.fetch_sub(1, Ordering::SeqCst) <= 1 {
                        KillTimer(hwnd, TIMER_CODEX_FIND);
                    }
                    if let Some(cfg) = with(|a| a.cfg.clone()) {
                        tags::sync(&cfg);
                        if cfg.codex.alerts {
                            codex::set_account_alerts(&cfg, true); // a sign-in's settings file may exist now
                        }
                    }
                }
                TIMER_DOCTOR => {
                    KillTimer(hwnd, TIMER_DOCTOR);
                    let found = with(|a| doctor::problems(&a.cfg)).unwrap_or_default();
                    for p in &found {
                        log(&format!("setup check: {p}"));
                    }
                    if let Some(first) = found.first() {
                        let more = if found.len() > 1 { format!(" ({} more in Settings > Check setup.)", found.len() - 1) } else { String::new() };
                        let (t, x) = (format!("{} setup check", crate::APP_NAME), format!("{first}{more}"));
                        with(|a| a.alert(&t, &x, None, card::Action::Show));
                    }
                    // Codex alerts were turned on but a Codex update rewrote its settings: add this app back.
                    if let Some(cfg) = with(|a| a.cfg.clone()) {
                        // (Also takes over Codex's own notifications; writes only what changed.)
                        if cfg.codex.alerts {
                            if let Err(e) = codex::set_alerts(&cfg, true) {
                                log(&format!("codex alerts: couldn't re-apply: {e}"));
                            }
                        }
                    }
                    // Build the chat index quietly now, so the first search is instant.
                    read_chats(0, 0);
                }
                TIMER_SUMMARY => {
                    KillTimer(hwnd, TIMER_SUMMARY);
                    with(|a| {
                        if a.summary_day != Some(App::today()) {
                            a.show_summary();
                        }
                        a.arm_summary();
                    });
                }
                TIMER_ICON => {
                    if with(|a| a.add_icon()) == Some(true) {
                        KillTimer(hwnd, TIMER_ICON);
                    }
                }
                _ => {}
            }
            0
        }
        WM_SETTINGCHANGE => {
            ui::theme_changed();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        // Monitors came or went: cards move to the right place.
        WM_DISPLAYCHANGE => {
            card::reposition();
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_POWERBROADCAST_ => {
            if wp == PBT_APMRESUMEAUTOMATIC {
                with(|a| a.catch_up_summary());
            }
            1
        }
        WM_QUERYENDSESSION => {
            with(|a| a.shutting_down = true);
            1
        }
        WM_ENDSESSION => {
            if wp == 0 {
                // Shutdown or sign-out was cancelled: the watchdog works again.
                with(|a| a.shutting_down = false);
            } else {
                with(|a| a.remove_icon());
            }
            0
        }
        WM_DESTROY => {
            tags::shutdown();
            card::close_all();
            with(|a| {
                a.remove_icon();
                UnregisterHotKey(a.hwnd, HOTKEY_ID);
            });
            PostQuitMessage(0);
            0
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::SeqCst) => {
            // Explorer (re)started: the old icon is gone.
            if with(|a| {
                a.icon_added = false;
                a.add_icon()
            }) == Some(true)
            {
                KillTimer(hwnd, TIMER_ICON);
            }
            0
        }
        WM_SHOWDASH => {
            ui::open_dashboard_on(wp.min(1));
            0
        }
        WM_COPYDATA => {
            let cd = &*(lp as *const windows_sys::Win32::System::DataExchange::COPYDATASTRUCT);
            if cd.dwData != COPY_EXPECT || cd.lpData.is_null() || cd.cbData as usize > 512 {
                return 0;
            }
            let text = std::slice::from_raw_parts(cd.lpData as *const u16, cd.cbData as usize / 2);
            let name = String::from_utf16_lossy(text);
            if with(|a| a.cfg.find(&name).is_some()).unwrap_or(false) {
                crate::tags::expect_window(&name);
            }
            1
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// WM_COPYDATA kind: "a window of this account is about to open" (from `--open`).
const COPY_EXPECT: usize = 1;

/// `--dashboard [memory]`: ask the running tray to open the dashboard (on its Usage or
/// Memory tab). False when it isn't running.
pub fn tell_dashboard(view: usize) -> bool {
    unsafe {
        let class = wide(CLASS);
        let h = FindWindowW(class.as_ptr(), null());
        if h.is_null() {
            return false;
        }
        AllowSetForegroundWindow(ASFW_ANY);
        PostMessageW(h, WM_SHOWDASH, view, 0) != 0
    }
}

/// Tell the running tray (from another process) that `name`'s window is about to open.
pub fn tell_expect(name: &str) {
    unsafe {
        let class = wide(CLASS);
        let h = FindWindowW(class.as_ptr(), null());
        if h.is_null() {
            return;
        }
        let text: Vec<u16> = name.encode_utf16().collect();
        let cd = windows_sys::Win32::System::DataExchange::COPYDATASTRUCT { dwData: COPY_EXPECT, cbData: (text.len() * 2) as u32, lpData: text.as_ptr() as *mut _ };
        let mut out = 0usize;
        SendMessageTimeoutW(h, WM_COPYDATA, 0, &cd as *const _ as LPARAM, SMTO_ABORTIFHUNG, 2000, &mut out);
    }
}

pub fn run(start: Start) {
    unsafe {
        let mutex_name = wide("Local\\Hive.Tray");
        let _mutex = CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Already running: show its flyout (or the Add form) instead.
            if !matches!(start, Start::Startup) {
                let class = wide(CLASS);
                let h = FindWindowW(class.as_ptr(), null());
                if !h.is_null() {
                    AllowSetForegroundWindow(ASFW_ANY);
                    let m = if matches!(start, Start::AddDialog) { WM_SHOWADD } else { WM_SHOWMENU };
                    PostMessageW(h, m, 0, 0);
                }
            }
            return;
        }

        let cfg = match Config::load_or_migrate(&config_path()) {
            Ok(c) => c,
            Err(e) => {
                msg_box(null_mut(), &format!("Couldn't read the settings file:\n{e}\n\nFix it and start {} again.", crate::APP_NAME), MB_ICONERROR);
                return;
            }
        };
        // After a rename (Claude Accounts, then Switchboard): the startup entry and Codex's notify setting
        // name this exe (each changes only when it still names the old one).
        move_startup_entry();
        if cfg.codex.alerts {
            if let Err(e) = codex::set_alerts(&cfg, true) {
                log(&format!("codex alerts: couldn't check the notify setting: {e}"));
            }
        }

        let hinst = GetModuleHandleW(null());
        let dpi = GetDpiForSystem();
        let res = 1usize as *const u16;
        let small = LoadImageW(hinst, res, IMAGE_ICON, GetSystemMetricsForDpi(SM_CXSMICON, dpi), GetSystemMetricsForDpi(SM_CYSMICON, dpi), 0) as HICON;
        let small = if small.is_null() { LoadIconW(null_mut(), IDI_APPLICATION) } else { small };
        let tray = tray_icon().unwrap_or(small);

        let class = wide(CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: small,
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let title = wide(crate::APP_NAME);
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        if hwnd.is_null() {
            return;
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::SeqCst);
        launcher::NOTIFY_HWND.store(hwnd as usize, Ordering::SeqCst);
        watch::start(hwnd as usize, account_dirs(&cfg));

        APP.with(|c| {
            *c.borrow_mut() = Some(App {
                hwnd,
                icon: tray,
                icon_added: false,
                cfg,
                cfg_mtime: mtime(),
                cfg_broken: false,
                waiting_since: None,
                seen_running: HashSet::new(),
                closed_at: HashMap::new(),
                quitting: HashMap::new(),
                finished_at: HashMap::new(),
                goal_hold: events::GoalHold::default(),
                last_relaunch: HashMap::new(),
                shutting_down: false,
                menu_names: Vec::new(),
                pending: 0,
                hotkey: String::new(),
                hotkeys_on: Vec::new(),
                hotkey_ids: HashMap::new(),
                hotkey_errors: HashMap::new(),
                levels: HashMap::new(),
                best: None,
                pending_open: None,
                pending_chat: None,
                events_at: 0,
                alerted: HashMap::new(),
                balloon_at: std::cell::Cell::new(None),
                summary_day: None,
                usage_now: Vec::new(),
                inbox_watch: card::InboxWatch::default(),
                inbox_checked: None,
                coordinator_seen: std::cell::RefCell::new(None),
                away: card::Away::default(),
                alarms: card::AlarmState::load(),
                alarm_checked: Instant::now(),
            })
        });
        events::trim();
        with(|a| {
            // Live alerts only for what happens from now on; unanswered questions are replayed below.
            a.events_at = std::fs::metadata(events::path()).map(|m| m.len()).unwrap_or(0);
            a.replay_unanswered();
            a.settings_applied();
            a.running_changed();
            a.usage_changed();
        });

        // Right after sign-in the taskbar may not exist yet: keep trying until it does.
        if with(|a| a.add_icon()) != Some(true) {
            SetTimer(hwnd, TIMER_ICON, 2000, None);
        }
        // Setup check once things have settled (the icon exists, logs are written).
        SetTimer(hwnd, TIMER_DOCTOR, 20_000, None);
        SetTimer(hwnd, TIMER_NUDGE, 60_000, None);
        SetTimer(hwnd, TIMER_CARD_SEEN, 2_000, None);
        SetTimer(hwnd, TIMER_MEMORY, 30 * 60_000, None);
        // A first harvest once startup has settled.
        if let Some(cfg) = with(|a| a.cfg.clone()) {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(120));
                crate::memory::harvest(&cfg);
            });
        }
        log(&format!(
            "tray started ({}, front window {})",
            match start { Start::Startup => "startup", Start::AddDialog => "add", Start::Normal => "manual" },
            crate::win::foreground_class()
        ));

        match start {
            Start::Startup => {
                with(begin_startup);
            }
            Start::AddDialog => {
                PostMessageW(hwnd, WM_SHOWADD, 0, 0);
            }
            Start::Normal => {
                // A double click on the exe shows the flyout. A deploy or script start must not:
                // the flyout takes the foreground and typing goes nowhere (card 105).
                let front = crate::win::foreground_class();
                if crate::win::start_may_take_foreground(&front) {
                    PostMessageW(hwnd, WM_SHOWMENU, 0, 0);
                } else {
                    log(&format!("start: left the foreground alone (front window {front}), flyout not shown"));
                }
            }
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        log("tray stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(name: &str, week: u8, five: u8, reset: Option<u64>) -> Standing {
        Standing { name: name.into(), week, five, reset, unc: 0 }
    }

    #[test]
    fn use_next_is_the_soonest_reset_that_still_has_room() {
        let all = vec![st("A", 10, 0, Some(500)), st("B", 60, 0, Some(100)), st("C", 96, 0, Some(50))];
        // C resets first but is past 95%: B (next to reset) wins over A (most room).
        assert_eq!(pick_best(&all, 95).as_deref(), Some("B"));
        // With a 97% threshold C still counts.
        assert_eq!(pick_best(&all, 97).as_deref(), Some("C"));
        // A full 5-hour window rules an account out for now.
        let all = vec![st("A", 10, 0, Some(500)), st("B", 60, 99, Some(100))];
        assert_eq!(pick_best(&all, 95).as_deref(), Some("A"));
        // Unknown reset dates go last; nobody under the line means no pick.
        let all = vec![st("A", 10, 0, None), st("B", 50, 0, Some(900))];
        assert_eq!(pick_best(&all, 95).as_deref(), Some("B"));
        assert_eq!(pick_best(&[st("A", 99, 0, None), st("B", 98, 0, None)], 95), None);
    }

    #[test]
    fn a_nearly_out_account_offers_use_nexts_pick_for_any_chat_and_any_account() {
        let a = |n: &str, w: u8, f: u8, r: u64| st(n, w, f, Some(r));
        let all = vec![a("Max", 96, 10, 900), a("Pro", 40, 0, 500), a("Spare", 10, 0, 100), a("Team", 20, 0, 50)];
        // 96%: the soonest reset with room, whatever kind the account is.
        assert_eq!(continue_target("Max", &all, 95).as_deref(), Some("Team"));
        assert_eq!(continue_target("Max", &all, 95), pick_best(&all.iter().filter(|s| s.name != "Max").cloned().collect::<Vec<_>>(), 95));
        // A Team chat at 96% is offered use_next's pick even when it is a personal account.
        let team = vec![a("Team", 96, 0, 50), a("Personal", 10, 0, 100), a("Other", 60, 0, 900)];
        assert_eq!(continue_target("Team", &team, 95).as_deref(), Some("Personal"));
        assert_eq!(continue_target("Team", &team, 95), pick_best(&team[1..], 95));
        // The 5-hour window counts too.
        assert_eq!(continue_target("Max", &[a("Max", 10, 97, 900), a("Pro", 40, 0, 500)], 95).as_deref(), Some("Pro"));
        // 80%: no button.
        assert_eq!(continue_target("Max", &[a("Max", 80, 80, 900), a("Pro", 40, 0, 500)], 95), None);
        // "Use next" off, or nobody with room: no button.
        assert_eq!(continue_target("Max", &all, 0), None);
        assert_eq!(continue_target("Max", &[a("Max", 99, 0, 1), a("Pro", 97, 0, 2)], 95), None);
    }
}
