//! Alert cards: this app's own notification, in the flyout's frosted style. A
//! small panel in the corner of the screen that stays up until you close it,
//! or for as long as you choose. (Windows' own notifications vanish after about
//! five seconds, and apps can't change that.) Cards stack, never take the
//! keyboard focus, pause their countdown while the mouse is over them, and have
//! buttons to act on them.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, InvalidateRect, MonitorFromPoint, ValidateRect, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::layout::Corner;
use crate::ui::{self, CardHit, CardView};
use crate::win::wide;

/// What clicking a card does.
#[derive(Clone)]
pub enum Action {
    /// Open this chat in its account: (account, chat id, title).
    Chat(String, String, String),
    /// Open (or bring forward) an account.
    Open(String),
    /// Show the flyout.
    Show,
    /// A sample card from Settings: the button does nothing.
    Preview,
    /// The "N more waiting" card: alerts that did not fit come back as cards close.
    Waiting,
    /// A chat alert whose account is nearly out: the button continues the chat in another
    /// account, a click on the card itself still opens the chat. (account, chat id, title, target, cli id).
    ContinueIn(String, String, String, String, String),
    /// A worker's inbox post: "Open coordinator" opens the coordinator chat (account, chat id, title).
    Coordinator(String, String, String),
    /// "While you were away": the button shows the flyout.
    Away,
}

impl Action {
    /// The action button's label; None for "just show the flyout" (click the card instead).
    fn label(&self) -> Option<String> {
        match self {
            Action::Chat(..) => Some("Open chat".into()),
            Action::Open(n) => Some(format!("Open {n}")),
            Action::Show | Action::Waiting => None,
            Action::Preview => Some("Open".into()),
            Action::ContinueIn(_, _, _, target, _) => Some(format!("Continue in {target}")),
            Action::Coordinator(..) => Some("Open coordinator".into()),
            Action::Away => Some("Show".into()),
        }
    }
}

/// What a click on a card runs. The button of a "Continue in" card continues the chat; the
/// rest of the card (and every other button) opens the chat as it always did.
fn click_action(a: Action, hit: Option<CardHit>) -> Action {
    match a {
        Action::ContinueIn(account, id, title, ..) if hit != Some(CardHit::Primary) => Action::Chat(account, id, title),
        a => a,
    }
}

struct Card {
    hwnd: HWND,
    /// The account (or "Codex") it is about: clicking into that window closes it.
    account: Option<String>,
    title: String,
    text: String,
    badge: Option<(String, u32)>,
    action: Action,
    primary: Option<String>,
    /// Seconds it stays up; 0 = until closed.
    stay: u32,
    /// Milliseconds left (counts down only while the mouse is away).
    left: i64,
    hover: Option<CardHit>,
    tracking: bool,
    /// Clickable areas from the last paint, in DIPs.
    hits: Vec<([f32; 4], CardHit)>,
    /// Height in pixels.
    h: i32,
    /// The chat it is about (account, chat id): a newer alert for it replaces this card.
    chat: Option<(String, String)>,
    /// What happened ("notify", "ask", "stop", "codex"); empty for other alerts.
    kind: String,
    /// How many alerts this card stands for (a chat's newer ones replace it).
    count: u32,
    /// When it last showed something new (ms): opening its chat after this closes it.
    at_ms: u64,
}

/// An alert that did not fit on screen, waiting for a free spot.
struct Queued {
    account: Option<String>,
    title: String,
    text: String,
    badge: Option<(String, u32)>,
    action: Action,
    stay: u32,
    left: i64,
    chat: Option<(String, String)>,
    kind: String,
    count: u32,
    /// When it last had something new (ms).
    at_ms: u64,
}

impl Queued {
    fn new(title: &str, text: &str, badge: Option<(String, u32)>, action: Action, stay: u32) -> Self {
        let account = match &action {
            Action::Chat(a, ..) | Action::Open(a) | Action::ContinueIn(a, ..) | Action::Coordinator(a, ..) if !a.is_empty() => Some(a.clone()),
            _ => None,
        };
        Queued { account, title: title.into(), text: text.into(), badge, action, stay, left: stay as i64 * 1000, chat: None, kind: String::new(), count: 1, at_ms: crate::time::now_ms() }
    }

    /// Take a newer alert for the same chat: its words, its kind, a fresh stay-up time.
    /// A repeat of the very same words is not counted again.
    fn update(&mut self, n: &Queued) {
        if n.title != self.title || n.text != self.text {
            self.count += 1;
        }
        self.title = n.title.clone();
        self.text = n.text.clone();
        self.badge = n.badge.clone();
        self.action = n.action.clone();
        self.kind = n.kind.clone();
        self.stay = n.stay;
        self.left = n.left;
        self.at_ms = n.at_ms;
    }
}

/// The key a chat alert is merged on. Alerts with no chat id are never merged.
fn chat_key(account: &str, chat: &str) -> Option<(String, String)> {
    (!chat.is_empty()).then(|| (account.to_string(), chat.to_string()))
}

thread_local! {
    static CARDS: RefCell<Vec<Card>> = const { RefCell::new(Vec::new()) };
    /// Oldest first.
    static WAITING: RefCell<Vec<Queued>> = const { RefCell::new(Vec::new()) };
    static CLASS_READY: Cell<bool> = const { Cell::new(false) };
    static KEPT_LOGGED: RefCell<std::collections::HashMap<String, u64>> = RefCell::new(std::collections::HashMap::new());
    static GATE: RefCell<Option<Gate>> = const { RefCell::new(None) };
}

const CLASS: &str = "HiveCard";
const MAX_CARDS: usize = 4;
const TICK_MS: u32 = 200;
const TIMER: usize = 1;
const WM_MOUSELEAVE_: u32 = 0x02A3;

thread_local! {
    /// The last card position read from the settings (the settings are busy during a re-entrant call).
    static POSITION: RefCell<(String, Option<usize>, String)> = RefCell::new((String::new(), None, "bottom_right".into()));
}

/// (monitor device name, its left-to-right index, corner) from the settings.
fn position() -> (String, Option<usize>, String) {
    if let Some(p) = crate::tray::card_position() {
        POSITION.with(|c| *c.borrow_mut() = p);
    }
    POSITION.with(|c| c.borrow().clone())
}

/// The chosen monitor's usable area and scale, and the corner cards stack from. A missing
/// monitor falls back to the primary one with the same corner (no error).
fn screen() -> (RECT, f32, Corner) {
    let (name, index, corner) = position();
    let corner = Corner::parse(&corner);
    let mons = crate::layout::card_monitors();
    if let Some(m) = crate::layout::choose_monitor(&mons, &name, index).and_then(|i| mons.get(i)) {
        let w = m.work;
        return (RECT { left: w[0], top: w[1], right: w[2], bottom: w[3] }, crate::win::debug_scale().unwrap_or(m.dpi as f32 / 96.0), corner);
    }
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (crate::win::usable_area(mi.rcWork), crate::win::debug_scale().unwrap_or(dx as f32 / 96.0), corner)
    }
}

/// `--card-demo <monitor index> <corner> <seconds>`: show the sample card in that spot for a few
/// seconds and exit (for checking placement; the tray need not run, nothing is saved).
pub fn demo(args: &[String]) {
    let mons = crate::layout::card_monitors();
    let idx: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(0);
    let corner = args.get(1).cloned().unwrap_or_else(|| "bottom_right".into());
    let secs: u64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(6);
    if let Some(m) = mons.get(idx) {
        POSITION.with(|c| *c.borrow_mut() = (m.name.clone(), Some(idx), corner));
    }
    show(
        "This is an alert card",
        "When a chat finishes or needs you, a card like this waits here. Open acts on it; Dismiss or the X closes it.",
        Some(("P".into(), 0xC9922E)),
        Action::Preview,
        0,
    );
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while std::time::Instant::now() < end {
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// Show a card. `stay` is seconds on screen, 0 for until it is closed.
pub fn show(title: &str, text: &str, badge: Option<(String, u32)>, action: Action, stay: u32) {
    if let Some(card) = make(Queued::new(title, text, badge, action, stay)) {
        CARDS.with(|c| c.borrow_mut().push(card));
    }
    restack();
}

/// The context alarm and the worker watchdog cards: they keep their own slot, so they never
/// replace a chat's "finished" or "needs you" card and are never replaced by one.
fn is_alarm_kind(kind: &str) -> bool {
    kind == "context" || kind == "quiet"
}

/// What a chat's state card (on screen or waiting) last said happened, if it has one.
/// Alarm cards are not state cards.
pub fn chat_kind(account: &str, chat: &str) -> Option<String> {
    let key = chat_key(account, chat)?;
    let on = CARDS.with(|c| c.borrow().iter().find(|k| k.chat.as_ref() == Some(&key) && !is_alarm_kind(&k.kind)).map(|k| k.kind.clone()));
    on.or_else(|| WAITING.with(|w| w.borrow().iter().find(|q| q.chat.as_ref() == Some(&key) && !is_alarm_kind(&q.kind)).map(|q| q.kind.clone())))
}

/// Chat cards on screen: (account, chat id, when they last showed something new, ms).
pub fn chat_cards() -> Vec<(String, String, u64)> {
    let mut v: Vec<(String, String, u64)> = CARDS.with(|c| c.borrow().iter().filter_map(|k| k.chat.clone().map(|(a, id)| (a, id, k.at_ms))).collect());
    WAITING.with(|w| v.extend(w.borrow().iter().filter_map(|q| q.chat.clone().map(|(a, id)| (a, id, q.at_ms)))));
    v
}

/// Close a chat's card (you opened that chat yourself).
pub fn close_chat(account: &str, chat: &str) {
    let Some(key) = chat_key(account, chat) else { return };
    let hwnds: Vec<HWND> = CARDS.with(|c| c.borrow().iter().filter(|k| k.chat.as_ref() == Some(&key)).map(|k| k.hwnd).collect());
    if !hwnds.is_empty() {
        for h in hwnds {
            close(h);
        }
        WAITING.with(|w| w.borrow_mut().retain(|q| q.chat.as_ref() != Some(&key)));
        return restack();
    }
    let gone = WAITING.with(|w| {
        let mut w = w.borrow_mut();
        let n = w.len();
        w.retain(|q| q.chat.as_ref() != Some(&key));
        n != w.len()
    });
    if gone {
        restack();
    }
}

/// A chat alert: one card per chat. A newer alert for a chat that already has a card
/// (on screen or waiting) replaces what that card says, in its place in the stack,
/// and restarts its time (2026-10-15: show only the latest one for that chat).
pub fn show_chat(chat: &str, kind: &str, title: &str, text: &str, badge: Option<(String, u32)>, action: Action, stay: u32) {
    let mut q = Queued::new(title, text, badge, action, stay);
    q.kind = kind.into();
    q.chat = chat_key(q.account.as_deref().unwrap_or(""), chat);
    let Some(key) = q.chat.clone() else {
        if let Some(card) = make(q) {
            CARDS.with(|c| c.borrow_mut().push(card));
        }
        return restack();
    };
    let h = card_size(&q.text, q.action.label().is_some(), true).1;
    let on_screen = CARDS.with(|c| {
        let mut c = c.borrow_mut();
        let k = c.iter_mut().find(|k| k.chat.as_ref() == Some(&key) && is_alarm_kind(&k.kind) == is_alarm_kind(&q.kind))?;
        if q.title != k.title || q.text != k.text {
            k.count += 1;
        }
        k.title = q.title.clone();
        k.text = q.text.clone();
        k.badge = q.badge.clone();
        k.primary = q.action.label();
        k.action = q.action.clone();
        k.kind = q.kind.clone();
        k.stay = q.stay;
        k.left = q.left;
        k.h = h;
        k.at_ms = crate::time::now_ms();
        Some((k.hwnd, k.stay, k.count))
    });
    if let Some((hwnd, stay, count)) = on_screen {
        unsafe {
            SetWindowTextW(hwnd, wide(title).as_ptr());
            if stay > 0 {
                SetTimer(hwnd, TIMER, TICK_MS, None);
            } else {
                KillTimer(hwnd, TIMER);
            }
            InvalidateRect(hwnd, null(), 0);
        }
        crate::log(&format!("card for this chat updated in place ({count} alerts)"));
        return restack();
    }
    let waiting = WAITING.with(|w| {
        let mut w = w.borrow_mut();
        let x = w.iter_mut().find(|x| x.chat.as_ref() == Some(&key) && is_alarm_kind(&x.kind) == is_alarm_kind(&q.kind))?;
        x.update(&q);
        Some(x.count)
    });
    if let Some(count) = waiting {
        crate::log(&format!("waiting card for this chat updated ({count} alerts)"));
        return restack();
    }
    if let Some(card) = make(q) {
        CARDS.with(|c| c.borrow_mut().push(card));
    }
    restack();
}

/// A card's window (not shown yet).
fn make(q: Queued) -> Option<Card> {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide(CLASS);
        if !CLASS_READY.with(|c| c.replace(true)) {
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(proc_),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_HAND),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
        }
        let primary = q.action.label();
        let (w, h) = card_size(&q.text, primary.is_some(), q.chat.is_some());
        // A captioned popup with no frame: the shape Windows needs to give it frosted glass.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class.as_ptr(),
            wide(&q.title).as_ptr(),
            WS_POPUP | WS_CAPTION,
            0,
            0,
            w,
            h,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return None;
        }
        ui::glass(hwnd);
        SendMessageW(hwnd, WM_NCACTIVATE, 1, 0);
        Some(Card {
            hwnd,
            account: q.account,
            title: q.title,
            text: q.text,
            badge: q.badge,
            action: q.action,
            primary,
            stay: q.stay,
            left: q.left,
            hover: None,
            tracking: false,
            hits: Vec::new(),
            h,
            chat: q.chat,
            kind: q.kind,
            count: q.count,
            at_ms: q.at_ms,
        })
    }
}

/// A card's size in pixels on the primary screen.
fn card_size(text: &str, buttons: bool, snooze: bool) -> (i32, i32) {
    let (_, s, _) = screen();
    ((ui::CARD_W * s).round() as i32, (ui::card_height(text, buttons, snooze) * s).round() as i32)
}

fn is_summary(k: &Card) -> bool {
    matches!(k.action, Action::Waiting)
}

/// Heights of the alert cards on screen (not the summary), oldest first.
fn alert_heights() -> Vec<i32> {
    CARDS.with(|c| c.borrow().iter().filter(|k| !is_summary(k)).map(|k| k.h).collect())
}

/// Fit the stack to the screen: alerts that don't fit wait (none is lost), waiting
/// ones come back as soon as there is room, and one card on top counts those still
/// waiting. Then place and show them all.
fn restack() {
    let (work, s, _) = screen();
    // A card on another monitor (the setting changed, or monitors came or went) may have been sized
    // for a different scale: resize its pixel height from its DIPs.
    CARDS.with(|c| {
        for k in c.borrow_mut().iter_mut() {
            let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(k.hwnd) };
            if dpi >= 96 && k.h > 0 {
                k.h = ((k.h as f32 * 96.0 / dpi as f32) * s).round() as i32;
            }
        }
    });
    let room = work.bottom - work.top - (24.0 * s) as i32;
    let gap = (8.0 * s) as i32;
    // The summary card's height: the most a card's text can take (titles may wrap).
    let summary_h = card_size("a\nb\nc\nd\ne\nf\ng\nh\ni", false, false).1;
    let fits = |hs: &[i32], summary: bool| {
        let n = hs.len() + summary as usize;
        n <= MAX_CARDS && hs.iter().sum::<i32>() + if summary { summary_h } else { 0 } + gap * n.saturating_sub(1) as i32 <= room
    };
    // Room again: the newest waiting alert comes back.
    while let Some(q) = WAITING.with(|w| w.borrow_mut().pop()) {
        let still_waiting = WAITING.with(|w| !w.borrow().is_empty());
        let mut hs = alert_heights();
        hs.push(card_size(&q.text, q.action.label().is_some(), q.chat.is_some()).1);
        if !fits(&hs, still_waiting) {
            WAITING.with(|w| w.borrow_mut().push(q));
            break;
        }
        if let Some(card) = make(q) {
            // Above the newer ones it waited behind, under the summary.
            CARDS.with(|c| {
                let mut c = c.borrow_mut();
                let at = c.iter().position(|k| !is_summary(k)).unwrap_or(c.len());
                c.insert(at, card);
            });
        }
    }
    // Too many: the oldest alerts go to wait. The newest always shows.
    loop {
        let hs = alert_heights();
        let waiting = WAITING.with(|w| !w.borrow().is_empty());
        if hs.len() <= 1 || fits(&hs, waiting) {
            break;
        }
        let gone = CARDS.with(|c| {
            let mut c = c.borrow_mut();
            // The oldest card waits, but never the one that just changed (an update in
            // place can make a card taller): its news must stay in sight.
            let newest = c.iter().filter(|k| !is_summary(k)).map(|k| k.at_ms).max();
            let i = c.iter().position(|k| !is_summary(k) && Some(k.at_ms) != newest)?;
            Some(c.remove(i))
        });
        let Some(k) = gone else { break };
        unsafe {
            KillTimer(k.hwnd, TIMER);
            DestroyWindow(k.hwnd);
        }
        let q = Queued { account: k.account, title: k.title, text: k.text, badge: k.badge, action: k.action, stay: k.stay, left: k.left, chat: k.chat, kind: k.kind, count: k.count, at_ms: k.at_ms };
        // Newer than everything already waiting: it goes last (oldest first).
        WAITING.with(|w| w.borrow_mut().push(q));
    }
    sync_summary();
    layout();
    let shown: Vec<(HWND, u32)> = CARDS.with(|c| c.borrow().iter().map(|k| (k.hwnd, k.stay)).collect());
    for (hwnd, stay) in shown {
        unsafe {
            if IsWindowVisible(hwnd) == 0 {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                if stay > 0 {
                    SetTimer(hwnd, TIMER, TICK_MS, None);
                }
            }
        }
    }
}

/// The "N more waiting" card on top of the stack: there exactly while alerts wait.
fn sync_summary() {
    // "Team has a question: Chat name" (a chat alert's first line is the chat's name).
    let line = |q: &Queued| {
        let chat = q.chat.as_ref().and_then(|_| q.text.lines().next()).map(str::trim).filter(|c| !c.is_empty());
        match chat {
            Some(c) => format!("{}: {}", q.title, c.chars().take(28).collect::<String>()),
            None => q.title.clone(),
        }
    };
    let lines: Vec<String> = WAITING.with(|w| w.borrow().iter().rev().map(line).collect());
    let text = if lines.is_empty() {
        None
    } else {
        let mut t: Vec<String> = lines.iter().take(3).cloned().collect();
        if lines.len() > 3 {
            t.push(format!("and {} more", lines.len() - 3));
        }
        t.push("They show here as you close these.".into());
        Some((format!("{} more waiting", lines.len()), t.join("\n")))
    };
    let current = CARDS.with(|c| c.borrow().iter().find(|k| is_summary(k)).map(|k| (k.title.clone(), k.text.clone())));
    if current == text {
        return;
    }
    let old = CARDS.with(|c| {
        let mut c = c.borrow_mut();
        let i = c.iter().position(is_summary)?;
        Some(c.remove(i))
    });
    if let Some(k) = old {
        unsafe { DestroyWindow(k.hwnd) };
    }
    let Some((title, text)) = text else { return };
    let q = Queued::new(&title, &text, None, Action::Waiting, 0);
    if let Some(card) = make(q) {
        CARDS.with(|c| c.borrow_mut().insert(0, card));
    }
}

/// Stack the cards from the chosen corner of the chosen monitor, newest in the corner.
fn layout() {
    let (work, s, corner) = screen();
    let w = (ui::CARD_W * s).round() as i32;
    let gap = (8.0 * s) as i32;
    let margin = (12.0 * s) as i32;
    let list: Vec<(HWND, i32)> = CARDS.with(|c| c.borrow().iter().map(|k| (k.hwnd, k.h)).collect());
    let hs: Vec<i32> = list.iter().map(|x| x.1).collect();
    let rects = crate::layout::stack_rects([work.left, work.top, work.right, work.bottom], corner, &hs, w, margin, gap);
    for ((hwnd, h), r) in list.into_iter().zip(rects) {
        // FRAMECHANGED: have Windows drop the title bar the caption style would otherwise reserve.
        unsafe { SetWindowPos(hwnd, HWND_TOPMOST, r[0], r[1], w, h, SWP_NOACTIVATE | SWP_FRAMECHANGED) };
    }
}

/// Put the cards where the settings say now (a setting changed, or monitors came or went).
pub fn reposition() {
    let any = CARDS.with(|c| !c.borrow().is_empty());
    if any {
        restack();
    }
}

/// You closed this card yourself (X, Dismiss, Open or right-click): a needs-me or question
/// card must not come back after a tray restart.
fn forget_on_restart(hwnd: HWND) {
    let chat = CARDS.with(|c| c.borrow().iter().find(|k| k.hwnd == hwnd).filter(|k| k.kind == "notify" || k.kind == "ask").and_then(|k| k.chat.clone()));
    if let Some((_, id)) = chat {
        crate::events::mark_dismissed(&id, crate::time::now());
    }
}

fn close(hwnd: HWND) {
    let removed = CARDS.with(|c| {
        let mut c = c.borrow_mut();
        let i = c.iter().position(|k| k.hwnd == hwnd)?;
        Some(is_summary(&c.remove(i)))
    });
    if let Some(summary) = removed {
        unsafe { DestroyWindow(hwnd) };
        if summary {
            WAITING.with(|w| w.borrow_mut().clear()); // its X: let the waiting ones go
        }
        restack();
    }
    if CARDS.with(|c| c.borrow().is_empty()) {
        ui::card_gfx_release(); // nothing on screen: give the drawing memory back
    }
}

thread_local! {
    /// A card was just clicked: (its account, when). The window that click brings
    /// forward must not close that account's other cards.
    static CLICKED: RefCell<Option<(String, std::time::Instant)>> = const { RefCell::new(None) };
}

/// Chats with an alert card up or waiting: (account, chat id). Tidy up leaves these alone.
pub fn chats_with_cards() -> std::collections::HashSet<(String, String)> {
    let of = |a: &Action| match a {
        Action::Chat(account, id, _) | Action::ContinueIn(account, id, ..) => Some((account.clone(), id.clone())),
        _ => None,
    };
    let mut out: std::collections::HashSet<(String, String)> = CARDS.with(|c| c.borrow().iter().filter_map(|c| of(&c.action)).collect());
    WAITING.with(|w| out.extend(w.borrow().iter().filter_map(|q| of(&q.action))));
    out
}

/// You brought an account's window forward yourself. Its card has done its job
/// when it is that account's only one; with several waiting (each a different
/// chat) they all stay. Other accounts' cards are never touched.
pub fn dismiss_account(name: &str) {
    let ours = CLICKED.with(|c| c.borrow().as_ref().is_some_and(|(a, at)| a == name && at.elapsed() < std::time::Duration::from_secs(2)));
    if ours {
        return; // the window our own card click brought forward
    }
    let shown: Vec<HWND> = CARDS.with(|c| c.borrow().iter().filter(|k| k.account.as_deref() == Some(name)).map(|k| k.hwnd).collect());
    let queued = WAITING.with(|w| w.borrow().iter().filter(|q| q.account.as_deref() == Some(name)).count());
    match (shown.len(), queued) {
        (0, 0) => {}
        (1, 0) => {
            crate::log(&format!("{name} in front: closed its card"));
            close(shown[0]);
        }
        (0, 1) => {
            WAITING.with(|w| w.borrow_mut().retain(|q| q.account.as_deref() != Some(name)));
            restack();
        }
        (n, q) => {
            // One line per account per 10 minutes: it was 131 lines in two days.
            let now = crate::time::now_ms();
            let due = KEPT_LOGGED.with(|m| {
                let mut m = m.borrow_mut();
                let due = m.get(name).is_none_or(|t| now.saturating_sub(*t) >= SUPPRESS_LOG_MS);
                if due {
                    m.insert(name.to_string(), now);
                }
                due
            });
            if due {
                crate::log(&format!("{name} in front: kept its {} cards (several chats waiting)", n + q));
            }
        }
    }
}

/// Close every card (the app is exiting).
pub fn close_all() {
    WAITING.with(|w| w.borrow_mut().clear());
    let all: Vec<HWND> = CARDS.with(|c| c.borrow_mut().drain(..).map(|k| k.hwnd).collect());
    for w in all {
        unsafe { DestroyWindow(w) };
    }
    ui::card_gfx_release();
}

/// What is under the mouse (pixels in, DIP hit areas).
fn hit_at(hwnd: HWND, lp: LPARAM) -> Option<CardHit> {
    let (x, y) = ((lp & 0xFFFF) as i16 as f32, ((lp >> 16) & 0xFFFF) as i16 as f32);
    let s = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    let (x, y) = (x / s, y / s);
    CARDS.with(|c| {
        let c = c.borrow();
        let k = c.iter().find(|k| k.hwnd == hwnd)?;
        // The most specific area wins: buttons are listed after the whole card.
        k.hits.iter().rev().find(|(r, _)| x >= r[0] && x < r[2] && y >= r[1] && y < r[3]).map(|(_, h)| *h)
    })
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wp != 0 => 0, // the whole window is client area
        // Never draw an (inactive) title bar: the card is never the active window.
        // Always "active" to Windows, so the frosted backdrop stays (it goes solid when inactive).
        WM_NCACTIVATE => DefWindowProcW(hwnd, msg, 1, lp),
        WM_PAINT => {
            let view = CARDS.with(|c| {
                c.borrow().iter().find(|k| k.hwnd == hwnd).map(|k| {
                    (k.title.clone(), k.text.clone(), k.badge.clone(), k.primary.clone(), k.stay, k.left, k.hover, k.count, k.chat.is_some(), k.chat.as_ref().is_some_and(|(_, id)| id.starts_with("local_")))
                })
            });
            match view {
                Some((title, text, badge, primary, stay, left, hover, count, snooze, mute)) => {
                    let v = CardView {
                        title: &title,
                        text: &text,
                        badge: badge.as_ref().map(|(t, c)| (t.as_str(), *c)),
                        primary: primary.as_deref(),
                        stay,
                        left,
                        hover,
                        count,
                        snooze,
                        mute,
                    };
                    let hits = ui::paint_card(hwnd, &v);
                    CARDS.with(|c| {
                        if let Some(k) = c.borrow_mut().iter_mut().find(|k| k.hwnd == hwnd) {
                            k.hits = hits;
                        }
                    });
                }
                None => {
                    ValidateRect(hwnd, null());
                }
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => 3, // MA_NOACTIVATE: clicking never steals focus from your work
        WM_MOUSEMOVE => {
            let h = hit_at(hwnd, lp);
            let (track, changed) = CARDS.with(|c| {
                let mut c = c.borrow_mut();
                let Some(k) = c.iter_mut().find(|k| k.hwnd == hwnd) else { return (false, false) };
                let track = !k.tracking;
                k.tracking = true;
                let changed = k.hover != h;
                k.hover = h;
                (track, changed)
            });
            if track {
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut t);
            }
            if changed {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE_ => {
            CARDS.with(|c| {
                if let Some(k) = c.borrow_mut().iter_mut().find(|k| k.hwnd == hwnd) {
                    k.hover = None;
                    k.tracking = false;
                }
            });
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let h = hit_at(hwnd, lp);
            let action = CARDS.with(|c| c.borrow().iter().find(|k| k.hwnd == hwnd).map(|k| k.action.clone()));
            if matches!(action, Some(Action::Waiting)) && !matches!(h, Some(CardHit::Close)) {
                return 0; // the summary closes only with its X (or a right-click)
            }
            let (title, account) = CARDS.with(|c| c.borrow().iter().find(|k| k.hwnd == hwnd).map(|k| (k.title.clone(), k.account.clone()))).unwrap_or_default();
            if matches!(h, Some(CardHit::Snooze) | Some(CardHit::Mute)) {
                let chat = CARDS.with(|c| c.borrow().iter().find(|k| k.hwnd == hwnd).and_then(|k| k.chat.clone().map(|key| (key, k.kind.clone()))));
                if let Some(((account, id), kind)) = chat {
                    if h == Some(CardHit::Snooze) {
                        snooze_chat(&account, &id);
                    } else {
                        mute_chat(&account, &id, &kind);
                    }
                    forget_on_restart(hwnd);
                    close(hwnd);
                }
                return 0;
            }
            forget_on_restart(hwnd);
            close(hwnd);
            if matches!(h, Some(CardHit::Primary) | Some(CardHit::Body)) {
                if let Some(a) = action {
                    crate::log(&format!("card clicked: {title}"));
                    if let Some(acc) = account {
                        CLICKED.with(|c| *c.borrow_mut() = Some((acc, std::time::Instant::now())));
                    }
                    crate::tray::card_action(click_action(a, h));
                }
            }
            0
        }
        WM_RBUTTONUP => {
            forget_on_restart(hwnd);
            close(hwnd); // right-click dismisses
            0
        }
        // Closed from outside (a test, or Windows): keep the stack in order.
        WM_CLOSE => {
            close(hwnd);
            0
        }
        WM_TIMER => {
            let expired = CARDS.with(|c| {
                let mut c = c.borrow_mut();
                let k = c.iter_mut().find(|k| k.hwnd == hwnd)?;
                if k.hover.is_none() {
                    k.left -= TICK_MS as i64;
                }
                Some(k.left <= 0)
            });
            match expired {
                Some(true) => close(hwnd),
                Some(false) => {
                    InvalidateRect(hwnd, null(), 0);
                }
                None => {
                    KillTimer(hwnd, TIMER);
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- inbox posts and the away summary -------------------------------------------------

/// A post that needs a card: a worker is blocked or has a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxPost {
    pub to: String,
    pub card: String,
    pub status: String,
    pub text: String,
}

impl InboxPost {
    /// "Card 120 worker is blocked" / "Card 120 worker has a question".
    pub fn title(&self) -> String {
        let id: String = self.card.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).take(16).collect();
        let who = if id.is_empty() { "A worker".to_string() } else { format!("Card {id} worker") };
        if self.status == "question" {
            format!("{who} has a question")
        } else {
            format!("{who} is blocked")
        }
    }
}

/// Posts older than this are not news (the tray was off, or nobody looked).
const INBOX_MAX_AGE_SECS: u64 = 30 * 60;

/// What one look at the inbox folder found.
#[derive(Default, Debug)]
pub struct InboxFound {
    /// Blocked and question posts that get a card.
    pub raise: Vec<InboxPost>,
    /// Every fresh new post (for the away summary).
    pub fresh: u32,
}

/// Watches the inbox folder. Each recipient file is read again only when its size or time
/// changes; what was raised is remembered in `<to>.tray` (a byte offset), so a restart does
/// not raise it again. Posts the recipient's own hook already read (`<to>.cursor`) are skipped.
#[derive(Default)]
pub struct InboxWatch {
    /// recipient -> (size, modified, offset handled up to)
    seen: std::collections::HashMap<String, (u64, Option<std::time::SystemTime>, u64)>,
}

fn offset_in(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

impl InboxWatch {
    pub fn poll(&mut self, dir: &std::path::Path, now: u64) -> InboxFound {
        use std::io::{Read, Seek, SeekFrom};
        let mut out = InboxFound::default();
        let Ok(rd) = std::fs::read_dir(dir) else { return out };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(to) = name.strip_suffix(".jsonl") else { continue };
            let Ok(md) = e.metadata() else { continue };
            let (len, mt) = (md.len(), md.modified().ok());
            let known = self.seen.get(to).copied();
            if known.is_some_and(|(l, m, _)| l == len && m == mt) {
                continue; // nothing changed: no file is opened
            }
            let tray_file = dir.join(format!("{to}.tray"));
            let cursor = offset_in(&dir.join(format!("{to}.cursor")));
            let mut start = match known {
                Some((_, _, o)) => o.max(cursor),
                None if tray_file.exists() => offset_in(&tray_file).max(cursor),
                // First sight of this file with no record of ours: old posts are history.
                None => len,
            };
            if start > len {
                start = 0; // the file was replaced
            }
            // A saved offset must sit at a line start; otherwise the file was re-created: read it all.
            if start > 0 && start < len {
                let mut b = [0u8; 1];
                let ok = std::fs::File::open(e.path()).and_then(|mut f| {
                    f.seek(SeekFrom::Start(start - 1))?;
                    f.read_exact(&mut b)
                });
                if ok.is_err() || b[0] != b'\n' {
                    start = 0;
                }
            }
            let mut end = start;
            if len > start {
                let mut buf = Vec::new();
                let read = std::fs::File::open(e.path()).and_then(|mut f| {
                    f.seek(SeekFrom::Start(start))?;
                    f.take(len - start).read_to_end(&mut buf)
                });
                if read.is_ok() {
                    for chunk in buf.split_inclusive(|b| *b == b'\n') {
                        if !chunk.ends_with(b"\n") {
                            break; // a post still being written
                        }
                        end += chunk.len() as u64;
                        let Ok(v) = serde_json::from_slice::<serde_json::Value>(chunk) else { continue };
                        let at = v.get("at").and_then(|x| x.as_u64()).unwrap_or(0);
                        if now.saturating_sub(at) > INBOX_MAX_AGE_SECS {
                            continue;
                        }
                        out.fresh += 1;
                        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let status = s("status");
                        if status == "blocked" || status == "question" {
                            out.raise.push(InboxPost { to: to.to_string(), card: s("card"), status, text: s("text") });
                        }
                    }
                }
            }
            self.seen.insert(to.to_string(), (len, mt, end));
            if end != start || known.is_none() {
                let tmp = dir.join(format!("{to}.tray.{}.tmp", std::process::id()));
                if std::fs::write(&tmp, end.to_string()).is_ok() && std::fs::rename(&tmp, &tray_file).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
        out
    }
}

/// How long you are idle before it counts as being away.
pub const AWAY_AFTER_SECS: u64 = 20 * 60;
/// Idle below this means you are at the PC.
const AT_PC_SECS: u64 = 60;

/// Counts what happened while you were idle, and says so when you come back.
#[derive(Default, Debug)]
pub struct Away {
    peak_idle: u64,
    finished: std::collections::HashSet<String>,
    waiting: std::collections::HashSet<String>,
    pub inbox: u32,
}

impl Away {
    /// A chat finished (`waiting` false) or wants you (true), while idle for `idle` seconds.
    /// `key` is the chat's id: one chat counts once however many events it sends.
    pub fn chat(&mut self, key: &str, waiting: bool, idle: u64) {
        if idle < AT_PC_SECS {
            return;
        }
        if waiting {
            self.waiting.insert(key.to_string());
        } else {
            self.finished.insert(key.to_string());
        }
    }

    pub fn posts(&mut self, n: u32, idle: u64) {
        if idle >= AT_PC_SECS {
            self.inbox += n;
        }
    }

    /// Call every couple of seconds. Returns (finished, waiting, inbox) once, when you are back
    /// after at least 20 minutes away and something happened; None otherwise.
    pub fn tick(&mut self, idle: u64) -> Option<(u32, u32, u32)> {
        if idle >= AT_PC_SECS {
            self.peak_idle = self.peak_idle.max(idle);
            return None;
        }
        let was_away = self.peak_idle >= AWAY_AFTER_SECS;
        let counts = (self.finished.len() as u32, self.waiting.len() as u32, self.inbox);
        *self = Away::default();
        (was_away && counts != (0, 0, 0)).then_some(counts)
    }
}

/// "3 chats finished, 1 waiting, 2 inbox posts".
pub fn away_text(finished: u32, waiting: u32, inbox: u32) -> String {
    let n = |c: u32, one: &str, many: &str| format!("{c} {}", if c == 1 { one } else { many });
    format!("{}, {waiting} waiting, {}", n(finished, "chat finished", "chats finished"), n(inbox, "inbox post", "inbox posts"))
}

// ---- repeat alerts: chime limit, snooze, mute ---------------------------------------

/// A chat chimes at most once per kind per this long (cards still merge and update).
const CHIME_EVERY_MS: u64 = 10 * 60 * 1000;
/// A suppressed alert is logged once per kind per chat per this long.
const SUPPRESS_LOG_MS: u64 = 10 * 60 * 1000;
/// "Snooze 1 h".
const SNOOZE_MS: u64 = 60 * 60 * 1000;

type ChatKey = (String, String);

/// "ask" and "notify" both mean the chat is waiting for you; other kinds stand alone.
fn wait_class(kind: &str) -> &str {
    if kind == "ask" || kind == "notify" {
        "wait"
    } else {
        kind
    }
}

/// What to do with an alert for a chat.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Show it (merging into the chat's card); `chime` says whether it may sound.
    Show { chime: bool },
    /// Snoozed or muted: no card, no chime. `log` is true once per kind per 10 minutes.
    Hidden { log: bool },
}

#[derive(Default)]
struct Gate {
    /// (chat, kind) -> when it last chimed.
    chimed: std::collections::HashMap<(ChatKey, String), u64>,
    /// chat -> snooze ends at (ms since the epoch).
    snoozed: std::collections::HashMap<ChatKey, u64>,
    /// chat -> (kind when muted, when muted).
    muted: std::collections::HashMap<ChatKey, (String, u64)>,
    /// (chat, kind) -> when a suppressed alert was last logged.
    logged: std::collections::HashMap<(ChatKey, String), u64>,
}

impl Gate {
    /// Decide an alert. `want_chime` is whether the caller would otherwise sound it.
    fn check(&mut self, key: &ChatKey, kind: &str, want_chime: bool, now: u64) -> Verdict {
        self.snoozed.retain(|_, until| *until > now);
        if self.snoozed.contains_key(key) {
            return self.hidden(key, kind, now);
        }
        if let Some((muted_kind, _)) = self.muted.get(key) {
            // A question and the idle "waiting for your input" nudge are one class: the chat is still waiting.
            if wait_class(muted_kind) == wait_class(kind) {
                return self.hidden(key, kind, now);
            }
            self.muted.remove(key); // the chat changed state: it speaks again
        }
        let slot = (key.clone(), kind.to_string());
        let quiet = self.chimed.get(&slot).is_some_and(|t| now.saturating_sub(*t) < CHIME_EVERY_MS);
        let chime = want_chime && !quiet;
        if chime {
            self.chimed.insert(slot, now);
        }
        if self.chimed.len() > 500 {
            self.chimed.retain(|_, t| now.saturating_sub(*t) < CHIME_EVERY_MS);
        }
        Verdict::Show { chime }
    }

    fn hidden(&mut self, key: &ChatKey, kind: &str, now: u64) -> Verdict {
        let slot = (key.clone(), kind.to_string());
        let log = self.logged.get(&slot).is_none_or(|t| now.saturating_sub(*t) >= SUPPRESS_LOG_MS);
        if log {
            self.logged.insert(slot, now);
        }
        Verdict::Hidden { log }
    }

    fn to_json(&self) -> String {
        let snoozed: Vec<_> = self.snoozed.iter().map(|((a, c), u)| serde_json::json!({"account": a, "chat": c, "until": u})).collect();
        let muted: Vec<_> = self.muted.iter().map(|((a, c), (k, at))| serde_json::json!({"account": a, "chat": c, "kind": k, "at": at})).collect();
        serde_json::to_string_pretty(&serde_json::json!({"snoozed": snoozed, "muted": muted})).unwrap_or_default()
    }

    /// Snoozes that have not ended and every mute; an unreadable file starts empty.
    fn from_json(text: &str, now: u64) -> Gate {
        let mut g = Gate::default();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return g };
        let s = |x: &serde_json::Value, f: &str| x.get(f).and_then(|y| y.as_str()).map(str::to_string);
        let n = |x: &serde_json::Value, f: &str| x.get(f).and_then(|y| y.as_u64());
        for x in v.get("snoozed").and_then(|a| a.as_array()).into_iter().flatten() {
            if let (Some(a), Some(c), Some(u)) = (s(x, "account"), s(x, "chat"), n(x, "until")) {
                if u > now {
                    // A clock set back, or a damaged file, must not make a snooze longer than an hour.
                    g.snoozed.insert((a, c), u.min(now + SNOOZE_MS));
                }
            }
        }
        for x in v.get("muted").and_then(|a| a.as_array()).into_iter().flatten() {
            if let (Some(a), Some(c), Some(k), Some(at)) = (s(x, "account"), s(x, "chat"), s(x, "kind"), n(x, "at")) {
                g.muted.insert((a, c), (k, at));
            }
        }
        g
    }
}

fn state_path() -> std::path::PathBuf {
    crate::data_file("hive-snooze.json")
}

fn save_to(g: &Gate, path: &std::path::Path) -> Result<(), String> {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, g.to_json()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

fn load_from(path: &std::path::Path, now: u64) -> Gate {
    std::fs::read_to_string(path).map(|t| Gate::from_json(&t, now)).unwrap_or_default()
}

fn with_gate<R>(f: impl FnOnce(&mut Gate) -> R) -> R {
    GATE.with(|g| {
        let mut g = g.borrow_mut();
        f(g.get_or_insert_with(|| load_from(&state_path(), crate::time::now_ms())))
    })
}

fn persist() {
    let r = GATE.with(|g| g.borrow().as_ref().map(|g| save_to(g, &state_path())));
    if let Some(Err(e)) = r {
        crate::log(&format!("could not save the snooze file: {e}"));
    }
}

/// Decide an alert for a chat (account, chat id): show it, with or without a chime, or hide it.
pub fn gate_alert(account: &str, chat: &str, kind: &str, want_chime: bool) -> Verdict {
    let Some(key) = chat_key(account, chat) else { return Verdict::Show { chime: want_chime } };
    let before = with_gate(|g| g.muted.len());
    let v = with_gate(|g| g.check(&key, kind, want_chime, crate::time::now_ms()));
    if with_gate(|g| g.muted.len()) != before {
        persist(); // a mute ended because the chat changed state
    }
    v
}

/// "Snooze 1 h": no cards or chime for this chat for an hour.
pub fn snooze_chat(account: &str, chat: &str) {
    let Some(key) = chat_key(account, chat) else { return };
    let until = crate::time::now_ms() + SNOOZE_MS;
    with_gate(|g| {
        g.muted.remove(&key);
        g.snoozed.insert(key, until);
    });
    persist();
    crate::log(&format!("{account}: snoozed a chat for 1 h"));
}

/// "Mute until I open it": silent until you open that chat or it changes state.
pub fn mute_chat(account: &str, chat: &str, kind: &str) {
    let Some(key) = chat_key(account, chat) else { return };
    let now = crate::time::now_ms();
    with_gate(|g| {
        g.snoozed.remove(&key);
        g.muted.insert(key, (kind.to_string(), now));
    });
    persist();
    crate::log(&format!("{account}: muted a chat until opened"));
}

/// This chat ran a turn (a stop): a mute set while it waited ends, since it has moved on.
pub fn chat_ran(account: &str, chat: &str) {
    let Some(key) = chat_key(account, chat) else { return };
    let ended = with_gate(|g| match g.muted.get(&key) {
        Some((kind, _)) if wait_class(kind) == "wait" => g.muted.remove(&key).is_some(),
        _ => false,
    });
    if ended {
        persist();
        crate::log(&format!("{account}: a muted chat ran again, alerts are back"));
    }
}

/// You opened this chat: it speaks again.
pub fn unmute_chat(account: &str, chat: &str) {
    let Some(key) = chat_key(account, chat) else { return };
    let had = with_gate(|g| g.muted.remove(&key).is_some() | g.snoozed.remove(&key).is_some());
    if had {
        persist();
        crate::log(&format!("{account}: opened a snoozed chat, alerts are back"));
    }
}

/// Muted chats: (account, chat id, when muted, ms). Opening one after that ends the mute.
pub fn muted_chats() -> Vec<(String, String, u64)> {
    with_gate(|g| g.muted.iter().map(|((a, c), (_, at))| (a.clone(), c.clone(), *at)).collect())
}

/// How many chats are snoozed or muted right now.
pub fn quiet_count() -> usize {
    let now = crate::time::now_ms();
    with_gate(|g| {
        g.snoozed.retain(|_, u| *u > now);
        g.snoozed.len() + g.muted.len()
    })
}

// ---- context alarm and worker watchdog ------------------------------------------------

/// A chat whose context passes this gets "hand off soon".
pub const CTX_SOON: u64 = 250_000;
/// ... and past this "hand off now" (the same line the dashboard draws).
pub const CTX_NOW: u64 = 300_000;
/// Only a chat that ran a turn this recently can have just crossed a line.
const CTX_FRESH_SECS: u64 = 10 * 60;
/// A card worker idle longer than this, with no inbox post, went quiet.
pub const QUIET_AFTER_SECS: u64 = 20 * 60;
/// Past this the chat is history, not news (a restart or a long absence raises nothing).
const QUIET_GIVE_UP_SECS: u64 = 3 * 3600;
/// When a worker's start is unknown: an inbox post this long before its last turn still counts
/// as "since it started".
const WORKER_SPAN_SECS: u64 = 2 * 3600;
/// The alarm scan runs at most this often.
pub const ALARM_EVERY_SECS: u64 = 120;

/// 0 below 250k, 1 past 250k, 2 past 300k.
pub fn ctx_level(context: u64) -> u8 {
    if context > CTX_NOW {
        2
    } else if context > CTX_SOON {
        1
    } else {
        0
    }
}

/// A chat's current context and when its last turn ran (unix seconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtxChat {
    pub key: String,
    pub context: u64,
    pub last: u64,
}

/// A chat titled "card <id>: ..." and when it last did anything (unix seconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worker {
    pub key: String,
    pub card: String,
    pub last_turn: u64,
    /// When the chat began (unix seconds), 0 when unknown.
    pub started: u64,
}

/// One inbox post: which card sent it and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostSeen {
    pub card: String,
    pub at: u64,
}

/// What was already raised: the highest context level per chat and the quiet workers.
/// Kept in `hive-alarms.json`, so a restart raises nothing twice.
#[derive(Default, Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AlarmState {
    /// Highest level raised per Claude Code session id.
    ctx: std::collections::HashMap<String, u8>,
    quiet: std::collections::BTreeSet<String>,
    /// Chats whose context alarm was held back by a snooze or mute: due once that ends,
    /// however long ago their last turn was.
    pending: std::collections::BTreeSet<String>,
}

/// An inbox post's card as a bare lowercase id: `126`, ` 126 ` and `card 126` all give "126".
pub fn norm_card(card: &str) -> String {
    let c = card.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    match c.strip_prefix("card") {
        Some(rest) if rest.is_empty() || rest.starts_with(|ch: char| !ch.is_ascii_alphanumeric()) => {
            rest.trim_matches(|ch: char| ch.is_whitespace() || ch == ':' || ch == '#').to_string()
        }
        _ => c,
    }
}

/// When a Claude Code session's transcript was created (unix seconds), found by its session id
/// under `projects`. None when there is no such file or the system keeps no creation time.
pub fn transcript_created(projects: &std::path::Path, session: &str) -> Option<u64> {
    if session.is_empty() || session.contains(['/', '\\', '.']) {
        return None;
    }
    let name = format!("{session}.jsonl");
    for e in std::fs::read_dir(projects).ok()?.flatten() {
        if let Ok(m) = std::fs::metadata(e.path().join(&name)) {
            let t = m.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
            return Some(t.as_secs());
        }
    }
    None
}

/// The id in a title like "card 126: tray alarm" (letters, digits, `.`, `_`, `-`; 16 at most).
pub fn card_id(title: &str) -> Option<String> {
    let t = title.trim_start();
    let head = t.get(..5)?;
    if !head.eq_ignore_ascii_case("card ") {
        return None;
    }
    let rest = t[5..].trim_start();
    let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).collect();
    let after = rest[id.len()..].chars().next();
    (!id.is_empty() && id.len() <= 16 && after == Some(':')).then_some(id)
}

impl AlarmState {
    /// Chats that just crossed a line not raised yet: (chat key, level 1 or 2). A chat past both
    /// lines at once gets the higher card only.
    pub fn context_due(&self, chats: &[CtxChat], now: u64) -> Vec<(String, u8)> {
        chats
            .iter()
            .filter(|c| now.saturating_sub(c.last) <= CTX_FRESH_SECS || self.pending.contains(&c.key))
            .filter_map(|c| {
                let level = ctx_level(c.context);
                (level > 0 && level > self.ctx.get(&c.key).copied().unwrap_or(0)).then(|| (c.key.clone(), level))
            })
            .collect()
    }

    /// Call once the card was shown: this level (and the one below) is done for the chat.
    pub fn mark_ctx(&mut self, key: &str, level: u8) {
        let e = self.ctx.entry(key.to_string()).or_insert(0);
        *e = (*e).max(level);
        self.pending.remove(key);
    }

    /// A snooze or mute held the alarm back: keep the chat due after the freshness window too.
    pub fn defer_ctx(&mut self, key: &str) {
        self.pending.insert(key.to_string());
    }

    /// Workers that went quiet and have not been raised: idle over 20 minutes (and under 3 hours)
    /// after their last turn, with no inbox post from their card since they started (the chat's
    /// creation time; a couple of hours before its last turn when that is unknown).
    pub fn quiet_due(&self, workers: &[Worker], posts: &[PostSeen], now: u64) -> Vec<String> {
        workers
            .iter()
            .filter(|w| w.last_turn > 0)
            .filter(|w| {
                let idle = now.saturating_sub(w.last_turn);
                idle > QUIET_AFTER_SECS && idle <= QUIET_GIVE_UP_SECS
            })
            .filter(|w| !self.quiet.contains(&w.key))
            .filter(|w| {
                let since = if w.started > 0 { w.started } else { w.last_turn.saturating_sub(WORKER_SPAN_SECS) };
                let id = norm_card(&w.card);
                !posts.iter().any(|p| norm_card(&p.card) == id && p.at >= since)
            })
            .map(|w| w.key.clone())
            .collect()
    }

    pub fn mark_quiet(&mut self, key: &str) {
        self.quiet.insert(key.to_string());
    }

    /// Keeps the file small.
    fn trim(&mut self) {
        const MAX: usize = 4000;
        if self.ctx.len() > MAX {
            self.ctx.clear();
        }
        if self.pending.len() > MAX {
            self.pending.clear();
        }
        while self.quiet.len() > MAX {
            let first = self.quiet.iter().next().cloned();
            match first {
                Some(k) => self.quiet.remove(&k),
                None => break,
            };
        }
    }

    pub fn load_from(path: &std::path::Path) -> AlarmState {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save_to(&mut self, path: &std::path::Path) -> Result<(), String> {
        self.trim();
        let text = serde_json::to_string(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }

    pub fn load() -> AlarmState {
        Self::load_from(&crate::data_file("hive-alarms.json"))
    }

    pub fn save(&mut self) {
        if let Err(e) = self.save_to(&crate::data_file("hive-alarms.json")) {
            crate::log(&format!("could not save the alarm file: {e}"));
        }
    }
}

/// Every inbox post in `dir` (the tail of each recipient file), read-only. Runs off the UI thread.
pub fn read_posts(dir: &std::path::Path) -> Vec<PostSeen> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 512 * 1024;
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        if e.path().extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Ok(mut f) = std::fs::File::open(e.path()) else { continue };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(TAIL);
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(start)).is_err() || f.take(len - start).read_to_end(&mut buf).is_err() {
            continue;
        }
        for (i, line) in buf.split(|b| *b == b'\n').enumerate() {
            if i == 0 && start > 0 {
                continue; // cut mid-line
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
            let card = v.get("card").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if !card.is_empty() {
                out.push(PostSeen { card, at: v.get("at").and_then(|x| x.as_u64()).unwrap_or(0) });
            }
        }
    }
    out
}

impl Gate {
    /// Snoozed, or muted for any reason: the alarms stay quiet for this chat.
    fn silenced(&mut self, key: &ChatKey, now: u64) -> bool {
        self.snoozed.retain(|_, until| *until > now);
        self.snoozed.contains_key(key) || self.muted.contains_key(key)
    }
}

/// May an alarm card (context or quiet worker) show for this chat? False while the chat is
/// snoozed or muted; the alarm is then not marked raised, so it comes after the snooze.
pub fn gate_alarm(account: &str, chat: &str) -> bool {
    let Some(key) = chat_key(account, chat) else { return true };
    !with_gate(|g| g.silenced(&key, crate::time::now_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles() -> Vec<String> {
        CARDS.with(|c| c.borrow().iter().map(|k| k.title.clone()).collect())
    }

    #[test]
    fn a_chat_keeps_one_card_with_its_newest_words() {
        assert_eq!(chat_key("Team", "local_1"), Some(("Team".into(), "local_1".into())));
        assert_eq!(chat_key("Team", ""), None, "no chat id: never merged");
        let chat = |title: &str, text: &str, kind: &str| {
            let mut q = Queued::new(title, text, None, Action::Chat("Team".into(), "local_1".into(), "T".into()), 30);
            q.kind = kind.into();
            q
        };
        let mut w = chat("Team finished", "T\nDone.", "stop");
        w.left = 1_000;
        w.update(&chat("Team has a question", "T\nShip it?", "ask"));
        assert_eq!((w.title.as_str(), w.text.as_str(), w.kind.as_str(), w.count, w.left), ("Team has a question", "T\nShip it?", "ask", 2, 30_000));
        w.update(&chat("Team has a question", "T\nShip it?", "ask"));
        assert_eq!(w.count, 2, "the same words again are not another update");
    }

    /// Probe: six alerts at once. Four slots show, the oldest wait under a count
    /// card, and closing a card brings the newest waiting one back. Needs a desktop.
    #[test]
    #[ignore]
    fn overflow_waits_instead_of_vanishing() {
        for i in 1..=6 {
            show(&format!("Alert {i}"), "Short text.", None, Action::Open(format!("Probe{i}")), 0);
        }
        let t = titles();
        println!("after 6: {t:?}");
        assert_eq!(t.len(), MAX_CARDS);
        assert_eq!(t[0], "3 more waiting");
        assert_eq!(&t[1..], ["Alert 4", "Alert 5", "Alert 6"]);
        let newest = CARDS.with(|c| c.borrow().last().map(|k| k.hwnd)).unwrap();
        close(newest);
        let t = titles();
        println!("after closing 6: {t:?}");
        assert_eq!(t, ["2 more waiting", "Alert 3", "Alert 4", "Alert 5"]);
        dismiss_account("Probe1");
        dismiss_account("Probe2");
        let t = titles();
        println!("after dismissing 1 and 2: {t:?}");
        assert_eq!(t, ["Alert 3", "Alert 4", "Alert 5"]);
        let top = CARDS.with(|c| c.borrow()[0].hwnd);
        close(top);
        assert_eq!(titles(), ["Alert 4", "Alert 5"]);
        // The summary's X lets the waiting ones go.
        for i in 7..=10 {
            show(&format!("Alert {i}"), "Short text.", None, Action::Open(format!("Probe{i}")), 0);
        }
        let summary = CARDS.with(|c| c.borrow().iter().find(|k| is_summary(k)).map(|k| k.hwnd)).unwrap();
        close(summary);
        let t = titles();
        println!("after summary X: {t:?}");
        assert!(t.iter().all(|s| !s.contains("waiting")) && WAITING.with(|w| w.borrow().is_empty()));
        close_all();
    }

    /// For a screenshot: six alerts, kept on screen for 6 s.
    #[test]
    #[ignore]
    fn overflow_screenshot() {
        for i in 1..=6 {
            let text = format!("\"Probe chat {i}\": Should I go ahead with the next step?");
            show(&format!("Probe{i} has a question"), &text, Some(("P".into(), 0x4C8BF5)), Action::Chat(format!("Probe{i}"), String::new(), String::new()), 0);
        }
        let until = std::time::Instant::now() + std::time::Duration::from_secs(6);
        while std::time::Instant::now() < until {
            unsafe {
                let mut m: MSG = std::mem::zeroed();
                while PeekMessageW(&mut m, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&m);
                    DispatchMessageW(&m);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        close_all();
    }

    fn k(chat: &str) -> ChatKey {
        ("Team".into(), chat.into())
    }
    const MIN: u64 = 60_000;
    const T0: u64 = 1_000_000_000_000;

    #[test]
    fn five_asks_in_ten_minutes_chime_once_and_other_chats_and_kinds_still_chime() {
        let mut g = Gate::default();
        let mut chimes = 0;
        for i in 0..5 {
            // The caller's own rule (card already up, same kind) is not in play here: every ask wants a chime.
            if g.check(&k("a"), "ask", true, T0 + i * 2 * MIN) == (Verdict::Show { chime: true }) {
                chimes += 1;
            }
        }
        assert_eq!(chimes, 1, "five asks from one chat: one chime");
        assert_eq!(g.check(&k("b"), "ask", true, T0 + MIN), Verdict::Show { chime: true }, "a second chat still chimes");
        assert_eq!(g.check(&k("a"), "stop", true, T0 + MIN), Verdict::Show { chime: true }, "a done event still chimes");
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 10 * MIN), Verdict::Show { chime: true }, "chimes again after 10 minutes");
        // The card is never hidden by the chime limit: every one of those was Show.
        assert_eq!(g.check(&k("a"), "ask", false, T0 + 11 * MIN), Verdict::Show { chime: false });
    }

    #[test]
    fn a_mute_on_a_question_outlasts_the_idle_nudge_and_ends_when_the_chat_runs() {
        let mut g = Gate::default();
        g.muted.insert(k("a"), ("ask".into(), T0));
        assert_eq!(g.check(&k("a"), "notify", true, T0 + 6 * MIN), Verdict::Hidden { log: true }, "the idle nudge is the same waiting class");
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 7 * MIN), Verdict::Hidden { log: true });
        assert!(g.muted.contains_key(&k("a")));
        assert_eq!(g.check(&k("a"), "stop", true, T0 + 8 * MIN), Verdict::Show { chime: true }, "a stop after the mute clears it");
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 9 * MIN), Verdict::Show { chime: true }, "and the next ask shows");
    }

    #[test]
    fn a_snooze_in_a_damaged_file_is_clamped_to_an_hour() {
        let text = format!(r#"{{"snoozed":[{{"account":"Team","chat":"a","until":{}}}],"muted":[]}}"#, T0 + 99_999 * MIN);
        let g = Gate::from_json(&text, T0);
        assert_eq!(g.snoozed.get(&k("a")), Some(&(T0 + SNOOZE_MS)));
    }

    #[test]
    fn a_suppressed_repeat_ask_restarts_its_cards_stay_timer() {
        close_all();
        let action = || Action::Chat("Team".into(), "local_t".into(), "T".into());
        show_chat("local_t", "ask", "Team has a question", "T
Ship it?", None, action(), 30);
        CARDS.with(|c| c.borrow_mut()[0].left = 500);
        show_chat("local_t", "ask", "Team has a question", "T
Ship it?", None, action(), 30);
        assert_eq!(CARDS.with(|c| c.borrow().iter().find(|k| k.chat.is_some()).map(|k| k.left)), Some(30_000));
        close_all();
    }

    #[test]
    fn snooze_hides_a_chat_for_an_hour_then_expires() {
        let mut g = Gate::default();
        g.snoozed.insert(k("a"), T0 + 60 * MIN);
        assert_eq!(g.check(&k("a"), "ask", true, T0 + MIN), Verdict::Hidden { log: true });
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 2 * MIN), Verdict::Hidden { log: false }, "one log line per kind per 10 minutes");
        assert_eq!(g.check(&k("a"), "stop", true, T0 + 3 * MIN), Verdict::Hidden { log: true }, "a snooze hides every kind");
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 12 * MIN), Verdict::Hidden { log: true }, "logged again after 10 minutes");
        assert_eq!(g.check(&k("b"), "ask", true, T0 + MIN), Verdict::Show { chime: true }, "other chats are not snoozed");
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 61 * MIN), Verdict::Show { chime: true }, "it expires on its own");
        assert!(g.snoozed.is_empty());
    }

    #[test]
    fn mute_clears_when_the_chat_changes_state() {
        let mut g = Gate::default();
        g.muted.insert(k("a"), ("ask".into(), T0));
        assert_eq!(g.check(&k("a"), "ask", true, T0 + MIN), Verdict::Hidden { log: true });
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 500 * MIN), Verdict::Hidden { log: true }, "a mute does not expire");
        assert_eq!(g.check(&k("a"), "stop", true, T0 + 501 * MIN), Verdict::Show { chime: true }, "a new state speaks");
        assert!(g.muted.is_empty());
        assert_eq!(g.check(&k("a"), "ask", true, T0 + 502 * MIN), Verdict::Show { chime: true }, "and the mute is gone");
    }

    #[test]
    fn snooze_and_mute_survive_a_reload_of_the_state_file() {
        let dir = std::env::temp_dir().join(format!("hive-snooze-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hive-snooze.json");
        let mut g = Gate::default();
        g.snoozed.insert(k("live"), T0 + 30 * MIN);
        g.snoozed.insert(k("old"), T0 - MIN);
        g.muted.insert(k("m"), ("ask".into(), T0 - 5 * MIN));
        save_to(&g, &path).unwrap();
        assert!(!dir.read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp")), "atomic write leaves no temp file");
        let mut back = load_from(&path, T0);
        assert_eq!(back.snoozed.len(), 1, "a snooze that already ended is dropped on load");
        assert_eq!(back.check(&k("live"), "ask", true, T0 + MIN), Verdict::Hidden { log: true });
        assert_eq!(back.check(&k("m"), "ask", true, T0 + MIN), Verdict::Hidden { log: true });
        assert_eq!(back.check(&k("live"), "ask", true, T0 + 31 * MIN), Verdict::Show { chime: true }, "still expires after the reload");
        // A missing or damaged file starts empty.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_from(&path, T0).snoozed.is_empty());
        assert!(load_from(&dir.join("none.json"), T0).muted.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- inbox cards and the away summary (card 120) ----

    const NOW: u64 = 1_800_000_000;

    fn inbox_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hive-inbox-card-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Tell the watcher this recipient was already looked at (a tray that ran before).
    fn seed(dir: &std::path::Path, to: &str) {
        std::fs::write(dir.join(format!("{to}.tray")), "0").unwrap();
    }

    fn post_line(dir: &std::path::Path, to: &str, card: &str, status: &str, text: &str, at: u64) {
        use std::io::Write;
        let line = serde_json::json!({"at": at, "to": to, "card": card, "status": status, "text": text});
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{to}.jsonl"))).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    fn a_blocked_post_raises_one_card_and_a_done_post_none() {
        let d = inbox_dir("raise");
        let mut w = InboxWatch::default();
        seed(&d, "coordinator");
        post_line(&d, "coordinator", "120", "blocked", "need the lock", NOW);
        let f = w.poll(&d, NOW);
        assert_eq!(f.raise.len(), 1);
        assert_eq!(f.raise[0].title(), "Card 120 worker is blocked");
        assert_eq!(f.raise[0].text, "need the lock");
        post_line(&d, "coordinator", "120", "done", "all good", NOW);
        post_line(&d, "coordinator", "120", "info", "fyi", NOW);
        let f = w.poll(&d, NOW);
        assert!(f.raise.is_empty(), "done and info raise nothing");
        assert_eq!(f.fresh, 2, "but they count for the away summary");
        post_line(&d, "coordinator", "121", "question", "which file?", NOW);
        let f = w.poll(&d, NOW);
        assert_eq!(f.raise.len(), 1);
        assert_eq!(f.raise[0].title(), "Card 121 worker has a question");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_post_already_raised_or_read_is_not_raised_again_after_a_restart() {
        let d = inbox_dir("restart");
        seed(&d, "coordinator");
        seed(&d, "other");
        seed(&d, "late");
        post_line(&d, "coordinator", "120", "blocked", "one", NOW);
        assert_eq!(InboxWatch::default().poll(&d, NOW).raise.len(), 1);
        // A new tray process: same folder, fresh watcher.
        let mut w = InboxWatch::default();
        assert!(w.poll(&d, NOW + 5).raise.is_empty(), "raised before the restart");
        post_line(&d, "coordinator", "120", "blocked", "two", NOW + 6);
        let f = w.poll(&d, NOW + 7);
        assert_eq!(f.raise.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["two"]);
        // The coordinator's own hook read it: no card for what it already saw.
        let len = std::fs::metadata(d.join("coordinator.jsonl")).unwrap().len();
        post_line(&d, "other", "9", "blocked", "x", NOW);
        std::fs::write(d.join("other.cursor"), std::fs::metadata(d.join("other.jsonl")).unwrap().len().to_string()).unwrap();
        assert!(InboxWatch::default().poll(&d, NOW).raise.iter().all(|p| p.to != "other"));
        assert!(len > 0);
        // A stale post (older than 30 minutes) is skipped.
        post_line(&d, "late", "5", "blocked", "old", NOW - 3600);
        assert!(InboxWatch::default().poll(&d, NOW).raise.iter().all(|p| p.to != "late"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn first_sight_of_an_inbox_file_raises_nothing_and_a_replaced_longer_file_is_read_again() {
        let d = inbox_dir("first");
        post_line(&d, "coordinator", "1", "blocked", "old news", NOW);
        let mut w = InboxWatch::default();
        let f = w.poll(&d, NOW);
        assert!(f.raise.is_empty() && f.fresh == 0, "no .tray file: history is not raised or counted");
        assert!(d.join("coordinator.tray").exists());
        // The file is replaced by a longer one whose bytes at the old offset are mid-line.
        let long = format!("{}\n", serde_json::json!({"at": NOW, "to": "coordinator", "card": "7", "status": "blocked", "text": "x".repeat(300)}));
        std::fs::write(d.join("coordinator.jsonl"), long.repeat(2)).unwrap();
        let f = InboxWatch::default().poll(&d, NOW);
        assert_eq!(f.raise.len(), 2, "a saved offset that is not at a line start restarts from 0");
        // No leftover temp files from the .tray write.
        assert!(std::fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_card_field_in_a_title_is_filtered_and_capped() {
        let p = |card: &str| InboxPost { to: "c".into(), card: card.into(), status: "blocked".into(), text: String::new() };
        assert_eq!(p("12 <b>\n--x").title(), "Card 12b--x worker is blocked");
        assert_eq!(p(&"a".repeat(40)).title(), format!("Card {} worker is blocked", "a".repeat(16)));
        assert_eq!(p("!!!").title(), "A worker is blocked");
    }

    #[test]
    fn five_inbox_posts_in_ten_minutes_chime_once_on_one_card_key() {
        let mut g = Gate::default();
        let key = ("Team".to_string(), "inbox:coordinator".to_string());
        let chimes = (0..5).filter(|i| g.check(&key, "inbox", true, T0 + i * 2 * MIN) == (Verdict::Show { chime: true })).count();
        assert_eq!(chimes, 1);
        // Snoozing that card's key silences it.
        g.snoozed.insert(key.clone(), T0 + 60 * MIN);
        assert_eq!(g.check(&key, "inbox", true, T0 + 11 * MIN), Verdict::Hidden { log: true });
    }

    #[test]
    fn an_unchanged_inbox_costs_almost_nothing_per_tick() {
        let d = inbox_dir("cost");
        for to in ["coordinator", "a", "b"] {
            post_line(&d, to, "1", "info", "x", NOW);
        }
        let mut w = InboxWatch::default();
        w.poll(&d, NOW);
        // Best of 5 rounds, so a busy PC (other builds) cannot fail it; card 129.
        let per_tick = (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                for _ in 0..40 {
                    assert!(w.poll(&d, NOW).raise.is_empty());
                }
                t.elapsed() / 40
            })
            .min()
            .unwrap();
        assert!(per_tick < std::time::Duration::from_millis(5), "{per_tick:?} per tick");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_away_summary_needs_twenty_minutes_idle_and_something_that_happened() {
        let min = 60;
        // 25 minutes away, events: one summary with the right counts, then nothing more.
        let mut a = Away::default();
        assert_eq!(a.tick(0), None);
        a.chat("c1", false, 5 * min);
        a.chat("c2", false, 8 * min);
        a.chat("c2", false, 9 * min); // the same chat again counts once
        a.chat("c3", true, 12 * min);
        a.chat("c3", true, 13 * min);
        a.posts(2, 15 * min);
        for idle in [10 * min, 20 * min, 25 * min] {
            assert_eq!(a.tick(idle), None, "still away");
        }
        assert_eq!(a.tick(3), Some((2, 1, 2)));
        assert_eq!(a.tick(4), None, "said once");
        // 25 minutes away and nothing happened.
        let mut a = Away::default();
        a.tick(25 * min);
        assert_eq!(a.tick(2), None);
        // Only 10 minutes away, with events: no summary, and the counts start over.
        let mut a = Away::default();
        a.chat("c1", false, 3 * min);
        a.tick(10 * min);
        assert_eq!(a.tick(2), None);
        assert_eq!((a.finished.len(), a.waiting.len(), a.inbox), (0, 0, 0));
        // Events while you are at the PC are not counted.
        let mut a = Away::default();
        a.chat("c1", false, 5);
        a.posts(3, 5);
        a.tick(30 * min);
        assert_eq!(a.tick(1), None);
        assert_eq!(away_text(3, 1, 2), "3 chats finished, 1 waiting, 2 inbox posts");
        assert_eq!(away_text(1, 0, 1), "1 chat finished, 0 waiting, 1 inbox post");
    }

    #[test]
    fn only_the_button_of_a_continue_in_card_continues_the_chat() {
        let a = || Action::ContinueIn("Max".into(), "local_1".into(), "T".into(), "Pro".into(), "cli".into());
        assert_eq!(a().label().as_deref(), Some("Continue in Pro"));
        assert!(matches!(click_action(a(), Some(CardHit::Primary)), Action::ContinueIn(..)));
        assert!(matches!(click_action(a(), Some(CardHit::Body)), Action::Chat(ref acc, ref id, _) if acc == "Max" && id == "local_1"));
    }
}

#[cfg(test)]
mod alarm_tests {
    use super::*;

    fn chat(key: &str, context: u64, last: u64) -> CtxChat {
        CtxChat { key: key.into(), context, last }
    }

    fn worker(key: &str, id: &str, last_turn: u64) -> Worker {
        Worker { key: key.into(), card: id.into(), last_turn, started: 0 }
    }

    #[test]
    fn context_thresholds_are_strictly_above() {
        assert_eq!(ctx_level(0), 0);
        assert_eq!(ctx_level(250_000), 0);
        assert_eq!(ctx_level(250_001), 1);
        assert_eq!(ctx_level(300_000), 1);
        assert_eq!(ctx_level(300_001), 2);
        let s = AlarmState::default();
        let now = 10_000;
        let due = s.context_due(&[chat("a", 249_000, now), chat("b", 260_000, now), chat("c", 310_000, now)], now);
        assert_eq!(due, vec![("b".to_string(), 1), ("c".to_string(), 2)]);
    }

    #[test]
    fn context_raises_once_per_threshold() {
        let mut s = AlarmState::default();
        let now = 10_000;
        let mut c = chat("a", 260_000, now);
        assert_eq!(s.context_due(&[c.clone()], now), vec![("a".to_string(), 1)]);
        s.mark_ctx("a", 1);
        assert!(s.context_due(&[c.clone()], now + 60).is_empty(), "250k does not repeat");
        c.context = 305_000;
        assert_eq!(s.context_due(&[c.clone()], now + 120), vec![("a".to_string(), 2)], "300k is its own card");
        s.mark_ctx("a", 2);
        assert!(s.context_due(&[c.clone()], now + 180).is_empty());
        c.context = 255_000; // compacted, then grew again: still done
        assert!(s.context_due(&[c], now + 240).is_empty());
        // Both lines passed at once: the 300k card only, and the 250k one is then done too.
        let mut s = AlarmState::default();
        assert_eq!(s.context_due(&[chat("z", 400_000, now)], now), vec![("z".to_string(), 2)]);
        s.mark_ctx("z", 2);
        assert!(s.context_due(&[chat("z", 400_000, now)], now).is_empty());
    }

    #[test]
    fn context_ignores_chats_not_running_now() {
        let s = AlarmState::default();
        let now = 100_000;
        assert!(s.context_due(&[chat("old", 320_000, now - CTX_FRESH_SECS - 1)], now).is_empty());
        assert_eq!(s.context_due(&[chat("new", 320_000, now - CTX_FRESH_SECS)], now).len(), 1);
    }

    #[test]
    fn raised_state_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("hive-alarm-state-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hive-alarms.json");
        let mut s = AlarmState::default();
        s.mark_ctx("a", 2);
        s.mark_quiet("local_7");
        s.save_to(&path).unwrap();
        let back = AlarmState::load_from(&path);
        assert_eq!(back, s);
        assert!(back.context_due(&[chat("a", 400_000, 5)], 5).is_empty());
        assert!(back.quiet_due(&[worker("local_7", "7", 1)], &[], 1 + QUIET_AFTER_SECS + 5).is_empty());
        assert_eq!(AlarmState::load_from(&dir.join("missing.json")), AlarmState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alarms_respect_snooze_and_mute() {
        let key: ChatKey = ("Max".into(), "local_1".into());
        let mut g = Gate::default();
        let now = 1_000_000;
        assert!(!g.silenced(&key, now));
        g.snoozed.insert(key.clone(), now + 5);
        assert!(g.silenced(&key, now));
        assert!(!g.silenced(&key, now + 5), "the snooze ended");
        g.muted.insert(key.clone(), ("stop".into(), now));
        assert!(g.silenced(&key, now + 10), "a mute of any kind holds the alarm back");
        assert!(g.muted.contains_key(&key), "and is left in place");
    }

    #[test]
    fn card_ids_come_from_titles() {
        assert_eq!(card_id("card 126: tray alarm").as_deref(), Some("126"));
        assert_eq!(card_id("  Card 7.1: x").as_deref(), Some("7.1"));
        assert_eq!(card_id("card 126 tray alarm"), None);
        assert_eq!(card_id("Hive coordinator"), None);
        assert_eq!(card_id("card : x"), None);
        assert_eq!(card_id("cards 1: x"), None);
        assert_eq!(card_id("card \u{e9}: x"), None);
        assert_eq!(card_id("ca\u{e9}d"), None);
    }

    #[test]
    fn quiet_worker_after_twenty_idle_minutes() {
        let s = AlarmState::default();
        let now = 1_000_000;
        let w = |ago: u64| vec![worker("local_1", "126", now - ago)];
        assert!(s.quiet_due(&w(QUIET_AFTER_SECS), &[], now).is_empty(), "exactly 20 minutes is not over");
        assert_eq!(s.quiet_due(&w(QUIET_AFTER_SECS + 1), &[], now), vec!["local_1".to_string()]);
        assert_eq!(s.quiet_due(&w(QUIET_GIVE_UP_SECS), &[], now).len(), 1);
        assert!(s.quiet_due(&w(QUIET_GIVE_UP_SECS + 1), &[], now).is_empty(), "history, not news");
        assert!(s.quiet_due(&[worker("local_1", "126", 0)], &[], now).is_empty(), "no known last turn");
        assert!(s.quiet_due(&w(60), &[], now).is_empty(), "still running");
    }

    #[test]
    fn quiet_worker_raises_once() {
        let mut s = AlarmState::default();
        let now = 1_000_000;
        let ws = [worker("local_1", "126", now - 30 * 60)];
        assert_eq!(s.quiet_due(&ws, &[], now).len(), 1);
        s.mark_quiet("local_1");
        assert!(s.quiet_due(&ws, &[], now + 600).is_empty());
    }

    #[test]
    fn an_inbox_post_from_the_card_suppresses_the_watchdog() {
        let s = AlarmState::default();
        let now = 1_000_000;
        let ws = [worker("local_1", "126", now - 30 * 60), worker("local_2", "127", now - 30 * 60)];
        let posts = [PostSeen { card: "126".into(), at: now - 40 * 60 }];
        assert_eq!(s.quiet_due(&ws, &posts, now), vec!["local_2".to_string()], "only card 127 is quiet");
        let other = [PostSeen { card: "125".into(), at: now - 10 }];
        assert_eq!(s.quiet_due(&ws, &other, now).len(), 2, "another card's post does not count");
        // A post from long before this worker's turns belongs to an earlier worker of that id.
        let old = [PostSeen { card: "126".into(), at: now - 30 * 60 - WORKER_SPAN_SECS - 1 }];
        assert_eq!(s.quiet_due(&ws[..1], &old, now).len(), 1);
    }

    #[test]
    fn post_cards_are_compared_as_bare_ids() {
        for raw in ["126", " 126 ", "card 126", "Card  126", "card:126", "CARD #126", "card	126"] {
            assert_eq!(norm_card(raw), "126", "{raw:?}");
        }
        assert_eq!(norm_card("cardinal"), "cardinal", "not a card prefix");
        assert_eq!(norm_card("card"), "");
        let s = AlarmState::default();
        let now = 1_000_000;
        let ws = [worker("local_1", "126", now - 30 * 60)];
        for raw in ["card 126", " 126 ", "Card 126"] {
            let posts = [PostSeen { card: raw.into(), at: now - 40 * 60 }];
            assert!(s.quiet_due(&ws, &posts, now).is_empty(), "a done post as {raw:?} suppresses");
        }
        let posts = [PostSeen { card: "card 127".into(), at: now - 40 * 60 }];
        assert_eq!(s.quiet_due(&ws, &posts, now).len(), 1);
    }

    #[test]
    fn a_post_from_before_the_chat_existed_does_not_count() {
        let s = AlarmState::default();
        let now = 1_000_000;
        let started = now - 60 * 60;
        let w = [Worker { key: "local_9".into(), card: "126".into(), last_turn: now - 30 * 60, started }];
        let old = [PostSeen { card: "126".into(), at: started - 30 * 60 }];
        assert_eq!(s.quiet_due(&w, &old, now), vec!["local_9".to_string()], "an earlier worker's done post");
        let own = [PostSeen { card: "card 126".into(), at: started + 10 * 60 }];
        assert!(s.quiet_due(&w, &own, now).is_empty());
        let at_start = [PostSeen { card: "126".into(), at: started }];
        assert!(s.quiet_due(&w, &at_start, now).is_empty());
    }

    #[test]
    fn a_held_back_context_alarm_comes_after_the_snooze() {
        let mut s = AlarmState::default();
        let now = 1_000_000;
        let stale = chat("sess", 310_000, now - 3 * 3600);
        assert!(s.context_due(&[stale.clone()], now).is_empty(), "an old crossing is history");
        // It crossed while snoozed (fresh then): the gate held it back.
        assert_eq!(s.context_due(&[chat("sess", 310_000, now - 60)], now - 30).len(), 1);
        s.defer_ctx("sess");
        assert_eq!(s.context_due(&[stale.clone()], now), vec![("sess".to_string(), 2)], "still due when the snooze ends");
        s.mark_ctx("sess", 2);
        assert!(s.context_due(&[stale], now).is_empty(), "raised once, no longer pending");
        let mut s = AlarmState::default();
        s.defer_ctx("x");
        let dir = std::env::temp_dir().join(format!("hive-alarm-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hive-alarms.json");
        s.save_to(&path).unwrap();
        assert_eq!(AlarmState::load_from(&path).context_due(&[chat("x", 260_000, 0)], now).len(), 1, "pending survives a restart");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_creation_time_is_found_by_session_id() {
        let dir = std::env::temp_dir().join(format!("hive-alarm-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj-a")).unwrap();
        std::fs::create_dir_all(dir.join("proj-b")).unwrap();
        std::fs::write(dir.join("proj-b").join("abc-123.jsonl"), "{}
").unwrap();
        let t = transcript_created(&dir, "abc-123").expect("created time");
        assert!(t > 1_600_000_000);
        assert_eq!(transcript_created(&dir, "nope"), None);
        assert_eq!(transcript_created(&dir, "../abc-123"), None, "no path tricks");
        assert_eq!(transcript_created(&dir, ""), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alarm_cards_keep_their_own_slot() {
        assert!(is_alarm_kind("context") && is_alarm_kind("quiet"));
        for k in ["stop", "notify", "ask", "inbox", ""] {
            assert!(!is_alarm_kind(k), "{k}");
        }
    }

    #[test]
    fn done_post_read_from_the_inbox_folder_suppresses() {
        let dir = std::env::temp_dir().join(format!("hive-alarm-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = 2_000_000u64;
        let line = |card: &str, status: &str, at: u64| format!("{{\"at\":{at},\"card\":\"{card}\",\"status\":\"{status}\",\"text\":\"x\"}}\n");
        let body = format!("not json\n{}{}", line("126", "done", now - 120), line("130", "info", now - 90));
        std::fs::write(dir.join("coordinator.jsonl"), body).unwrap();
        std::fs::write(dir.join("coordinator.cursor"), "0").unwrap();
        let posts = read_posts(&dir);
        assert_eq!(posts.len(), 2);
        let s = AlarmState::default();
        let ws = [worker("local_1", "126", now - 25 * 60), worker("local_3", "128", now - 25 * 60)];
        assert_eq!(s.quiet_due(&ws, &posts, now), vec!["local_3".to_string()]);
        assert!(read_posts(&dir.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
