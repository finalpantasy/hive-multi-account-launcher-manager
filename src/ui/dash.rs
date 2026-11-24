//! The usage dashboard: a full, resizable window next to the tray flyout. Tokens and
//! API-price cost by day, account, model and project (Claude Code transcripts and Codex
//! rollouts, see stats.rs), when each account's limits run out at the current pace
//! (Claude's own usage samples), and what each account is doing right now.
//!
//! The look follows the logo: faceted, amber, glowing. It draws on the GPU (a window
//! render target) and animates while it's on screen: bars grow in, numbers count up,
//! each account's light pulses while it works, the background facets shimmer.

use super::*;
use crate::stats::{self, Agg, ChatIndex, Forecast, Index, Summary};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP, D2D_SIZE_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, ID2D1LinearGradientBrush, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, ID2D1PathGeometry, ID2D1RadialGradientBrush, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
    D2D1_EXTEND_MODE_CLAMP, D2D1_GAMMA_2_2, D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_WINDOW_STATE_OCCLUDED,
};
use windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_ALIGNMENT_TRAILING;

const DCLASS: &str = "HiveDashboard";
const WM_DASH_PROGRESS: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 20;
const WM_DASH_READY: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 21;
/// A live reading from the worker thread is ready (`LIVE`).
const WM_DASH_LIVE: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 22;
/// The memory health read finished (worker thread).
const WM_DASH_TOP: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 24;
const WM_DASH_MEM: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 23;
const TIMER_FRAME: usize = 1;
const TIMER_LIVE: usize = 2;
/// Smallest size the layout fits in, in DIPs.
const MIN_W: f32 = 1100.0;
const MIN_H: f32 = 860.0;
const GAP: f32 = 14.0;

/// The read transcripts and which account each chat belongs to (kept between openings).
static DATA: Mutex<Option<(Arc<Index>, Arc<HashMap<String, String>>)>> = Mutex::new(None);
static LOADING: AtomicBool = AtomicBool::new(false);
/// The top-chats panel's latest numbers (made on a worker thread), the chat index kept in
/// memory between refreshes, and whether a pass is running.
static TOP: Mutex<Option<TopPanel>> = Mutex::new(None);
static TOP_INDEX: Mutex<Option<ChatIndex>> = Mutex::new(None);
static TOP_BUSY: AtomicBool = AtomicBool::new(false);
/// The live reading (processes, and now and then each account's limits), made off the
/// window's thread so the animation never waits on a scan of every process.
static LIVE: Mutex<Option<Live>> = Mutex::new(None);
static LIVE_BUSY: AtomicBool = AtomicBool::new(false);
/// Frame timer ticks, for the slower pace while another window is in front.
static BACK_TICK: AtomicU64 = AtomicU64::new(0);
static LIVE_TICKS: AtomicU64 = AtomicU64::new(0);

type Live = (HashMap<String, (usize, u64)>, Option<(Vec<String>, Vec<Pod>)>);

/// The dashboard window, if it is open (results from worker threads go to whichever
/// window is open when they finish, not the one that asked).
fn dash_hwnd() -> HWND {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(crate::win::wide(DCLASS).as_ptr(), std::ptr::null()) }
}
/// done << 32 | of, while reading.
static PROGRESS: AtomicU64 = AtomicU64::new(0);
/// The Memory view's numbers, read on a worker thread.
static MEM: Mutex<Option<crate::memory::Health>> = Mutex::new(None);
static MEM_BUSY: AtomicBool = AtomicBool::new(false);
/// What a Keep or Delete did (worker thread): (message, worked).
static TOAST: Mutex<Option<(String, bool)>> = Mutex::new(None);

/// Which notes the note browser lists.
#[derive(Clone, PartialEq, Debug)]
enum Pick {
    All,
    /// Over 90 days old, oldest first: the review list.
    Old,
    Folder(String),
}

/// The note browser's buttons.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    Open,
    Reveal,
    Keep,
    Delete,
    Skip,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum DHit {
    Range(usize),
    Metric(usize),
    /// 0: the ranked lists, 1: top chats this week.
    Bottom(usize),
    /// None: every account.
    Account(Option<usize>),
    Bar(usize),
    Refresh,
    /// 0: usage, 1: memory.
    View(usize),
    /// A project in the memory graph.
    Node(usize),
    /// The Memory view's page: 0 notes, 1 stats.
    MemPage(usize),
    /// A row of the note browser's folder list.
    Folder(usize),
    /// A row of the note list (index into `found`).
    Note(usize),
    Act(Act),
    ClearSearch,
    Review,
    /// A scrolling pane under the mouse: 0 folders, 1 notes, 2 the note.
    Pane(u8),
}

/// Model family colors (stats::FAMILIES): Opus, Sonnet, Haiku, Fable, other.
const FAM: [u32; 5] = [0xFF8A3D, 0x4FB3FF, 0x5BE38A, 0xB58CFF, 0x9A938C];

const RANGES: [(&str, u64); 4] = [("7 days", 7), ("30 days", 30), ("90 days", 90), ("All time", 0)];
const METRICS: [&str; 2] = ["Cost", "Tokens"];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// One account's card in the live row.
struct Pod {
    name: String,
    color: C,
    codex: bool,
    open: bool,
    /// Code chats loaded right now.
    chats: usize,
    /// 0..1, how busy its processes are (smoothed).
    busy: f32,
    five: Option<u8>,
    five_fc: Forecast,
    week: Option<u8>,
    week_fc: Forecast,
    as_of: u64,
    /// Times it passed 95% of a window in the last 7 days, and the last time it did (limits.jsonl).
    past95: u32,
    last95: Option<u64>,
    /// Tokens in each of the last 24 hours, oldest first.
    hours: [f64; 24],
    /// Today so far.
    today: Agg,
}

/// One row of "top chats this week".
#[derive(Clone, Debug, PartialEq)]
struct TopRow {
    title: String,
    account: String,
    /// Tokens by model family (stats::FAMILIES).
    mix: [u64; 5],
    tokens: u64,
    context: u64,
    past: bool,
}

#[derive(Clone, Default, Debug)]
struct TopPanel {
    rows: Vec<TopRow>,
    /// Transcript files still waiting to be counted (0: all counted).
    left: usize,
    /// At least one pass has finished or published.
    ready: bool,
    /// Chats in the last 7 days whose last turn left more than 300k of context (all of them, not just the rows).
    missed: usize,
}

struct Dash {
    hwnd: HWND,
    gfx: Option<Gfx>,
    fx: Option<Fx>,
    big: Option<IDWriteTextFormat>,
    scale: f32,
    range: usize,
    metric: usize,
    /// Index into `accounts`; None: all.
    account: Option<usize>,
    /// Account names in list order (Claude, then Codex sign-ins), then "Terminal and other".
    accounts: Vec<String>,
    pods: Vec<Pod>,
    /// Last CPU reading per account: (total 100 ns, when).
    cpu: HashMap<String, (u64, Instant)>,
    summary: Option<Summary>,
    /// 0: the ranked lists, 1: top chats this week.
    bottom: usize,
    top: TopPanel,
    /// Last 7 days: tokens by account and model family (the "Last week" block).
    week: std::collections::BTreeMap<String, [u64; 5]>,
    /// The days the chart shows (yyyymmdd), oldest first.
    days: Vec<u32>,
    hits: Vec<(R, DHit)>,
    hover: Option<DHit>,
    tracking: bool,
    updated: Option<u64>,
    /// Animation clock, and when the numbers last changed (they grow in from there).
    born: Instant,
    shown_at: Instant,
    /// Frames drawn and their total time, for one log line about speed.
    frames: (u32, f64),
    /// 0: usage, 1: memory.
    view: usize,
    mem: Option<crate::memory::Health>,
    /// When the memory numbers were read, and when they first showed (they grow in).
    mem_at: Option<Instant>,
    mem_shown: Instant,
    /// 0: the note browser, 1: the stats.
    mem_page: usize,
    /// The note browser: what is typed, which folder, the matching notes (indexes into
    /// `mem.list`), the chosen note and its text.
    query: String,
    pick: Pick,
    found: Vec<usize>,
    sel: Option<PathBuf>,
    body: Option<(PathBuf, String)>,
    /// First row shown in the folder and note lists, and the note text's scroll (DIPs).
    folder_top: usize,
    list_top: usize,
    body_top: f32,
    /// Measured while drawing: rows that fit in the folder and note lists, and how far
    /// the note text can scroll.
    fits: Cell<(usize, usize)>,
    body_max: Cell<f32>,
    /// Delete asks twice: the note and when it was first clicked.
    confirm: Option<(PathBuf, Instant)>,
    toast: Option<(String, bool, Instant)>,
    /// Wrapped text for the note, and its laid-out text (path, width, length).
    wrap: Option<IDWriteTextFormat>,
    layout: RefCell<Option<(PathBuf, f32, usize, IDWriteTextLayout, f32)>>,
}

/// GPU resources for one render target.
struct Fx {
    rt: ID2D1HwndRenderTarget,
    res: Res,
    /// The size the target was made or last resized for.
    size: D2D_SIZE_U,
}

/// Brushes and the logo made for a render target.
struct Res {
    brush: ID2D1SolidColorBrush,
    /// A soft round light per color (made on first use).
    glows: RefCell<HashMap<u32, ID2D1RadialGradientBrush>>,
    /// A top-lit vertical fade per color, for bars.
    fades: RefCell<HashMap<u32, ID2D1LinearGradientBrush>>,
    /// The background's facets for one size: (width, height, [(shape, seed, warmth)]).
    facets: RefCell<Option<(f32, f32, Vec<(ID2D1PathGeometry, f32, f32)>)>>,
    logo: Option<ID2D1Bitmap>,
}

thread_local! {
    static DASH: RefCell<Option<Dash>> = const { RefCell::new(None) };
}

fn dwith<T>(f: impl FnOnce(&mut Dash) -> T) -> Option<T> {
    DASH.with(|c| c.try_borrow_mut().ok().and_then(|mut d| d.as_mut().map(f)))
}

/// The glowing version of each account's color.
fn color_of(accounts: &[String], name: &str) -> C {
    if name == stats::OTHER {
        return hex(0x9A938C, 1.0);
    }
    const NEON: [u32; 8] = [0xFF8A3D, 0x4FB3FF, 0x5BE38A, 0xB58CFF, 0x2EE6D6, 0xFFD24D, 0xFF5FA2, 0x9BE15D];
    let i = accounts.iter().position(|a| a == name).unwrap_or(0);
    hex(NEON[i % NEON.len()], 1.0)
}

fn mix(a: C, b: C, k: f32) -> C {
    let k = k.clamp(0.0, 1.0);
    C(a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k, a.2 + (b.2 - a.2) * k, a.3 + (b.3 - a.3) * k)
}

fn alpha(c: C, a: f32) -> C {
    C(c.0, c.1, c.2, a)
}

fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    1.0 - (1.0 - x).powi(3)
}

/// A steady 0..1 hash of a number (for the facets' shimmer).
fn hash01(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xffff) as f32 / 65535.0
}

const AMBER: u32 = 0xF57A08;

/// Open the dashboard on a tab (0: usage, 1: memory).
pub fn open_view(view: usize) {
    open();
    dwith(|d| d.view = view);
    if view == 1 {
        load_mem();
    }
}

/// Open the dashboard (or bring it forward), and read new transcripts in the background.
pub fn open() {
    let existing = dwith(|d| d.hwnd);
    let hwnd = match existing {
        Some(h) => h,
        None => match unsafe { create_window() } {
            Some(h) => h,
            None => {
                crate::log(&format!("dashboard: the window couldn't be created (error {})", unsafe { windows_sys::Win32::Foundation::GetLastError() }));
                return;
            }
        },
    };
    refresh_pods();
    poll_live();
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetTimer(hwnd, TIMER_FRAME, 33, None);
        SetTimer(hwnd, TIMER_LIVE, 2000, None);
    }
    crate::win::bring_forward(hwnd);
    if DATA.lock().ok().is_some_and(|d| d.is_some()) {
        recompute();
    }
    load(hwnd);
}

/// Every place the transcripts live: Claude Code's, and each Codex sign-in's chats.
fn roots(cfg: &crate::config::Config) -> Vec<(std::path::PathBuf, stats::Source)> {
    let mut roots = vec![(crate::claude::projects_dir(), stats::Source::Claude)];
    if cfg.codex.enabled {
        let homes = std::iter::once(("Codex".to_string(), crate::codex::home(cfg))).chain(cfg.codex.accounts.iter().map(|a| (a.name.clone(), crate::codex::account_home(cfg, &a.name))));
        for (name, home) in homes {
            for sub in ["sessions", "archived_sessions"] {
                roots.push((home.join(sub), stats::Source::Codex(name.clone())));
            }
        }
    }
    roots
}

/// Read new or changed transcripts, then match chats to accounts (worker thread).
fn load(hwnd: HWND) {
    if LOADING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(cfg) = tray::config() else {
        LOADING.store(false, Ordering::SeqCst);
        return;
    };
    let _ = hwnd;
    load_top(cfg.clone());
    std::thread::spawn(move || {
        // However this ends (even a panic), the next refresh can run.
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                LOADING.store(false, Ordering::SeqCst);
            }
        }
        let _done = Done;
        let started = Instant::now();
        let file = crate::data_file("hive-stats.json");
        // The index in memory (a copy, while the window still draws from it), else the saved one.
        let held = DATA.lock().ok().and_then(|d| d.as_ref().map(|(i, _)| i.clone()));
        let mut index = held.map_or_else(|| Index::load(&file), |i| Arc::try_unwrap(i).unwrap_or_else(|i| Index::clone(&i)));
        let last = std::sync::atomic::AtomicU64::new(0);
        let read = index.update(&roots(&cfg), &|done, of| {
            PROGRESS.store(((done as u64) << 32) | of as u64, Ordering::SeqCst);
            let now = crate::time::now_ms();
            if now - last.load(Ordering::Relaxed) > 150 {
                last.store(now, Ordering::Relaxed);
                unsafe { PostMessageW(dash_hwnd(), WM_DASH_PROGRESS, 0, 0) };
            }
        });
        if read > 0 {
            index.save(&file);
        }
        // Each account's chats name their Claude Code session (cliSessionId).
        let mut cache = crate::claude::ChatCache::load(&crate::data_file("hive-chats.json"));
        let mut map = HashMap::new();
        for a in &cfg.accounts {
            for c in crate::claude::chats_cached(&cfg, a, &mut cache) {
                if !c.cli.is_empty() {
                    map.insert(c.cli, a.name.clone());
                }
            }
        }
        crate::log(&format!("dashboard: read {read} transcripts in {} ms", started.elapsed().as_millis()));
        if let Ok(mut d) = DATA.lock() {
            *d = Some((Arc::new(index), Arc::new(map)));
        }
        PROGRESS.store(0, Ordering::SeqCst);
        LOADING.store(false, Ordering::SeqCst);
        unsafe { PostMessageW(dash_hwnd(), WM_DASH_READY, 0, 0) };
    });
}

/// Count the last week's tokens per chat (worker thread, low priority). Reads only what is new
/// since the last pass, a bounded amount per pass; the panel says "counting..." until it is done.
fn load_top(cfg: crate::config::Config) {
    if TOP_BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                TOP_BUSY.store(false, Ordering::SeqCst);
            }
        }
        let _done = Done;
        let _bg = stats::low_priority();
        let file = crate::data_file("hive-top-chats.json");
        let mut idx = TOP_INDEX.lock().ok().and_then(|mut i| i.take()).unwrap_or_else(|| ChatIndex::load(&file));
        // Title and account of every chat Hive knows (the chat cache keeps them).
        let mut cache = crate::claude::ChatCache::load(&crate::data_file("hive-chats.json"));
        let mut known: HashMap<String, (String, String)> = HashMap::new();
        for a in &cfg.accounts {
            for c in crate::claude::chats_cached(&cfg, a, &mut cache) {
                if !c.cli.is_empty() {
                    known.insert(c.cli, (a.name.clone(), c.title));
                }
            }
        }
        let root = crate::claude::projects_dir();
        let started = Instant::now();
        let mut passes = 0;
        loop {
            let now = crate::time::now();
            let (read, left) = idx.update(&root, now, std::time::Duration::from_secs(20));
            if idx.take_dirty() {
                idx.save(&file);
            }
            let all = idx.top(now, usize::MAX);
            let missed = stats::missed_handoffs(&all);
            let rows = all.iter().take(10).map(|c| top_row(c, &known)).collect();
            if let Ok(mut t) = TOP.lock() {
                *t = Some(TopPanel { rows, left, ready: true, missed });
            }
            unsafe { PostMessageW(dash_hwnd(), WM_DASH_TOP, 0, 0) };
            passes += 1;
            // Keep going while there is work and the pass made progress (and not for ever).
            if left == 0 || read == 0 || started.elapsed().as_secs() > 540 {
                break;
            }
        }
        crate::log(&format!("dashboard: top chats counted in {} ms ({passes} pass(es))", started.elapsed().as_millis()));
        if let Ok(mut i) = TOP_INDEX.lock() {
            *i = Some(idx);
        }
    });
}

fn top_row(c: &stats::TopChat, known: &HashMap<String, (String, String)>) -> TopRow {
    let (account, title) = known.get(&c.session).cloned().unwrap_or_else(|| (stats::OTHER.to_string(), format!("Terminal chat {}", c.session.chars().take(8).collect::<String>())));
    TopRow { title: if title.trim().is_empty() { "(untitled chat)".into() } else { title }, account, mix: c.mix, tokens: c.tokens, context: c.context, past: c.past_handoff() }
}

/// Read the memory vault's health (worker thread), then show it.
fn load_mem() {
    if MEM_BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let h = crate::memory::health();
        if let Ok(mut m) = MEM.lock() {
            *m = Some(h);
        }
        MEM_BUSY.store(false, Ordering::SeqCst);
        unsafe { PostMessageW(dash_hwnd(), WM_DASH_MEM, 0, 0) };
    });
}

/// "12 min ago" and the like; "never" for 0.
fn ago(at: u64) -> String {
    if at == 0 {
        return "never".into();
    }
    let s = crate::time::now().saturating_sub(at);
    match s {
        0..=89 => "just now".into(),
        90..=5399 => format!("{} min ago", (s + 30) / 60),
        5400..=172_799 => format!("{} h ago", (s + 1800) / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// Each account's limits and where they're heading, and which accounts there are.
fn refresh_pods() {
    let Some(cfg) = tray::config() else { return };
    let (names, pods) = pods_data(&cfg);
    apply_pods(names, pods);
}

/// The accounts' limits, read from disk (any thread).
fn pods_data(cfg: &crate::config::Config) -> (Vec<String>, Vec<Pod>) {
    let cfg = cfg.clone();
    let names: Vec<String> = cfg
        .accounts
        .iter()
        .map(|a| a.name.clone())
        .chain(cfg.codex.enabled.then(|| "Codex".to_string()))
        .chain(cfg.codex.accounts.iter().map(|a| a.name.clone()))
        .chain([stats::OTHER.to_string()])
        .collect();
    let mut pods = Vec::new();
    let mut limit_inputs = Vec::new();
    for a in &cfg.accounts {
        let org = match crate::claude::org(&cfg, a) {
            crate::claude::Org::Id(id) => Some(id),
            _ => None,
        };
        let u = crate::claude::usage(&cfg, a, org.as_deref());
        let s = crate::claude::usage_samples(&cfg, a, org.as_deref());
        let fh: Vec<(u64, u8)> = s.iter().map(|x| (x.0, x.1)).collect();
        let sd: Vec<(u64, u8)> = s.iter().map(|x| (x.0, x.2)).collect();
        let (five, five_fc, week, week_fc, as_of) = match &u {
            Some(u) => (
                u.five_hour,
                if u.five_hour.is_some() { stats::forecast(&fh, 3 * 3600, u.five_reset) } else { Forecast::Unknown },
                Some(u.week),
                stats::forecast(&sd, 24 * 3600, u.week_reset.map(|w| w.0)),
                u.as_of,
            ),
            None => (None, Forecast::Unknown, None, Forecast::Unknown, 0),
        };
        limit_inputs.push(stats::LimitInput { account: a.name.clone(), samples: s.clone(), five_reset: u.as_ref().and_then(|u| u.five_reset), week_reset: u.as_ref().and_then(|u| u.week_reset.map(|w| w.0)) });
        pods.push(Pod { name: a.name.clone(), color: color_of(&names, &a.name), codex: false, open: false, chats: 0, busy: 0.0, five, five_fc, week, week_fc, as_of, past95: 0, last95: None, hours: [0.0; 24], today: Agg::default() });
    }
    if cfg.codex.enabled {
        let cx = crate::codex::read(&cfg);
        // Codex's own windows: the 5-hour one (300 minutes) and the weekly one (a week or more).
        let pct = |label: &str| {
            cx.as_ref().and_then(|c| c.windows.iter().find(|w| if label == "5" { w.minutes == 300 } else { w.minutes >= 7 * 24 * 60 }).map(|w| w.pct.min(100)))
        };
        for name in std::iter::once("Codex".to_string()).chain(cfg.codex.accounts.iter().map(|a| a.name.clone())) {
            let main = name == "Codex";
            pods.push(Pod {
                color: color_of(&names, &name),
                name,
                codex: true,
                open: false,
                chats: 0,
                busy: 0.0,
                five: if main { pct("5") } else { None },
                five_fc: Forecast::Unknown,
                week: if main { pct("week") } else { None },
                week_fc: Forecast::Unknown,
                as_of: 0,
                past95: 0,
                last95: None,
                hours: [0.0; 24],
                today: Agg::default(),
            });
        }
    }
    // Log any new crossings (this also reads samples from before Hive first ran), then
    // count the last 7 days per account.
    let (dir, now) = (crate::exe_dir(), crate::time::now());
    stats::record_and_log(&dir, &limit_inputs, now, &crate::log);
    let counts = stats::past95(&dir, now);
    for p in pods.iter_mut().filter(|p| !p.codex) {
        if let Some(&(n, last)) = counts.get(&p.name) {
            (p.past95, p.last95) = (n, last);
        }
    }
    (names, pods)
}

fn apply_pods(names: Vec<String>, mut pods: Vec<Pod>) {
    dwith(|d| {
        // Keep the live readings across a refresh.
        for p in &mut pods {
            if let Some(old) = d.pods.iter().find(|o| o.name == p.name) {
                (p.open, p.chats, p.busy, p.hours, p.today) = (old.open, old.chats, old.busy, old.hours, old.today);
            }
        }
        d.accounts = names;
        d.pods = pods;
        if d.account.is_some_and(|i| i >= d.accounts.len()) {
            d.account = None;
        }
    });
}

/// What each account is doing right now: open or not, chats loaded, CPU in use. Read on
/// a worker thread; every 30 s it also re-reads each account's limits, so the numbers
/// move while the window stays open.
fn poll_live() {
    if LIVE_BUSY.swap(true, Ordering::SeqCst) {
        return; // the last reading is still being made
    }
    let Some(cfg) = tray::config() else {
        LIVE_BUSY.store(false, Ordering::SeqCst);
        return;
    };
    let tick = LIVE_TICKS.fetch_add(1, Ordering::SeqCst);
    std::thread::spawn(move || {
        let act = crate::procs::activity(&cfg);
        let pods = (tick % 15 == 14).then(|| pods_data(&cfg));
        if let Ok(mut l) = LIVE.lock() {
            *l = Some((act, pods));
        }
        LIVE_BUSY.store(false, Ordering::SeqCst);
        unsafe { PostMessageW(dash_hwnd(), WM_DASH_LIVE, 0, 0) };
    });
}

/// Show the latest live reading (window thread).
fn apply_live() {
    let Some((act, pods)) = LIVE.lock().ok().and_then(|mut l| l.take()) else { return };
    if let Some((names, pods)) = pods {
        apply_pods(names, pods);
    }
    let cores = std::thread::available_parallelism().map_or(4, |c| c.get()) as f64;
    dwith(|d| {
        let now = Instant::now();
        for p in &mut d.pods {
            match act.get(&p.name) {
                Some(&(chats, cpu)) => {
                    p.open = true;
                    p.chats = chats;
                    let rate = match d.cpu.get(&p.name) {
                        // Share of one core used since the last reading (100 ns units).
                        Some(&(prev, at)) if cpu >= prev => {
                            let secs = now.duration_since(at).as_secs_f64().max(0.5);
                            ((cpu - prev) as f64 / 1e7 / secs / cores.min(8.0) * 4.0).min(1.0) as f32
                        }
                        // A chat closed since (its time left the sum): hold the light steady.
                        Some(_) => p.busy,
                        None => 0.0,
                    };
                    p.busy = p.busy * 0.5 + rate * 0.5;
                    d.cpu.insert(p.name.clone(), (cpu, now));
                }
                None => {
                    (p.open, p.chats, p.busy) = (false, 0, 0.0);
                    d.cpu.remove(&p.name);
                }
            }
        }
    });
}

/// Totals for the chosen range and account, and each account's last 24 hours.
fn recompute() {
    let Some((index, map)) = DATA.lock().ok().and_then(|d| d.clone()) else { return };
    dwith(|d| {
        let days = RANGES[d.range].1;
        let today = crate::time::now();
        let first_day = if days == 0 {
            index.groups().map(|(k, _)| k.day).min().unwrap_or_else(|| ymd(today))
        } else {
            ymd(today.saturating_sub((days - 1) * 86_400))
        };
        let only = d.account.and_then(|i| d.accounts.get(i).cloned());
        let s = stats::summarize(&index, &map, first_day, &|a| only.as_deref().map_or(true, |o| o == a));
        let mut list = Vec::new();
        let mut t = today;
        loop {
            let day = ymd(t);
            if day < first_day || list.len() > 2000 {
                break;
            }
            list.push(day);
            t = t.saturating_sub(86_400);
        }
        list.reverse();
        // The last 24 hours per account (slot 23 is this hour).
        let mut slots: HashMap<(u32, u8), usize> = HashMap::new();
        for i in 0..24u64 {
            let st = crate::time::local(today.saturating_sub(i * 3600));
            slots.insert((st.wYear as u32 * 10000 + st.wMonth as u32 * 100 + st.wDay as u32, st.wHour as u8), 23 - i as usize);
        }
        let mut hours: HashMap<&str, [f64; 24]> = HashMap::new();
        let mut todays: HashMap<&str, Agg> = HashMap::new();
        let today_ymd = ymd(today);
        for (k, tk) in index.groups() {
            if k.day == today_ymd {
                let account = match k.session.strip_prefix(stats::DIRECT) {
                    Some(a) => a,
                    None => map.get(&k.session).map_or(stats::OTHER, String::as_str),
                };
                let e = todays.entry(account).or_default();
                e.tokens.add(tk);
                e.cost += stats::cost(&k.model, k.fast, tk).unwrap_or(0.0);
            }
            let Some(&slot) = slots.get(&(k.day, k.hour)) else { continue };
            let account = match k.session.strip_prefix(stats::DIRECT) {
                Some(a) => a,
                None => map.get(&k.session).map_or(stats::OTHER, String::as_str),
            };
            hours.entry(account).or_insert([0.0; 24])[slot] += tk.total() as f64;
        }
        for p in &mut d.pods {
            p.hours = hours.get(p.name.as_str()).copied().unwrap_or([0.0; 24]);
            p.today = todays.get(p.name.as_str()).copied().unwrap_or_default();
        }
        // The last 7 local days for every account (the "Last week" block ignores the filters).
        d.week = stats::week_by_account(&index, &map, ymd(today.saturating_sub(6 * 86_400)));
        d.days = list;
        d.summary = Some(s);
        d.updated = Some(crate::time::now());
        d.shown_at = Instant::now();
    });
}

/// Local date (yyyymmdd) of a time.
fn ymd(secs: u64) -> u32 {
    let st = crate::time::local(secs);
    st.wYear as u32 * 10000 + st.wMonth as u32 * 100 + st.wDay as u32
}

fn day_label(day: u32) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = ((day / 100) % 100) as usize;
    format!("{} {}", MONTHS.get(m.wrapping_sub(1)).unwrap_or(&""), day % 100)
}

pub fn money(v: f64) -> String {
    if v >= 10_000.0 {
        format!("${:.1}k", v / 1000.0)
    } else if v >= 100.0 {
        let n = v.round() as u64;
        if n >= 1000 { format!("${},{:03}", n / 1000, n % 1000) } else { format!("${n}") }
    } else {
        format!("${v:.2}")
    }
}

pub fn count(v: u64) -> String {
    let f = v as f64;
    if f >= 1e9 {
        format!("{:.1}B", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}M", f / 1e6)
    } else if f >= 1e4 {
        format!("{:.0}k", f / 1e3)
    } else if f >= 1e3 {
        format!("{:.1}k", f / 1e3)
    } else {
        v.to_string()
    }
}

fn value(metric: usize, a: &Agg) -> f64 {
    if metric == 0 { a.cost } else { a.tokens.total() as f64 }
}

fn show(metric: usize, v: f64) -> String {
    if metric == 0 { money(v) } else { count(v as u64) }
}

/// "5:12 PM" today, else "Thu 9:12 PM".
fn short_when(t: u64) -> String {
    let (at, now) = (crate::time::local(t), crate::time::local(crate::time::now()));
    let clock = crate::time::clock_at(t);
    if (at.wYear, at.wMonth, at.wDay) == (now.wYear, now.wMonth, now.wDay) {
        clock
    } else {
        format!("{} {clock}", WEEKDAYS[at.wDayOfWeek as usize % 7])
    }
}

fn forecast_short(f: Forecast) -> String {
    match f {
        Forecast::Reached => "limit hit".into(),
        Forecast::At(t) => format!("limit {}", short_when(t)),
        Forecast::Lasts => "lasts to reset".into(),
        Forecast::Unknown => String::new(),
    }
}

// ---- shapes and light ---------------------------------------------------------------

impl Res {
    fn new(rt: &ID2D1RenderTarget) -> Option<Res> {
        let brush = unsafe { rt.CreateSolidColorBrush(&d2c(C(1.0, 1.0, 1.0, 1.0)), None) }.ok()?;
        Some(Res { brush, glows: RefCell::new(HashMap::new()), fades: RefCell::new(HashMap::new()), facets: RefCell::new(None), logo: logo_bitmap(rt) })
    }
}

struct Ink<'a> {
    p: Painter<'a>,
    res: &'a Res,
    t: f32,
}

impl Ink<'_> {
    /// A soft round light in color `c`: bright in the middle, fading to nothing.
    fn halo(&self, cx: f32, cy: f32, r: f32, c: C, a: f32) {
        if a <= 0.005 || r <= 0.5 {
            return;
        }
        let key = ((c.0 * 255.0) as u32) << 16 | ((c.1 * 255.0) as u32) << 8 | (c.2 * 255.0) as u32;
        let mut glows = self.res.glows.borrow_mut();
        if !glows.contains_key(&key) {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: d2c(alpha(c, 1.0)) },
                D2D1_GRADIENT_STOP { position: 0.3, color: d2c(alpha(c, 0.5)) },
                D2D1_GRADIENT_STOP { position: 0.65, color: d2c(alpha(c, 0.14)) },
                D2D1_GRADIENT_STOP { position: 1.0, color: d2c(alpha(c, 0.0)) },
            ];
            let made = unsafe {
                self.p.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP).and_then(|coll| {
                    let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES { center: pt(0.0, 0.0), gradientOriginOffset: pt(0.0, 0.0), radiusX: 1.0, radiusY: 1.0 };
                    self.p.rt.CreateRadialGradientBrush(&props, None, &coll)
                })
            };
            let Ok(b) = made else { return };
            glows.insert(key, b);
        }
        let g = &glows[&key];
        unsafe {
            g.SetCenter(pt(cx, cy));
            g.SetRadiusX(r);
            g.SetRadiusY(r);
            g.SetOpacity(a.clamp(0.0, 1.0));
            let e = D2D1_ELLIPSE { point: pt(cx, cy), radiusX: r, radiusY: r };
            self.p.rt.FillEllipse(&e, g);
        }
    }

    fn geometry(&self, pts: &[(f32, f32)], closed: bool) -> Option<ID2D1PathGeometry> {
        unsafe {
            let geo = self.p.g.factory.CreatePathGeometry().ok()?;
            let sink = geo.Open().ok()?;
            let (x, y) = *pts.first()?;
            sink.BeginFigure(pt(x, y), if closed { D2D1_FIGURE_BEGIN_FILLED } else { D2D1_FIGURE_BEGIN_HOLLOW });
            for &(x, y) in &pts[1..] {
                sink.AddLine(pt(x, y));
            }
            sink.EndFigure(if closed { D2D1_FIGURE_END_CLOSED } else { D2D1_FIGURE_END_OPEN });
            sink.Close().ok()?;
            Some(geo)
        }
    }

    fn poly(&self, pts: &[(f32, f32)], c: C) {
        if let Some(g) = self.geometry(pts, true) {
            self.p.color(c);
            unsafe { self.p.rt.FillGeometry(&g, self.p.brush, None) };
        }
    }

    fn poly_edge(&self, pts: &[(f32, f32)], c: C, w: f32) {
        if let Some(g) = self.geometry(pts, true) {
            self.p.color(c);
            unsafe { self.p.rt.DrawGeometry(&g, self.p.brush, w, None) };
        }
    }

    /// A card cut like a gem: two corners clipped, a lit top edge, an inner facet.
    fn gem(&self, r: R, cut: f32, tint: C, lit: f32) {
        let c2 = cut * 0.4;
        let shape = [(r.l + cut, r.t), (r.r - c2, r.t), (r.r, r.t + c2), (r.r, r.b - cut), (r.r - cut, r.b), (r.l + c2, r.b), (r.l, r.b - c2), (r.l, r.t + cut)];
        self.poly(&shape, hex(0x1A1512, 0.86));
        // A lighter facet across the upper part, and a darker one low right.
        let mid = r.t + (r.b - r.t) * 0.42;
        self.poly(&[(r.l + cut, r.t), (r.r - c2, r.t), (r.r, r.t + c2), (r.r, mid - 18.0), (r.l, mid + 10.0), (r.l, r.t + cut)], alpha(tint, 0.05 + 0.03 * lit));
        self.poly(&[(r.r, mid + 20.0), (r.r, r.b - cut), (r.r - cut, r.b), (r.r - (r.r - r.l) * 0.45, r.b)], hex(0x000000, 0.18));
        self.poly_edge(&shape, alpha(tint, 0.22 + 0.25 * lit), 1.0);
        // The lit top edge.
        self.p.line(r.l + cut + 2.0, r.t + 0.5, r.r - c2 - 2.0, r.t + 0.5, alpha(mix(tint, hex(0xFFFFFF, 1.0), 0.4), 0.35 + 0.3 * lit), 1.0);
    }

    /// A bar lit from the top: bright cap, glow above, color fading down.
    fn bar(&self, r: R, c: C, lit: f32) {
        if r.b - r.t < 0.5 {
            return;
        }
        let key = ((c.0 * 255.0) as u32) << 24 | ((c.1 * 255.0) as u32) << 16 | ((c.2 * 255.0) as u32) << 8 | (c.3 * 255.0) as u32;
        let mut fades = self.res.fades.borrow_mut();
        if !fades.contains_key(&key) {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: d2c(alpha(mix(c, hex(0xFFFFFF, 1.0), 0.22), c.3)) },
                D2D1_GRADIENT_STOP { position: 1.0, color: d2c(alpha(mix(c, hex(0x000000, 1.0), 0.25), c.3 * 0.55)) },
            ];
            let made = unsafe {
                self.p.rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP).and_then(|coll| {
                    let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(0.0, 0.0), endPoint: pt(0.0, 1.0) };
                    self.p.rt.CreateLinearGradientBrush(&props, None, &coll)
                })
            };
            if let Ok(b) = made {
                fades.insert(key, b);
            }
        }
        match fades.get(&key) {
            Some(b) => unsafe {
                b.SetStartPoint(pt(0.0, r.t));
                b.SetEndPoint(pt(0.0, r.b.max(r.t + 1.0)));
                self.p.rt.FillRectangle(&r.d2(), b);
            },
            None => self.p.fill(r, 0.0, c),
        }
        drop(fades);
        self.p.fill(R::new(r.l, r.t, r.r, r.t + 2.0_f32.min(r.b - r.t)), 0.0, alpha(mix(c, hex(0xFFFFFF, 1.0), 0.6), 0.95));
        if lit > 0.0 {
            self.halo((r.l + r.r) / 2.0, r.t, ((r.r - r.l) * 1.6).max(8.0), c, 0.35 * lit);
        }
    }

    /// A ring gauge: a faint track, the arc in color with a light at its end.
    fn ring(&self, cx: f32, cy: f32, rad: f32, width: f32, frac: f32, c: C) {
        unsafe {
            let e = D2D1_ELLIPSE { point: pt(cx, cy), radiusX: rad, radiusY: rad };
            self.p.color(hex(0xFFFFFF, 0.08));
            self.p.rt.DrawEllipse(&e, self.p.brush, width, None);
        }
        let frac = frac.clamp(0.0, 0.9999);
        if frac <= 0.001 {
            return;
        }
        let a0 = -std::f32::consts::FRAC_PI_2;
        let a1 = a0 + frac * std::f32::consts::TAU;
        let (sx, sy) = (cx + rad * a0.cos(), cy + rad * a0.sin());
        let (ex, ey) = (cx + rad * a1.cos(), cy + rad * a1.sin());
        unsafe {
            let Ok(geo) = self.p.g.factory.CreatePathGeometry() else { return };
            let Ok(sink) = geo.Open() else { return };
            sink.BeginFigure(pt(sx, sy), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: pt(ex, ey),
                size: D2D_SIZE_F { width: rad, height: rad },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: if frac > 0.5 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            if sink.Close().is_err() {
                return;
            }
            self.p.color(alpha(c, 0.25));
            self.p.rt.DrawGeometry(&geo, self.p.brush, width + 5.0, &self.p.g.round);
            self.p.color(c);
            self.p.rt.DrawGeometry(&geo, self.p.brush, width, &self.p.g.round);
        }
        self.halo(ex, ey, width * 2.6, c, 0.55);
        self.p.circle(ex, ey, width * 0.42, mix(c, hex(0xFFFFFF, 1.0), 0.65));
    }
}

/// The background: a field of dark facets, warmer toward the logo, shimmering.
fn backdrop(ink: &Ink, w: f32, h: f32) {
    ink.p.fill(R::new(0.0, 0.0, w, h), 0.0, hex(0x0D0B0A, 1.0));
    let mut cache = ink.res.facets.borrow_mut();
    if cache.as_ref().map_or(true, |(cw, ch, _)| (*cw - w).abs() > 0.5 || (*ch - h).abs() > 0.5) {
        let cell = 92.0;
        let (nx, ny) = ((w / cell).ceil() as i32 + 1, (h / cell).ceil() as i32 + 1);
        let point = |i: i32, j: i32| {
            let n = (i * 7919 + j * 104_729) as u32;
            (i as f32 * cell + (hash01(n) - 0.5) * cell * 0.7, j as f32 * cell + (hash01(n ^ 0xABCD) - 0.5) * cell * 0.7)
        };
        let mut list = Vec::new();
        for j in 0..ny {
            for i in 0..nx {
                let (a, b, c, d) = (point(i, j), point(i + 1, j), point(i, j + 1), point(i + 1, j + 1));
                for (k, tri) in [[a, b, c], [b, d, c]].iter().enumerate() {
                    let seed = hash01((i * 31 + j * 977 + k as i32 * 13) as u32);
                    let (cx, cy) = ((tri[0].0 + tri[1].0 + tri[2].0) / 3.0, (tri[0].1 + tri[1].1 + tri[2].1) / 3.0);
                    let warm = (1.0 - ((cx / w) * 0.8 + (cy / h) * 0.9)).clamp(0.0, 1.0);
                    if let Some(g) = ink.geometry(&[tri[0], tri[1], tri[2]], true) {
                        list.push((g, seed, warm));
                    }
                }
            }
        }
        *cache = Some((w, h, list));
    }
    if let Some((_, _, list)) = cache.as_ref() {
        for (g, seed, warm) in list {
            let shimmer = 0.5 + 0.5 * (ink.t * 0.55 + seed * 6.28).sin();
            let base = mix(hex(0x14110F, 1.0), hex(0x2B1C10, 1.0), seed * 0.6 + warm * 0.4);
            ink.p.color(alpha(base, 0.55 + 0.25 * seed + 0.12 * shimmer * warm));
            unsafe { ink.p.rt.FillGeometry(g, ink.p.brush, None) };
        }
    }
    drop(cache);
    ink.halo(60.0, 40.0, 420.0, hex(AMBER, 1.0), 0.22);
    ink.halo(w - 80.0, h - 60.0, 520.0, hex(0x6B2E06, 1.0), 0.18);
}

// ---- the picture -------------------------------------------------------------------

impl Dash {
    fn paint(&self, ink: &Ink, w: f32, h: f32, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        backdrop(ink, w, h);
        let x0 = 24.0;
        let x1 = w - 24.0;
        let amber = hex(AMBER, 1.0);

        // Header: the mark, glowing and breathing.
        let breathe = 0.75 + 0.25 * (ink.t * 1.3).sin();
        ink.halo(x0 + 22.0, 38.0, 46.0, amber, 0.45 * breathe);
        if let Some(b) = &ink.res.logo {
            let r = D2D_RECT_F { left: x0, top: 16.0, right: x0 + 44.0, bottom: 60.0 };
            unsafe { p.rt.DrawBitmap(b, Some(&r), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None) };
        }
        p.text(crate::APP_NAME, &p.g.title, R::new(x0 + 56.0, 16.0, x0 + 420.0, 38.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
        p.text(if self.view == 1 { "MEMORY" } else { "USAGE DASHBOARD" }, &p.g.tiny, R::new(x0 + 57.0, 38.0, x0 + 420.0, 56.0), alpha(amber, 0.9), DWRITE_TEXT_ALIGNMENT_LEADING);
        let progress = PROGRESS.load(Ordering::SeqCst);
        let loading = LOADING.load(Ordering::SeqCst);
        let status = if self.view == 1 {
            match &self.mem_at {
                Some(at) if !MEM_BUSY.load(Ordering::SeqCst) => format!("VAULT  \u{00B7}  read {}", crate::time::clock_at(crate::time::now().saturating_sub(at.elapsed().as_secs()))),
                _ => "Reading the vault".into(),
            }
        } else if loading {
            let (done, of) = (progress >> 32, progress & 0xffff_ffff);
            if of > 0 { format!("Reading chat history  {done} / {of}") } else { "Reading chat history".into() }
        } else {
            self.updated.map_or(String::new(), |u| format!("LIVE  \u{00B7}  updated {}", crate::time::clock_at(u)))
        };
        let rr = R::new(x1 - 34.0, 20.0, x1, 54.0);
        p.text(&status, &p.g.small, R::new(x1 - 400.0, 20.0, rr.l - 26.0, 54.0), t.text2, DWRITE_TEXT_ALIGNMENT_TRAILING);
        let dot = (rr.l - 16.0, 37.0);
        let loading = if self.view == 1 { MEM_BUSY.load(Ordering::SeqCst) } else { loading };
        let pulse = 0.5 + 0.5 * (ink.t * if loading { 6.0 } else { 2.2 }).sin();
        ink.halo(dot.0, dot.1, 14.0, if loading { amber } else { hex(0x5BE38A, 1.0) }, 0.5 + 0.4 * pulse);
        p.circle(dot.0, dot.1, 3.5, if loading { amber } else { hex(0x9CF5B8, 1.0) });
        if self.hover == Some(DHit::Refresh) {
            p.fill(rr, 6.0, hex(0xFFFFFF, 0.08));
        }
        p.text("\u{E72C}", &p.g.icon, rr, t.text, DWRITE_TEXT_ALIGNMENT_CENTER);
        hits.push((rr, DHit::Refresh));

        // Filters.
        let y = 70.0;
        let mut x = chips(ink, hits, self.hover, x0, y, &["Usage".to_string(), "Memory".to_string()], Some(self.view), DHit::View) + 18.0;
        if self.view == 1 {
            chips(ink, hits, self.hover, x, y, &["Notes".to_string(), "Stats".to_string()], Some(self.mem_page), DHit::MemPage);
            let area = R::new(x0, 114.0, x1, h - 30.0);
            if self.mem_page == 0 {
                self.paint_notes(ink, area, hits);
            } else {
                self.paint_memory(ink, area, hits);
            }
            let foot = R::new(x0, h - 24.0, x1, h - 6.0);
            match &self.toast {
                Some((msg, ok, at)) if at.elapsed().as_secs() < 8 => p.text(msg, &p.g.small, foot, hex(if *ok { GREEN } else { RED }, 1.0), DWRITE_TEXT_ALIGNMENT_LEADING),
                _ if self.mem_page == 0 => p.text(
                    "Type to search every note. Up and Down move through the list, Enter opens the note, Esc clears the search. Delete asks twice; git history keeps every deleted note.",
                    &p.g.tiny,
                    foot,
                    t.text3,
                    DWRITE_TEXT_ALIGNMENT_LEADING,
                ),
                _ => p.text("Memory uses no AI: the harvest copies what chats already wrote. Tokens are estimated as characters / 4.", &p.g.tiny, foot, t.text3, DWRITE_TEXT_ALIGNMENT_LEADING),
            }
            return;
        }
        x = chips(ink, hits, self.hover, x, y, &RANGES.iter().map(|r| r.0.to_string()).collect::<Vec<_>>(), Some(self.range), DHit::Range);
        x = chips(ink, hits, self.hover, x + 18.0, y, &METRICS.iter().map(|m| m.to_string()).collect::<Vec<_>>(), Some(self.metric), DHit::Metric);
        let mut names = vec!["All".to_string()];
        names.extend(self.accounts.iter().cloned());
        chips(ink, hits, self.hover, x + 18.0, y, &names, Some(self.account.map_or(0, |i| i + 1)), |i| DHit::Account(i.checked_sub(1)));

        // Live pods: one per account.
        let py = 114.0;
        let ph = 196.0;
        self.paint_pods(ink, R::new(x0, py, x1, py + ph));

        let Some(s) = &self.summary else {
            let msg = if loading { "Reading every Claude Code and Codex chat on this PC. The first time takes about half a minute." } else { "No chat history found." };
            p.text(msg, &p.g.body, R::new(x0, py + ph + 30.0, x1, py + ph + 60.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        };
        let grow = ease(self.shown_at.elapsed().as_secs_f32() / 0.9);
        let m = self.metric;

        // Tiles.
        let ty = py + ph + GAP;
        let th = 76.0;
        let tw = (x1 - x0 - 3.0 * GAP) / 4.0;
        let days_with = s.by_day.len().max(1);
        let total = s.total.tokens.total();
        let cache_share = if total > 0 { 100.0 * s.total.tokens.read as f64 / total as f64 } else { 0.0 };
        let busiest = s.by_day.iter().map(|(d, a)| (*d, a.values().map(|x| value(m, x)).sum::<f64>())).max_by(|a, b| a.1.total_cmp(&b.1));
        let g = grow as f64;
        let tiles = [
            ("COST AT API PRICES", money(s.total.cost * g), format!("{} a day", money(s.total.cost / days_with as f64)), amber),
            ("TOKENS", count((total as f64 * g) as u64), format!("{cache_share:.0}% cache reads"), hex(0x4FB3FF, 1.0)),
            ("REPLIES", count((s.total.tokens.replies as f64 * g) as u64), format!("{} written", count(s.total.tokens.output)), hex(0x5BE38A, 1.0)),
            ("BUSIEST DAY", busiest.map_or("-".into(), |b| day_label(b.0)), busiest.map_or(String::new(), |b| show(m, b.1)), hex(0xFF5FA2, 1.0)),
        ];
        for (i, (label, big, sub, c)) in tiles.iter().enumerate() {
            let r = R::new(x0 + i as f32 * (tw + GAP), ty, x0 + i as f32 * (tw + GAP) + tw, ty + th);
            ink.gem(r, 12.0, *c, 0.4);
            ink.halo(r.l + 18.0, r.t + 18.0, 26.0, *c, 0.35);
            p.circle(r.l + 18.0, r.t + 18.0, 3.0, mix(*c, hex(0xFFFFFF, 1.0), 0.5));
            p.text(label, &p.g.tiny, R::new(r.l + 30.0, r.t + 9.0, r.r - 12.0, r.t + 27.0), alpha(*c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
            if let Some(f) = &self.big {
                p.text(big, f, R::new(r.l + 16.0, r.t + 26.0, r.r - 110.0, r.t + 58.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            }
            p.text(sub, &p.g.small, R::new(r.r - 170.0, r.t + 32.0, r.r - 14.0, r.t + 54.0), t.text2, DWRITE_TEXT_ALIGNMENT_TRAILING);
        }

        // The day chart and the heatmap.
        let cy = ty + th + GAP;
        let rest = h - cy - 30.0;
        let ch = match self.bottom {
            2 => (rest * 0.34).max(170.0),
            1 => (rest * 0.40).max(190.0),
            _ => (rest * 0.56).max(220.0),
        };
        let hw = 430.0;
        self.paint_chart(ink, R::new(x0, cy, x1 - hw - GAP, cy + ch), s, grow, hits);
        self.paint_heat(ink, R::new(x1 - hw, cy, x1, cy + ch), s, grow);

        // Ranked lists.
        let by = cy + ch + GAP;
        let past = self.top.missed;
        let labels = vec!["Models, projects, accounts".to_string(), if past > 0 { format!("Top chats this week ({past} past 300k)") } else { "Top chats this week".to_string() }, "Model share and last week".to_string()];
        chips(ink, hits, self.hover, x0, by, &labels, Some(self.bottom), DHit::Bottom);
        let ly = by + 34.0;
        let lh = h - ly - 30.0;
        if self.bottom == 2 {
            let sh = 118.0_f32.min(lh * 0.45);
            self.paint_share(ink, R::new(x0, ly, x1, ly + sh), s, grow);
            self.paint_week(ink, R::new(x0, ly + sh + GAP, x1, ly + lh));
            return self.paint_foot(ink, x0, x1, h, s, 2);
        }
        if self.bottom == 1 {
            self.paint_top(ink, R::new(x0, ly, x1, ly + lh));
            return self.paint_foot(ink, x0, x1, h, s, 1);
        }
        let cw = (x1 - x0 - 2.0 * GAP) / 3.0;
        let lists: [(&str, &std::collections::BTreeMap<String, Agg>, bool, C); 3] =
            [("MODELS", &s.by_model, false, amber), ("PROJECTS", &s.by_project, false, hex(0x2EE6D6, 1.0)), ("ACCOUNTS", &s.by_account, true, hex(0xB58CFF, 1.0))];
        for (i, (title, map, colored, c)) in lists.iter().enumerate() {
            let r = R::new(x0 + i as f32 * (cw + GAP), ly, x0 + i as f32 * (cw + GAP) + cw, ly + lh);
            self.paint_ranked(ink, r, title, map, *colored, *c, grow);
        }
        self.paint_foot(ink, x0, x1, h, s, 0);
    }

    fn paint_foot(&self, ink: &Ink, x0: f32, x1: f32, h: f32, s: &Summary, page: usize) {
        let p = &ink.p;
        let t = theme(true);
        if page == 2 {
            let note = "Share is each model family's part of the day's tokens (input, output, cache reads and writes). Last week is the last 7 days across all accounts, whatever the range and account filters say. Past 300k is the hand-off line.";
            p.text(note, &p.g.tiny, R::new(x0, h - 24.0, x1, h - 6.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        }
        if page == 1 {
            let note = "Tokens are input, output, cache reads and cache writes in the last 7 days, from each chat's transcript (helper agents included). Context is the last turn's input plus cache; past 300k is the hand-off line.";
            p.text(note, &p.g.tiny, R::new(x0, h - 24.0, x1, h - 6.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        }
        let note = format!(
            "Cost is what the same tokens would cost on the API, for comparison: a plan doesn't bill per token. Codex (OpenAI) models have no price here, so they count in tokens only.{}",
            if s.unpriced > 0 { format!(" Unpriced: {} tokens.", count(s.unpriced)) } else { String::new() }
        );
        p.text(&note, &p.g.tiny, R::new(x0, h - 24.0, x1, h - 6.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
    }

    /// One stacked bar per day: each model family's share of that day's tokens.
    fn paint_share(&self, ink: &Ink, r: R, s: &Summary, grow: f32) {
        let p = &ink.p;
        let t = theme(true);
        let c = hex(0xB58CFF, 1.0);
        ink.gem(r, 14.0, c, 0.2);
        p.text(&format!("MODEL SHARE OF TOKENS PER DAY  \u{00B7}  {}", RANGES[self.range].0.to_lowercase()), &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.l + 420.0, r.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        // Legend: each family's share of the whole range.
        let mut fam = [0u64; 5];
        for f in s.by_day_family.values() {
            for i in 0..5 {
                fam[i] += f[i];
            }
        }
        let all = fam.iter().sum::<u64>().max(1) as f64;
        let mut lx = r.r - 16.0;
        for i in (0..5).rev() {
            if fam[i] == 0 {
                continue;
            }
            let label = format!("{} {:.0}%", stats::FAMILIES[i], 100.0 * fam[i] as f64 / all);
            let wd = 14.0 + 7.0 * label.chars().count() as f32;
            lx -= wd + 10.0;
            p.fill(R::new(lx, r.t + 16.0, lx + 8.0, r.t + 24.0), 2.0, hex(FAM[i], 1.0));
            p.text(&label, &p.g.tiny, R::new(lx + 12.0, r.t + 12.0, lx + wd + 8.0, r.t + 28.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
        let area = R::new(r.l + 16.0, r.t + 34.0, r.r - 16.0, r.b - 26.0);
        let n = self.days.len().max(1) as f32;
        let slot = (area.r - area.l) / n;
        let gap = if slot > 4.0 { 1.5 } else if slot > 2.5 { 1.0 } else { 0.0 };
        let alert = stats::opus_alert();
        let mut red = 0;
        for (i, day) in self.days.iter().enumerate() {
            let l = area.l + i as f32 * slot;
            let col = R::new(l + gap / 2.0, area.t, (l + slot - gap / 2.0).max(l + 1.0), area.b);
            p.fill(col, 0.0, hex(0xFFFFFF, 0.04));
            let Some(sh) = stats::day_shares(&s.by_day_family, *day) else { continue };
            let hot = stats::opus_over(&sh, alert);
            if hot {
                red += 1;
                p.fill(R::new(col.l - 0.5, area.t - 2.0, col.r + 0.5, area.b + 5.0), 1.0, hex(RED, 0.20));
                p.fill(R::new(col.l, area.b + 2.0, col.r, area.b + 5.0), 0.0, hex(RED, 1.0));
            }
            let full = area.b - area.t;
            let mut y = area.b;
            for (f, v) in sh.iter().enumerate() {
                let hgt = full * *v as f32 * ease(grow * 1.3);
                if hgt >= 0.4 {
                    p.fill(R::new(col.l, y - hgt, col.r, y), 0.0, hex(FAM[f], 0.92));
                    y -= hgt;
                }
            }
        }
        let first = self.days.first().map_or(String::new(), |d| day_label(*d));
        let last = self.days.last().map_or(String::new(), |d| day_label(*d));
        let note = match alert {
            Some(a) => format!("Opus alert at {a:.0}%: {red} day{} in red", if red == 1 { "" } else { "s" }),
            None => "Opus alert off. Set HIVE_OPUS_ALERT=60 (percent) to mark days with that much Opus red.".to_string(),
        };
        p.text(&format!("{first}  to  {last}"), &p.g.tiny, R::new(r.l + 18.0, r.b - 20.0, r.l + 260.0, r.b - 4.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        p.text(&note, &p.g.tiny, R::new(r.l + 260.0, r.b - 20.0, r.r - 16.0, r.b - 4.0), if red > 0 { hex(RED, 1.0) } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_TRAILING);
    }

    /// Last 7 days: tokens by account and model, the top 3 chats, and the missed hand-offs.
    fn paint_week(&self, ink: &Ink, r: R) {
        let p = &ink.p;
        let t = theme(true);
        let w = r.r - r.l - 2.0 * GAP;
        let (wa, wc) = (w * 0.46, w * 0.34);
        let acc = R::new(r.l, r.t, r.l + wa, r.b);
        let chats = R::new(acc.r + GAP, r.t, acc.r + GAP + wc, r.b);
        let miss = R::new(chats.r + GAP, r.t, r.r, r.b);

        // Tokens by account, each a bar of its model mix.
        let c = hex(0xB58CFF, 1.0);
        ink.gem(acc, 14.0, c, 0.2);
        p.text("LAST WEEK  \u{00B7}  TOKENS BY ACCOUNT AND MODEL", &p.g.tiny, R::new(acc.l + 18.0, acc.t + 12.0, acc.r - 16.0, acc.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let mut rows: Vec<(&String, &[u64; 5], u64)> = self.week.iter().map(|(a, f)| (a, f, f.iter().sum::<u64>())).filter(|x| x.2 > 0).collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        let mut y = acc.t + 34.0;
        if rows.is_empty() {
            p.text("No tokens in the last 7 days.", &p.g.small, R::new(acc.l + 18.0, y, acc.r - 16.0, y + 18.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
        let row_h = ((acc.b - y - 6.0) / rows.len().max(1) as f32).floor().clamp(14.0, 24.0);
        let fit = (((acc.b - y - 4.0) / row_h).floor() as usize).max(1);
        for (a, f, total) in rows.iter().take(fit) {
            p.text(a, &p.g.small, R::new(acc.l + 18.0, y, acc.l + 18.0 + wa * 0.22, y + row_h), color_of(&self.accounts, a), DWRITE_TEXT_ALIGNMENT_LEADING);
            let bar = R::new(acc.l + 22.0 + wa * 0.22, y + row_h / 2.0 - 3.0, acc.l + 22.0 + wa * 0.44, y + row_h / 2.0 + 3.0);
            p.fill(bar, 3.0, hex(0xFFFFFF, 0.06));
            let mut x = bar.l;
            for (i, v) in f.iter().enumerate() {
                let wv = (bar.r - bar.l) * *v as f32 / *total as f32;
                if wv >= 0.5 {
                    p.fill(R::new(x, bar.t, (x + wv).min(bar.r), bar.b), 0.0, hex(FAM[i], 1.0));
                    x += wv;
                }
            }
            let mut sh: Vec<(usize, f32)> = f.iter().enumerate().map(|(i, v)| (i, 100.0 * *v as f32 / *total as f32)).filter(|x| x.1 >= 1.0).collect();
            sh.sort_by(|a, b| b.1.total_cmp(&a.1));
            let words: Vec<String> = sh.iter().take(2).map(|(i, pct)| format!("{} {:.0}%", stats::FAMILIES[*i], pct)).collect();
            p.text(&words.join("  "), &p.g.tiny, R::new(bar.r + 8.0, y + 2.0, acc.r - 90.0, y + row_h), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&count(*total), &p.g.small, R::new(acc.r - 90.0, y, acc.r - 16.0, y + row_h), t.text, DWRITE_TEXT_ALIGNMENT_TRAILING);
            y += row_h;
        }

        // The three chats with the most tokens.
        let c = hex(0xFF5FA2, 1.0);
        ink.gem(chats, 14.0, c, 0.2);
        let sub = if !self.top.ready { "counting..." } else if self.top.left > 0 { "still counting" } else { "" };
        p.text(&format!("TOP 3 CHATS  {sub}"), &p.g.tiny, R::new(chats.l + 18.0, chats.t + 12.0, chats.r - 16.0, chats.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let mut y = chats.t + 34.0;
        let ch_h = ((chats.b - y - 6.0) / 3.0).floor().clamp(14.0, 36.0);
        if self.top.rows.is_empty() {
            p.text(if self.top.ready && self.top.left == 0 { "No chats in the last 7 days." } else { "counting..." }, &p.g.small, R::new(chats.l + 18.0, y, chats.r - 16.0, y + 18.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
        for (i, row) in self.top.rows.iter().take(3).enumerate() {
            p.text(&format!("{}", i + 1), &p.g.tiny, R::new(chats.l + 16.0, y + 2.0, chats.l + 30.0, y + ch_h), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&row.title, &p.g.small, R::new(chats.l + 32.0, y, chats.r - 96.0, y + 16.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&count(row.tokens), &p.g.small, R::new(chats.r - 94.0, y, chats.r - 16.0, y + 16.0), t.text, DWRITE_TEXT_ALIGNMENT_TRAILING);
            if ch_h >= 28.0 {
                p.text(&row.account, &p.g.tiny, R::new(chats.l + 32.0, y + 16.0, chats.r - 96.0, y + 32.0), color_of(&self.accounts, &row.account), DWRITE_TEXT_ALIGNMENT_LEADING);
            }
            y += ch_h;
        }

        // Chats that ended past the hand-off line.
        let n = self.top.missed;
        let c = if n > 0 { hex(RED, 1.0) } else { hex(GREEN, 1.0) };
        ink.gem(miss, 14.0, c, 0.2);
        p.text("MISSED HAND-OFFS", &p.g.tiny, R::new(miss.l + 18.0, miss.t + 12.0, miss.r - 16.0, miss.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        if let Some(f) = &self.big {
            let txt = if self.top.ready { n.to_string() } else { "-".to_string() };
            p.text(&txt, f, R::new(miss.l + 18.0, miss.t + 34.0, miss.r - 16.0, miss.t + 68.0), if self.top.ready { c } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
        let line = if !self.top.ready { "counting...".to_string() } else if n == 0 { "no chat ended past 300k".to_string() } else { format!("chat{} ended past 300k context", if n == 1 { "" } else { "s" }) };
        p.text(&line, &p.g.small, R::new(miss.l + 18.0, miss.t + 70.0, miss.r - 12.0, miss.t + 88.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
    }

    fn paint_top(&self, ink: &Ink, r: R) {
        let p = &ink.p;
        let t = theme(true);
        let c = hex(0xFF5FA2, 1.0);
        ink.gem(r, 14.0, c, 0.2);
        let sub = if !self.top.ready {
            "counting...".to_string()
        } else if self.top.left > 0 {
            format!("counting...  {} transcripts left", self.top.left)
        } else {
            "last 7 days".to_string()
        };
        p.text(&format!("TOP CHATS THIS WEEK  \u{00B7}  {sub}"), &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        // Columns, from the left: rank, title, account, model bar and shares, tokens, context, hand-off mark.
        let w = r.r - r.l;
        let cols = [r.l + 16.0, r.l + 34.0, r.l + 34.0 + w * 0.27, r.l + 34.0 + w * 0.38, r.l + 34.0 + w * 0.62, r.r - 16.0 - w * 0.25 - 120.0, r.r - 16.0 - w * 0.25];
        for (x, label) in [(cols[1], "CHAT"), (cols[2], "ACCOUNT"), (cols[3], "MODELS")] {
            p.text(label, &p.g.tiny, R::new(x, r.t + 30.0, x + 120.0, r.t + 44.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
        p.text("TOKENS", &p.g.tiny, R::new(cols[5] - 60.0, r.t + 30.0, cols[5] + 20.0, r.t + 44.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        p.text("CONTEXT", &p.g.tiny, R::new(cols[6] - 90.0, r.t + 30.0, cols[6], r.t + 44.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        let mut y = r.t + 48.0;
        let row_h = ((r.b - y - 6.0) / 10.0).floor().clamp(16.0, 22.0);
        if self.top.rows.is_empty() {
            let msg = if self.top.ready && self.top.left == 0 { "No chats in the last 7 days." } else { "counting..." };
            p.text(msg, &p.g.small, R::new(r.l + 18.0, y, r.r - 16.0, y + 18.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        }
        for (i, row) in self.top.rows.iter().enumerate() {
            if y + row_h > r.b - 2.0 {
                break;
            }
            if row.past {
                p.fill(R::new(r.l + 8.0, y - 1.0, r.r - 8.0, y + row_h - 2.0), 4.0, hex(RED, 0.10));
            }
            p.text(&format!("{}", i + 1), &p.g.tiny, R::new(cols[0], y + 2.0, cols[1], y + row_h), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&row.title, &p.g.small, R::new(cols[1], y, cols[2] - 10.0, y + row_h), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&row.account, &p.g.small, R::new(cols[2], y, cols[3] - 10.0, y + row_h), color_of(&self.accounts, &row.account), DWRITE_TEXT_ALIGNMENT_LEADING);
            // Stacked bar of the model mix, then the two biggest shares as words.
            let total = row.mix.iter().sum::<u64>().max(1) as f32;
            let bar = R::new(cols[3], y + row_h / 2.0 - 3.0, cols[3] + 70.0, y + row_h / 2.0 + 3.0);
            p.fill(bar, 3.0, hex(0xFFFFFF, 0.06));
            let mut x = bar.l;
            for (f, v) in row.mix.iter().enumerate() {
                let wv = (bar.r - bar.l) * *v as f32 / total;
                if wv >= 0.5 {
                    p.fill(R::new(x, bar.t, (x + wv).min(bar.r), bar.b), 0.0, hex(FAM[f], 1.0));
                    x += wv;
                }
            }
            let mut shares: Vec<(usize, f32)> = row.mix.iter().enumerate().map(|(f, v)| (f, 100.0 * *v as f32 / total)).filter(|(_, pct)| *pct >= 1.0).collect();
            shares.sort_by(|a, b| b.1.total_cmp(&a.1));
            let words: Vec<String> = shares.iter().take(2).map(|(f, pct)| format!("{} {:.0}%", stats::FAMILIES[*f], pct)).collect();
            p.text(&words.join("  "), &p.g.tiny, R::new(bar.r + 8.0, y + 2.0, cols[5] - 62.0, y + row_h), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&count(row.tokens), &p.g.small, R::new(cols[5] - 60.0, y, cols[5] + 20.0, y + row_h), t.text, DWRITE_TEXT_ALIGNMENT_TRAILING);
            let ctx_col = if row.past { hex(RED, 1.0) } else { t.text2 };
            p.text(&if row.context > 0 { count(row.context) } else { "-".to_string() }, &p.g.small, R::new(cols[6] - 90.0, y, cols[6], y + row_h), ctx_col, DWRITE_TEXT_ALIGNMENT_TRAILING);
            if row.past {
                p.text("past 300k: hand off", &p.g.small, R::new(cols[6] + 12.0, y, r.r - 12.0, y + row_h), hex(RED, 1.0), DWRITE_TEXT_ALIGNMENT_LEADING);
            }
            y += row_h;
        }
    }

    fn paint_pods(&self, ink: &Ink, area: R) {
        let p = &ink.p;
        let t = theme(true);
        let n = self.pods.len().max(1);
        let pw = (area.r - area.l - GAP * (n as f32 - 1.0)) / n as f32;
        for (i, pod) in self.pods.iter().enumerate() {
            let r = R::new(area.l + i as f32 * (pw + GAP), area.t, area.l + i as f32 * (pw + GAP) + pw, area.b);
            let c = pod.color;
            let working = pod.open && pod.busy > 0.05;
            // A slow, smooth breath while it works; steady when open.
            let lit = if working { 0.62 + 0.38 * (ink.t * 2.4).sin() } else if pod.open { 0.45 } else { 0.0 };
            ink.gem(r, 16.0, c, lit);
            if pod.open {
                ink.halo(r.l + pw * 0.5, r.t + 6.0, pw * 0.55, c, 0.10 + 0.12 * lit);
            }
            // Name and light.
            let lx = r.l + 20.0;
            let ly = r.t + 22.0;
            if pod.open {
                ink.halo(lx, ly, 16.0 + 8.0 * lit, c, 0.45 + 0.4 * lit);
                p.circle(lx, ly, 4.5, mix(c, hex(0xFFFFFF, 1.0), 0.55));
            } else {
                p.circle(lx, ly, 4.5, hex(0x5A524C, 1.0));
            }
            let today = if pod.today.cost > 0.0 {
                money(pod.today.cost)
            } else if pod.today.tokens.total() > 0 {
                format!("{} tok", count(pod.today.tokens.total()))
            } else {
                String::new()
            };
            let tw = if today.is_empty() { 0.0 } else { p.width(&today, &p.g.bold) + 6.0 };
            p.text(&pod.name, &p.g.bold, R::new(lx + 14.0, ly - 11.0, r.r - 14.0 - tw, ly + 11.0), if pod.open { t.text } else { t.text2 }, DWRITE_TEXT_ALIGNMENT_LEADING);
            if !today.is_empty() {
                p.text(&today, &p.g.bold, R::new(r.r - 14.0 - tw, ly - 11.0, r.r - 14.0, ly + 11.0), mix(c, hex(0xFFFFFF, 1.0), 0.35), DWRITE_TEXT_ALIGNMENT_TRAILING);
                p.text("today", &p.g.tiny, R::new(r.r - 80.0, ly + 9.0, r.r - 14.0, ly + 25.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
            }
            let state = if !pod.open {
                "closed".to_string()
            } else if pod.codex {
                if working { "working".into() } else { "open".into() }
            } else {
                let chats = match pod.chats {
                    0 => "no chats loaded".to_string(),
                    1 => "1 chat loaded".to_string(),
                    n => format!("{n} chats loaded"),
                };
                if working { format!("working  \u{00B7}  {chats}") } else { chats }
            };
            p.text(&state, &p.g.tiny, R::new(lx + 14.0, ly + 9.0, r.r - 12.0, ly + 25.0), if working { alpha(c, 0.95) } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_LEADING);

            // Two rings: 5-hour and week.
            let rad = (pw / 4.0 - 14.0).clamp(16.0, 30.0);
            let ry = r.t + 62.0 + rad;
            for (k, (label, pct, fc)) in [("5H", pod.five, pod.five_fc), ("WEEK", pod.week, pod.week_fc)].iter().enumerate() {
                let rx = r.l + pw * (0.27 + 0.46 * k as f32);
                let col = match fc {
                    Forecast::Reached => hex(0xFF5A5A, 1.0),
                    Forecast::At(_) => hex(0xFFB020, 1.0),
                    _ => c,
                };
                match pct {
                    Some(v) => {
                        ink.ring(rx, ry, rad, 5.0, *v as f32 / 100.0 * ease(self.shown_at.elapsed().as_secs_f32() / 1.1), col);
                        p.text(&format!("{v}%"), &p.g.bold, R::new(rx - rad, ry - 11.0, rx + rad, ry + 7.0), t.text, DWRITE_TEXT_ALIGNMENT_CENTER);
                    }
                    None => {
                        ink.ring(rx, ry, rad, 5.0, 0.0, col);
                        p.text("-", &p.g.bold, R::new(rx - rad, ry - 11.0, rx + rad, ry + 7.0), t.text3, DWRITE_TEXT_ALIGNMENT_CENTER);
                    }
                }
                p.text(label, &p.g.tiny, R::new(rx - rad, ry + 6.0, rx + rad, ry + 20.0), t.text3, DWRITE_TEXT_ALIGNMENT_CENTER);
                let f = forecast_short(*fc);
                if !f.is_empty() {
                    p.text(&f, &p.g.tiny, R::new(rx - pw * 0.25, ry + rad + 4.0, rx + pw * 0.25, ry + rad + 20.0), if matches!(fc, Forecast::At(_) | Forecast::Reached) { col } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_CENTER);
                }
            }
            // The last 24 hours as an equalizer; this hour moves while it works.
            let eq = R::new(r.l + 16.0, r.b - 38.0, r.r - 16.0, r.b - 12.0);
            let max = pod.hours.iter().cloned().fold(0.0, f64::max);
            let bw = (eq.r - eq.l) / 24.0;
            for (k, v) in pod.hours.iter().enumerate() {
                let f = if max > 0.0 { (v / max) as f32 } else { 0.0 };
                let hgt = (f * (eq.b - eq.t)).max(1.5);
                let x = eq.l + bw * k as f32;
                let col = if f > 0.0 { c } else { hex(0x3A332E, 1.0) };
                let now = k == 23 && working;
                ink.bar(R::new(x + 1.0, eq.b - hgt, x + bw - 1.5, eq.b), alpha(col, 0.35 + 0.65 * (k as f32 / 23.0)), if now { lit } else { 0.0 });
                if now {
                    // This hour, while it works: a soft light under the bar, breathing with the pod.
                    ink.halo(x + bw / 2.0, eq.b, bw * 2.2, c, 0.25 * lit);
                }
            }
            let stale = as_stale(pod.as_of);
            if !pod.codex {
                let text = stats::past95_text(pod.past95, pod.last95, &short_when);
                let hot = pod.past95 > 0;
                let to = if stale { r.l + pw * 0.58 } else { r.r - 14.0 };
                p.text(&text, &p.g.tiny, R::new(r.l + 16.0, r.b - 56.0, to, r.b - 40.0), if hot { mix(c, hex(0xFFFFFF, 1.0), 0.35) } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_LEADING);
            }
            if stale {
                p.text(&format!("as of {}", short_when(pod.as_of)), &p.g.tiny, R::new(r.l + pw * 0.58, r.b - 56.0, r.r - 14.0, r.b - 40.0), hex(0xFFB020, 0.9), DWRITE_TEXT_ALIGNMENT_TRAILING);
            }
        }
    }

    fn paint_chart(&self, ink: &Ink, r: R, s: &Summary, grow: f32, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let m = self.metric;
        ink.gem(r, 16.0, hex(AMBER, 1.0), 0.25);
        p.text(if m == 0 { "COST BY DAY" } else { "TOKENS BY DAY" }, &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), hex(AMBER, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let mut lx = r.r - 18.0;
        for name in self.accounts.iter().rev().filter(|a| s.by_account.contains_key(*a)) {
            let wdt = p.width(name, &p.g.tiny);
            lx -= wdt;
            p.text(name, &p.g.tiny, R::new(lx, r.t + 12.0, lx + wdt + 2.0, r.t + 28.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            lx -= 12.0;
            let c = color_of(&self.accounts, name);
            ink.halo(lx + 4.0, r.t + 20.0, 9.0, c, 0.5);
            p.circle(lx + 4.0, r.t + 20.0, 3.0, c);
            lx -= 12.0;
        }
        let plot = R::new(r.l + 58.0, r.t + 40.0, r.r - 18.0, r.b - 28.0);
        let totals: Vec<f64> = self.days.iter().map(|d| s.by_day.get(d).map_or(0.0, |a| a.values().map(|x| value(m, x)).sum())).collect();
        let top = nice_ceiling(totals.iter().cloned().fold(0.0, f64::max));
        for k in 0..=3 {
            let v = top * k as f64 / 3.0;
            let y = plot.b - (plot.b - plot.t) * k as f32 / 3.0;
            p.line(plot.l, y, plot.r, y, hex(0xFFFFFF, if k == 0 { 0.14 } else { 0.05 }), 1.0);
            p.text(&show(m, v), &p.g.tiny, R::new(r.l + 6.0, y - 9.0, plot.l - 8.0, y + 9.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        }
        let n = self.days.len().max(1);
        let slot = (plot.r - plot.l) / n as f32;
        let bw = (slot * 0.66).clamp(1.0, 26.0);
        let label_every = (n as f32 / ((plot.r - plot.l) / 64.0)).ceil().max(1.0) as usize;
        for (i, day) in self.days.iter().enumerate() {
            let cx = plot.l + slot * (i as f32 + 0.5);
            // Bars rise left to right as they grow in.
            let g = ease(grow * 1.6 - i as f32 / n as f32 * 0.6);
            let mut y = plot.b;
            if let (Some(accts), true) = (s.by_day.get(day), top > 0.0) {
                let order: Vec<&String> = self.accounts.iter().filter(|a| accts.contains_key(*a)).collect();
                for (k, name) in order.iter().enumerate() {
                    let hgt = ((value(m, &accts[*name]) / top) as f32 * (plot.b - plot.t) * g).max(0.0);
                    if hgt > 0.0 {
                        let top_one = k + 1 == order.len();
                        ink.bar(R::new(cx - bw / 2.0, y - hgt, cx + bw / 2.0, y), color_of(&self.accounts, name), if top_one { 0.6 } else { 0.0 });
                        y -= hgt;
                    }
                }
            }
            if self.hover == Some(DHit::Bar(i)) {
                p.fill(R::new(cx - slot / 2.0, plot.t, cx + slot / 2.0, plot.b), 0.0, hex(0xFFFFFF, 0.05));
                ink.halo(cx, y, 22.0, hex(0xFFFFFF, 1.0), 0.25);
            }
            if i % label_every == 0 || i + 1 == n {
                p.text(&day_label(*day), &p.g.tiny, R::new(cx - 32.0, plot.b + 6.0, cx + 32.0, plot.b + 22.0), t.text3, DWRITE_TEXT_ALIGNMENT_CENTER);
            }
            hits.push((R::new(cx - slot / 2.0, plot.t, cx + slot / 2.0, plot.b), DHit::Bar(i)));
        }
        // The 7-day average, a glowing line over the bars.
        if n >= 7 && top > 0.0 && grow > 0.3 {
            let pts: Vec<(f32, f32)> = (0..n)
                .map(|i| {
                    let from = i.saturating_sub(6);
                    let avg = totals[from..=i].iter().sum::<f64>() / (i - from + 1) as f64;
                    (plot.l + slot * (i as f32 + 0.5), plot.b - (avg / top) as f32 * (plot.b - plot.t) * ease((grow - 0.3) / 0.7))
                })
                .collect();
            if let Some(g) = ink.geometry(&pts, false) {
                unsafe {
                    p.color(hex(0xFFE2B0, 0.18));
                    p.rt.DrawGeometry(&g, p.brush, 7.0, &p.g.round);
                    p.color(hex(0xFFE2B0, 0.9));
                    p.rt.DrawGeometry(&g, p.brush, 1.8, &p.g.round);
                }
            }
            if let Some(&(lx2, ly2)) = pts.last() {
                ink.halo(lx2, ly2, 14.0, hex(0xFFE2B0, 1.0), 0.6);
                p.text("7-day average", &p.g.tiny, R::new(lx2 - 120.0, ly2 - 22.0, lx2 - 6.0, ly2 - 6.0), hex(0xFFE2B0, 0.85), DWRITE_TEXT_ALIGNMENT_TRAILING);
            }
        }
        if let Some(DHit::Bar(i)) = self.hover {
            if let Some(day) = self.days.get(i) {
                let accts = s.by_day.get(day);
                let mut lines = vec![(day_label(*day), show(m, totals[i]), hex(AMBER, 1.0))];
                for name in &self.accounts {
                    if let Some(a) = accts.and_then(|x| x.get(name)) {
                        let v = if m == 0 && a.cost == 0.0 && a.tokens.total() > 0 { format!("{} tok", count(a.tokens.total())) } else { show(m, value(m, a)) };
                        lines.push((name.clone(), v, color_of(&self.accounts, name)));
                    }
                }
                let tw = 230.0;
                let th = 14.0 + lines.len() as f32 * 20.0;
                let cx = plot.l + slot * (i as f32 + 0.5);
                let tx = if cx + 16.0 + tw < r.r { cx + 16.0 } else { cx - 16.0 - tw };
                let tr = R::new(tx, plot.t, tx + tw, plot.t + th);
                ink.gem(tr, 10.0, hex(AMBER, 1.0), 0.8);
                for (k, (a, v, c)) in lines.iter().enumerate() {
                    let ly = tr.t + 7.0 + k as f32 * 20.0;
                    let font = if k == 0 { &p.g.bold } else { &p.g.small };
                    if k > 0 {
                        ink.halo(tr.l + 16.0, ly + 10.0, 8.0, *c, 0.5);
                        p.circle(tr.l + 16.0, ly + 10.0, 3.5, *c);
                    }
                    p.text(a, font, R::new(tr.l + if k == 0 { 12.0 } else { 28.0 }, ly, tr.r - 80.0, ly + 20.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
                    p.text(v, font, R::new(tr.r - 100.0, ly, tr.r - 12.0, ly + 20.0), t.text, DWRITE_TEXT_ALIGNMENT_TRAILING);
                }
            }
        }
    }

    /// When the work happens: weekday by hour, brighter where more was used.
    fn paint_heat(&self, ink: &Ink, r: R, s: &Summary, grow: f32) {
        let p = &ink.p;
        let t = theme(true);
        let m = self.metric;
        ink.gem(r, 16.0, hex(0xFF5FA2, 1.0), 0.25);
        p.text("WHEN YOU WORK", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), hex(0xFF5FA2, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let grid = R::new(r.l + 48.0, r.t + 42.0, r.r - 16.0, r.b - 26.0);
        let cw = (grid.r - grid.l) / 24.0;
        let chh = (grid.b - grid.t) / 7.0;
        let max = s.by_hour.iter().flat_map(|d| d.iter()).map(|a| value(m, a)).fold(0.0, f64::max);
        let cool = hex(0x3A1A4A, 1.0);
        let hot = hex(0xF57A08, 1.0);
        let white = hex(0xFFD98A, 1.0);
        for (d, row) in s.by_hour.iter().enumerate() {
            let y = grid.t + chh * d as f32;
            p.text(WEEKDAYS[d], &p.g.tiny, R::new(r.l + 12.0, y, grid.l - 6.0, y + chh), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            for (hr, a) in row.iter().enumerate() {
                let f = if max > 0.0 { (value(m, a) / max) as f32 } else { 0.0 };
                let x = grid.l + cw * hr as f32;
                let cell = R::new(x + 1.5, y + 1.5, x + cw - 1.5, y + chh - 1.5);
                if f <= 0.0 {
                    p.fill(cell, 2.0, hex(0xFFFFFF, 0.03));
                    continue;
                }
                let k = f.sqrt() * grow;
                let c = if k < 0.75 { mix(cool, hot, k / 0.75) } else { mix(hot, white, (k - 0.75) / 0.25) };
                p.fill(cell, 2.0, alpha(c, 0.25 + 0.75 * k));
                if k > 0.7 {
                    let tw = 0.6 + 0.4 * (ink.t * 1.7 + (d * 24 + hr) as f32).sin().abs();
                    ink.halo((cell.l + cell.r) / 2.0, (cell.t + cell.b) / 2.0, cw * 1.3, hot, 0.22 * tw * k);
                }
            }
        }
        for hr in [0, 6, 12, 18] {
            let x = grid.l + cw * hr as f32;
            let label = match hr {
                0 => "12a",
                6 => "6a",
                12 => "12p",
                _ => "6p",
            };
            p.text(label, &p.g.tiny, R::new(x - 4.0, grid.b + 4.0, x + 40.0, grid.b + 20.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
    }

    fn paint_ranked(&self, ink: &Ink, r: R, title: &str, map: &std::collections::BTreeMap<String, Agg>, colored: bool, c: C, grow: f32) {
        let p = &ink.p;
        let t = theme(true);
        ink.gem(r, 14.0, c, 0.2);
        p.text(title, &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let m = self.metric;
        // By cost, tokens decide among the unpriced (Codex).
        let mut rows: Vec<(&String, f64, f64)> = map.iter().map(|(k, a)| (k, value(m, a), a.tokens.total() as f64)).filter(|(_, v, tk)| *v > 0.0 || *tk > 0.0).collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.total_cmp(&a.2)));
        let total: f64 = rows.iter().map(|x| x.1).sum();
        let max = rows.first().map_or(1.0, |x| x.1).max(f64::MIN_POSITIVE);
        let fit = (((r.b - r.t - 40.0) / 30.0).floor() as usize).max(1);
        let shown = if rows.len() > fit { fit - 1 } else { rows.len() };
        let mut y = r.t + 36.0;
        for (k, (name, v, tk)) in rows.iter().take(shown).enumerate() {
            let col = if colored { color_of(&self.accounts, name) } else { mix(c, hex(0xFFFFFF, 1.0), 0.15 * (k as f32 / 4.0).min(1.0)) };
            let pct = if total > 0.0 { 100.0 * v / total } else { 0.0 };
            p.text(&format!("{}", k + 1), &p.g.tiny, R::new(r.l + 16.0, y, r.l + 30.0, y + 16.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(name, &p.g.small, R::new(r.l + 32.0, y, r.r - 130.0, y + 16.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            let shown_v = if *v > 0.0 { format!("{}  {pct:.0}%", show(m, *v)) } else { format!("{} tokens", count(*tk as u64)) };
            p.text(&shown_v, &p.g.small, R::new(r.r - 140.0, y, r.r - 16.0, y + 16.0), t.text2, DWRITE_TEXT_ALIGNMENT_TRAILING);
            let bar = R::new(r.l + 32.0, y + 19.0, r.r - 16.0, y + 23.0);
            p.fill(bar, 2.0, hex(0xFFFFFF, 0.06));
            let f = if *v > 0.0 { (*v / max) as f32 } else { 0.0 } * ease(grow * 1.4 - k as f32 * 0.08);
            if f > 0.0 {
                let end = bar.l + (bar.r - bar.l) * f;
                p.fill(R::new(bar.l, bar.t, end, bar.b), 2.0, col);
                ink.halo(end, (bar.t + bar.b) / 2.0, 10.0, col, 0.55);
            }
            y += 30.0;
        }
        if rows.len() > shown {
            let rest: f64 = rows.iter().skip(shown).map(|x| x.1).sum();
            p.text(&format!("{} more  \u{00B7}  {}", rows.len() - shown, show(m, rest)), &p.g.tiny, R::new(r.l + 32.0, y, r.r - 16.0, y + 16.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
        if rows.is_empty() {
            p.text("Nothing in this range.", &p.g.small, R::new(r.l + 18.0, y, r.r - 16.0, y + 16.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
    }
}

// ---- the memory view ---------------------------------------------------------------

const TEAL: u32 = 0x2EE6D6;
const GREEN: u32 = 0x5BE38A;
const RED: u32 = 0xFF5A5A;
const GOLD: u32 = 0xFFB020;

impl Dash {
    fn paint_memory(&self, ink: &Ink, area: R, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let Some(m) = &self.mem else {
            p.text("Reading the memory vault.", &p.g.body, R::new(area.l, area.t + 10.0, area.r, area.t + 40.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        };
        let grow = ease(self.mem_shown.elapsed().as_secs_f32() / 0.9);
        let (starts, avg_tok) = start_tokens(m);
        let harvest_old = m.harvest_at == 0 || crate::time::now().saturating_sub(m.harvest_at) > 2 * 3600;
        let lines_today = m.days.last().map_or(0, |d| d.1);
        let mb = |b: u64| format!("{:.1} MB", b as f64 / 1_048_576.0);
        let g = grow as f64;
        let tiles = [
            ("NOTES", count((m.notes as f64 * g) as u64), format!("+{} this week", m.notes_week), hex(TEAL, 1.0)),
            ("START OF A CHAT", format!("{} tok", count((avg_tok as f64 * g) as u64)), format!("{} chats today", count(starts)), hex(AMBER, 1.0)),
            ("VAULT", mb((m.vault_bytes as f64 * g) as u64), format!("of {} MB  \u{00B7}  git {}", m.cap_bytes >> 20, mb(m.git_bytes)), hex(0x4FB3FF, 1.0)),
            ("LAST HARVEST", ago(m.harvest_at), format!("{lines_today} lines today"), hex(if harvest_old { GOLD } else { GREEN }, 1.0)),
        ];
        let th = 76.0;
        let tw = (area.r - area.l - 3.0 * GAP) / 4.0;
        for (i, (label, big, sub, c)) in tiles.iter().enumerate() {
            let r = R::new(area.l + i as f32 * (tw + GAP), area.t, area.l + i as f32 * (tw + GAP) + tw, area.t + th);
            ink.gem(r, 12.0, *c, 0.4);
            ink.halo(r.l + 18.0, r.t + 18.0, 26.0, *c, 0.35);
            p.circle(r.l + 18.0, r.t + 18.0, 3.0, mix(*c, hex(0xFFFFFF, 1.0), 0.5));
            p.text(label, &p.g.tiny, R::new(r.l + 30.0, r.t + 9.0, r.r - 12.0, r.t + 27.0), alpha(*c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
            if let Some(f) = &self.big {
                p.text(big, f, R::new(r.l + 16.0, r.t + 26.0, r.r - 100.0, r.t + 58.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            }
            p.text(sub, &p.g.small, R::new(r.r - 180.0, r.t + 32.0, r.r - 14.0, r.t + 54.0), t.text2, DWRITE_TEXT_ALIGNMENT_TRAILING);
        }
        let cy = area.t + th + GAP;
        let rest = area.b - cy;
        let ch = (rest * 0.56).max(240.0);
        let hw = 430.0;
        self.paint_saved(ink, R::new(area.l, cy, area.r - hw - GAP, cy + ch), m, grow);
        self.paint_graph(ink, R::new(area.r - hw, cy, area.r, cy + ch), m, grow, hits);
        let ly = cy + ch + GAP;
        let cw = (area.r - area.l - 2.0 * GAP) / 3.0;
        let col = |i: usize| R::new(area.l + i as f32 * (cw + GAP), ly, area.l + i as f32 * (cw + GAP) + cw, area.b);
        self.paint_mem_projects(ink, col(0), m, grow);
        self.paint_hooks(ink, col(1), m, grow);
        self.paint_problems(ink, col(2), m);
    }

    /// What the harvest and the chats saved, day by day.
    fn paint_saved(&self, ink: &Ink, r: R, m: &crate::memory::Health, grow: f32) {
        let p = &ink.p;
        let t = theme(true);
        let (amber, teal) = (hex(AMBER, 1.0), hex(TEAL, 1.0));
        ink.gem(r, 16.0, amber, 0.25);
        p.text("SAVED BY DAY", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(amber, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let mut lx = r.r - 18.0;
        for (name, c) in [("notes saved or edited", teal), ("digest lines (harvest)", amber)] {
            let wdt = p.width(name, &p.g.tiny);
            lx -= wdt;
            p.text(name, &p.g.tiny, R::new(lx, r.t + 12.0, lx + wdt + 2.0, r.t + 28.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            lx -= 12.0;
            ink.halo(lx + 4.0, r.t + 20.0, 9.0, c, 0.5);
            p.circle(lx + 4.0, r.t + 20.0, 3.0, c);
            lx -= 14.0;
        }
        let plot = R::new(r.l + 50.0, r.t + 40.0, r.r - 18.0, r.b - 28.0);
        let top = nice_ceiling(m.days.iter().map(|d| (d.1 + d.2) as f64).fold(0.0, f64::max));
        for k in 0..=3 {
            let v = top * k as f64 / 3.0;
            let y = plot.b - (plot.b - plot.t) * k as f32 / 3.0;
            p.line(plot.l, y, plot.r, y, hex(0xFFFFFF, if k == 0 { 0.14 } else { 0.05 }), 1.0);
            p.text(&count(v as u64), &p.g.tiny, R::new(r.l + 6.0, y - 9.0, plot.l - 8.0, y + 9.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        }
        let n = m.days.len().max(1);
        let slot = (plot.r - plot.l) / n as f32;
        let bw = (slot * 0.66).clamp(1.0, 26.0);
        for (i, (day, lines, notes)) in m.days.iter().enumerate() {
            let cx = plot.l + slot * (i as f32 + 0.5);
            let gi = ease(grow * 1.6 - i as f32 / n as f32 * 0.6);
            let scale = (plot.b - plot.t) / top as f32 * gi;
            let h1 = *lines as f32 * scale;
            let h2 = *notes as f32 * scale;
            ink.bar(R::new(cx - bw / 2.0, plot.b - h1, cx + bw / 2.0, plot.b), amber, if *notes == 0 && i + 1 == n { 0.6 } else { 0.0 });
            ink.bar(R::new(cx - bw / 2.0, plot.b - h1 - h2, cx + bw / 2.0, plot.b - h1), teal, if i + 1 == n { 0.6 } else { 0.0 });
            if i % 5 == 4 || i + 1 == n {
                let label = day.get(5..).unwrap_or(day).replace('-', "/");
                p.text(&label, &p.g.tiny, R::new(cx - 32.0, plot.b + 6.0, cx + 32.0, plot.b + 22.0), t.text3, DWRITE_TEXT_ALIGNMENT_CENTER);
            }
        }
    }

    /// Every project around the shared notes: size is how many notes, brightness how
    /// recently one changed.
    fn paint_graph(&self, ink: &Ink, r: R, m: &crate::memory::Health, grow: f32, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let violet = hex(0xB58CFF, 1.0);
        ink.gem(r, 16.0, violet, 0.25);
        p.text("MEMORY GRAPH", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(violet, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        p.text("bigger: more notes  \u{00B7}  brighter: newer", &p.g.tiny, R::new(r.l + 120.0, r.t + 12.0, r.r - 18.0, r.t + 28.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        let (cx, cy) = ((r.l + r.r) / 2.0, (r.t + 36.0 + r.b - 10.0) / 2.0);
        let maxr = ((r.r - r.l) / 2.0 - 46.0).min((r.b - r.t - 46.0) / 2.0 - 14.0).max(40.0);
        let n = m.projects.len().max(1);
        let now = crate::time::now();
        let amber = hex(AMBER, 1.0);
        let dim = hex(0x5A4A66, 1.0);
        let biggest = m.projects.first().map_or(1, |x| x.notes).max(1) as f32;
        let nodes: Vec<(f32, f32, f32, C, f32)> = m
            .projects
            .iter()
            .enumerate()
            .map(|(i, ph)| {
                let a = i as f32 * 2.399_963 + 0.6;
                let d = maxr * (0.42 + 0.58 * ((i as f32 + 0.5) / n as f32).sqrt()) * ease(grow * 1.3 - i as f32 / n as f32 * 0.3);
                let drift = 2.5 * (ink.t * 0.4 + i as f32 * 1.7).sin();
                let (x, y) = (cx + (d + drift) * a.cos(), cy + (d + drift) * a.sin() * 0.92);
                let rad = 4.0 + 13.0 * (ph.notes as f32 / biggest).sqrt();
                let age = now.saturating_sub(ph.newest) as f32 / 86_400.0;
                let fresh = (1.0 - age / 30.0).clamp(0.0, 1.0);
                (x, y, rad, mix(dim, amber, fresh), fresh)
            })
            .collect();
        for (x, y, _, c, fresh) in &nodes {
            p.line(cx, cy, *x, *y, alpha(*c, 0.10 + 0.25 * fresh), 1.0);
        }
        // The shared notes in the middle: every chat in every project gets them.
        ink.halo(cx, cy, 44.0, violet, 0.35);
        p.circle(cx, cy, 15.0, mix(violet, hex(0xFFFFFF, 1.0), 0.15));
        p.text(&format!("shared {}", m.shared), &p.g.tiny, R::new(cx - 50.0, cy + 17.0, cx + 50.0, cy + 33.0), t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
        for (i, ((x, y, rad, c, fresh), ph)) in nodes.iter().zip(&m.projects).enumerate() {
            let hover = self.hover == Some(DHit::Node(i));
            if *fresh > 0.9 {
                let pulse = 0.6 + 0.4 * (ink.t * 1.8 + i as f32).sin();
                ink.halo(*x, *y, rad * 2.6, *c, 0.35 * pulse);
            }
            p.circle(*x, *y, *rad + if hover { 2.0 } else { 0.0 }, alpha(*c, 0.55 + 0.45 * fresh));
            p.circle(*x - rad * 0.3, *y - rad * 0.3, rad * 0.35, alpha(hex(0xFFFFFF, 1.0), 0.18));
            if i < 6 || hover {
                let right = *x >= cx;
                let lr = if right { R::new(x + rad + 4.0, y - 8.0, x + rad + 150.0, y + 8.0) } else { R::new(x - rad - 150.0, y - 8.0, x - rad - 4.0, y + 8.0) };
                p.text(&ph.name, &p.g.tiny, lr, if hover { t.text } else { t.text2 }, if right { DWRITE_TEXT_ALIGNMENT_LEADING } else { DWRITE_TEXT_ALIGNMENT_TRAILING });
            }
            hits.push((R::new(x - rad - 3.0, y - rad - 3.0, x + rad + 3.0, y + rad + 3.0), DHit::Node(i)));
        }
        if let Some(DHit::Node(i)) = self.hover {
            if let (Some(ph), Some((x, y, rad, c, _))) = (m.projects.get(i), nodes.get(i)) {
                let lines = [
                    (ph.name.clone(), String::new()),
                    ("notes".into(), ph.notes.to_string()),
                    ("over 90 days old".into(), ph.stale.to_string()),
                    ("start of a chat".into(), format!("{} tok", count(ph.start_bytes as u64 / 4))),
                    ("last note change".into(), ago(ph.newest)),
                    ("digest".into(), format!("{} KB", ph.digest_bytes / 1024)),
                ];
                let (tw, th) = (230.0, 14.0 + lines.len() as f32 * 20.0);
                let tx = if x + rad + 12.0 + tw < r.r { x + rad + 12.0 } else { x - rad - 12.0 - tw };
                let ty = (y - th / 2.0).clamp(r.t + 6.0, r.b - th - 6.0);
                let tr = R::new(tx, ty, tx + tw, ty + th);
                ink.gem(tr, 10.0, *c, 0.8);
                for (k, (a, v)) in lines.iter().enumerate() {
                    let ly = tr.t + 7.0 + k as f32 * 20.0;
                    let font = if k == 0 { &p.g.bold } else { &p.g.small };
                    p.text(a, font, R::new(tr.l + 12.0, ly, tr.r - 70.0, ly + 20.0), if k == 0 { t.text } else { t.text2 }, DWRITE_TEXT_ALIGNMENT_LEADING);
                    p.text(v, font, R::new(tr.r - 110.0, ly, tr.r - 12.0, ly + 20.0), t.text, DWRITE_TEXT_ALIGNMENT_TRAILING);
                }
            }
        }
    }

    fn paint_mem_projects(&self, ink: &Ink, r: R, m: &crate::memory::Health, grow: f32) {
        let p = &ink.p;
        let t = theme(true);
        let c = hex(TEAL, 1.0);
        ink.gem(r, 14.0, c, 0.2);
        p.text("PROJECTS", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        p.text("notes  \u{00B7}  start of a chat", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        let max = m.projects.first().map_or(1, |x| x.notes).max(1) as f32;
        let fit = (((r.b - r.t - 40.0) / 30.0).floor() as usize).max(1);
        let shown = if m.projects.len() > fit { fit - 1 } else { m.projects.len() };
        let mut y = r.t + 36.0;
        for (k, ph) in m.projects.iter().take(shown).enumerate() {
            p.text(&ph.name, &p.g.small, R::new(r.l + 18.0, y, r.r - 130.0, y + 16.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            let v = format!("{}  \u{00B7}  {} tok", ph.notes, count(ph.start_bytes as u64 / 4));
            p.text(&v, &p.g.small, R::new(r.r - 150.0, y, r.r - 16.0, y + 16.0), t.text2, DWRITE_TEXT_ALIGNMENT_TRAILING);
            let bar = R::new(r.l + 18.0, y + 19.0, r.r - 16.0, y + 23.0);
            p.fill(bar, 2.0, hex(0xFFFFFF, 0.06));
            let f = ph.notes as f32 / max * ease(grow * 1.4 - k as f32 * 0.08);
            if f > 0.0 {
                let end = bar.l + (bar.r - bar.l) * f;
                // The part over 90 days old, dimmer.
                let old = if ph.notes > 0 { ph.stale as f32 / ph.notes as f32 } else { 0.0 };
                let split = end - (end - bar.l) * old;
                p.fill(R::new(bar.l, bar.t, split, bar.b), 2.0, c);
                if split < end {
                    p.fill(R::new(split, bar.t, end, bar.b), 2.0, hex(0x6A6460, 1.0));
                }
                ink.halo(split, (bar.t + bar.b) / 2.0, 10.0, c, 0.5);
            }
            y += 30.0;
        }
        if m.projects.len() > shown {
            let rest: usize = m.projects.iter().skip(shown).map(|x| x.notes).sum();
            p.text(&format!("{} more  \u{00B7}  {rest} notes", m.projects.len() - shown), &p.g.tiny, R::new(r.l + 18.0, y, r.r - 16.0, y + 16.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
    }

    /// Whether each moving part ran lately, and the chats given memory each day.
    fn paint_hooks(&self, ink: &Ink, r: R, m: &crate::memory::Health, grow: f32) {
        let p = &ink.p;
        let t = theme(true);
        let c = hex(GREEN, 1.0);
        ink.gem(r, 14.0, c, 0.2);
        p.text("RUNNING", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let now = crate::time::now();
        let last = |k: &str| m.hooks.last_start.get(k).copied().unwrap_or(0);
        let rows: [(&str, u64, u64); 5] = [
            ("Claude: start of chat", last("claude"), 86_400),
            ("Codex: start of chat", last("codex"), 3 * 86_400),
            ("Long-chat reminder hook", m.hooks.last_prompt, 86_400),
            ("Harvest, every 30 min", m.harvest_at, 2 * 3600),
            ("Git history, daily", m.commit_at, 2 * 86_400),
        ];
        let mut y = r.t + 38.0;
        for (label, at, ok) in rows {
            let age = now.saturating_sub(at);
            let col = if at == 0 { hex(0x5A524C, 1.0) } else if age <= ok { c } else if age <= ok * 3 { hex(GOLD, 1.0) } else { hex(RED, 1.0) };
            if at > 0 && age <= ok {
                ink.halo(r.l + 24.0, y + 9.0, 10.0, col, 0.5);
            }
            p.circle(r.l + 24.0, y + 9.0, 4.0, col);
            p.text(label, &p.g.small, R::new(r.l + 38.0, y, r.r - 110.0, y + 18.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&ago(at), &p.g.small, R::new(r.r - 120.0, y, r.r - 16.0, y + 18.0), if at > 0 && age <= ok { t.text2 } else { col }, DWRITE_TEXT_ALIGNMENT_TRAILING);
            y += 26.0;
        }
        // Chats given memory, the last 14 days (Claude and Codex stacked).
        let eq = R::new(r.l + 18.0, (y + 26.0).min(r.b - 30.0), r.r - 16.0, r.b - 22.0);
        if eq.b - eq.t < 14.0 {
            return;
        }
        let days: Vec<(u64, u64)> = (0..14u64)
            .rev()
            .map(|i| {
                let d = crate::memory::day_key(now.saturating_sub(i * 86_400));
                m.hooks.days.get(&d).map_or((0, 0), |x| (x.get("claude").map_or(0, |v| v.0), x.get("codex").map_or(0, |v| v.0)))
            })
            .collect();
        p.text(&format!("chats given memory, 14 days  \u{00B7}  {} reminders", m.hooks.reminders), &p.g.tiny, R::new(r.l + 18.0, eq.t - 18.0, r.r - 16.0, eq.t - 2.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        let max = days.iter().map(|d| d.0 + d.1).max().unwrap_or(0).max(1) as f32;
        let bw = (eq.r - eq.l) / 14.0;
        for (k, (cl, cx)) in days.iter().enumerate() {
            let x = eq.l + bw * k as f32;
            let s = (eq.b - eq.t) / max * ease(grow * 1.5 - k as f32 * 0.04);
            let (h1, h2) = (*cl as f32 * s, *cx as f32 * s);
            if cl + cx == 0 {
                p.fill(R::new(x + 1.0, eq.b - 1.5, x + bw - 1.5, eq.b), 0.0, hex(0x3A332E, 1.0));
            }
            ink.bar(R::new(x + 1.0, eq.b - h1, x + bw - 1.5, eq.b), hex(AMBER, 1.0), 0.0);
            ink.bar(R::new(x + 1.0, eq.b - h1 - h2, x + bw - 1.5, eq.b - h1), hex(0x4FB3FF, 1.0), 0.0);
        }
    }

    fn paint_problems(&self, ink: &Ink, r: R, m: &crate::memory::Health) {
        let p = &ink.p;
        let t = theme(true);
        let bad = !m.problems.is_empty();
        let c = hex(if bad { GOLD } else { GREEN }, 1.0);
        ink.gem(r, 14.0, c, 0.2);
        p.text("NEEDS A LOOK", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(c, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        if !bad {
            p.text("All clear.", &p.g.body, R::new(r.l + 18.0, r.t + 40.0, r.r - 16.0, r.t + 66.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        }
        let mut y = r.t + 38.0;
        for line in &m.problems {
            if y + 34.0 > r.b - 8.0 {
                break;
            }
            p.circle(r.l + 24.0, y + 8.0, 3.0, c);
            p.text(line, &p.g.small, R::new(r.l + 36.0, y, r.r - 16.0, y + 36.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            y += 40.0;
        }
    }
}

/// Chats given memory today, and the tokens one gets at its start (today's average, or
/// the projects' average before any chat started today).
fn start_tokens(m: &crate::memory::Health) -> (u64, u64) {
    let today = m.days.last().map(|d| d.0.clone()).unwrap_or_default();
    let (starts, bytes) = m.hooks.days.get(&today).map_or((0, 0), |d| d.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)));
    let avg = if starts > 0 {
        bytes / starts / 4
    } else {
        let n = m.projects.len().max(1);
        (m.projects.iter().map(|x| x.start_bytes).sum::<usize>() / n / 4) as u64
    };
    (starts, avg)
}

// ---- the note browser --------------------------------------------------------------

const FOLDER_ROW: f32 = 30.0;
const NOTE_ROW: f32 = 52.0;

impl Dash {
    /// The folder list: every note, the old ones, shared, then projects by size.
    /// (pick, label, notes, of them over 90 days old)
    fn folder_rows(&self) -> Vec<(Pick, String, usize, usize)> {
        let Some(m) = &self.mem else { return Vec::new() };
        let old = m.list.iter().filter(|n| n.stale).count();
        let mut rows = vec![(Pick::All, "All notes".to_string(), m.list.len(), old), (Pick::Old, "Over 90 days old".to_string(), old, old)];
        let mut by: std::collections::BTreeMap<&str, (usize, usize)> = std::collections::BTreeMap::new();
        for n in &m.list {
            let e = by.entry(n.folder.as_str()).or_default();
            e.0 += 1;
            e.1 += n.stale as usize;
        }
        let mut rest: Vec<(&str, (usize, usize))> = by.into_iter().collect();
        rest.sort_by(|a, b| (b.0 == "shared").cmp(&(a.0 == "shared")).then(b.1 .0.cmp(&a.1 .0)).then(a.0.cmp(b.0)));
        rows.extend(rest.into_iter().map(|(f, (n, o))| (Pick::Folder(f.to_string()), f.to_string(), n, o)));
        rows
    }

    /// The chosen note's place in the list.
    fn sel_index(&self) -> Option<usize> {
        let (m, s) = (self.mem.as_ref()?, self.sel.as_ref()?);
        self.found.iter().position(|&i| &m.list[i].path == s)
    }

    /// The notes that match the folder and the search, again. `reset`: back to the top.
    fn refilter(&mut self, reset: bool) {
        let words: Vec<String> = self.query.to_lowercase().split_whitespace().map(str::to_string).collect();
        let found: Vec<usize> = match &self.mem {
            None => Vec::new(),
            Some(m) => {
                let mut f: Vec<usize> = m
                    .list
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| match &self.pick {
                        Pick::All => true,
                        Pick::Old => n.stale,
                        Pick::Folder(x) => &n.folder == x,
                    })
                    .filter(|(_, n)| words.iter().all(|w| n.text.contains(w.as_str()) || n.name.to_lowercase().contains(w.as_str()) || n.folder.contains(w.as_str())))
                    .map(|(i, _)| i)
                    .collect();
                if self.pick == Pick::Old {
                    f.sort_by(|a, b| m.list[*a].updated.cmp(&m.list[*b].updated));
                }
                f
            }
        };
        self.found = found;
        if self.sel_index().is_none() {
            self.sel = self.found.first().and_then(|&i| self.mem.as_ref().map(|m| m.list[i].path.clone()));
        }
        if reset {
            self.list_top = 0;
        }
        self.list_top = self.list_top.min(self.found.len().saturating_sub(1));
        self.load_body(false);
    }

    /// Read the chosen note's text (again when `fresh`: the vault was read again).
    fn load_body(&mut self, fresh: bool) {
        match &self.sel {
            None => self.body = None,
            Some(p) => {
                let same = self.body.as_ref().is_some_and(|b| &b.0 == p);
                if !same || fresh {
                    self.body = Some((p.clone(), crate::memory::note_body(p)));
                }
                if !same {
                    self.body_top = 0.0;
                    self.confirm = None;
                }
            }
        }
    }

    /// Choose the note at `k` in the list, and keep it in view.
    fn select(&mut self, k: usize) {
        let Some(&i) = self.found.get(k) else { return };
        self.sel = self.mem.as_ref().map(|m| m.list[i].path.clone());
        self.load_body(false);
        let fit = self.fits.get().1.max(1);
        if k < self.list_top {
            self.list_top = k;
        } else if k >= self.list_top + fit {
            self.list_top = k + 1 - fit;
        }
    }

    fn move_sel(&mut self, by: i32) {
        if self.found.is_empty() {
            return;
        }
        let k = self.sel_index().map_or(0, |k| (k as i32 + by).clamp(0, self.found.len() as i32 - 1) as usize);
        self.select(k);
    }

    fn choose(&mut self, pick: Pick) {
        self.pick = pick;
        self.query.clear();
        self.sel = None;
        self.refilter(true);
    }

    fn act(&mut self, a: Act) {
        let Some(p) = self.sel.clone() else { return };
        match a {
            Act::Open => crate::win::shell_open(&p.to_string_lossy(), None),
            Act::Reveal => crate::win::shell_open("explorer.exe", Some(&format!("/select,\"{}\"", p.display()))),
            Act::Skip => self.move_sel(1),
            Act::Keep | Act::Delete => {
                // The second click must come a moment later: a double-click is not two answers.
                if a == Act::Delete && !self.confirm.as_ref().is_some_and(|(c, at)| c == &p && (400..5000).contains(&at.elapsed().as_millis())) {
                    self.confirm = Some((p, Instant::now()));
                    return;
                }
                self.confirm = None;
                // The note leaves the list (deleted, or kept from the old-notes list):
                // the next one gets the choice.
                if a == Act::Delete || self.pick == Pick::Old {
                    if let Some(k) = self.sel_index() {
                        let next = self.found.get(k + 1).or_else(|| k.checked_sub(1).and_then(|j| self.found.get(j))).copied();
                        self.sel = next.and_then(|i| self.mem.as_ref().map(|m| m.list[i].path.clone()));
                        self.load_body(false);
                    }
                }
                let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                std::thread::spawn(move || {
                    let done = if a == Act::Keep {
                        crate::memory::mark_checked(&p).map(|_| format!("Kept {name}: marked as checked today."))
                    } else {
                        crate::memory::delete_note(&p).map(|rel| format!("Deleted {rel}. Git history keeps a copy."))
                    };
                    crate::log(&format!("dashboard: {}", done.as_ref().unwrap_or_else(|e| e)));
                    if let Ok(mut t) = TOAST.lock() {
                        *t = Some(match done {
                            Ok(s) => (s, true),
                            Err(e) => (e, false),
                        });
                    }
                    // Read the vault now (not through load_mem, which skips while a
                    // minute's read is under way), so the list shows the change.
                    let h = crate::memory::health();
                    if let Ok(mut m) = MEM.lock() {
                        *m = Some(h);
                    }
                    unsafe { PostMessageW(dash_hwnd(), WM_DASH_MEM, 0, 0) };
                });
            }
        }
    }

    /// A key while the note browser shows; false: not used here.
    fn key(&mut self, k: u16) -> bool {
        if self.view != 1 || self.mem_page != 0 {
            return false;
        }
        match k {
            VK_ESCAPE if !self.query.is_empty() => {
                self.query.clear();
                self.refilter(true);
            }
            VK_ESCAPE if self.confirm.is_some() => self.confirm = None,
            VK_BACK if !self.query.is_empty() => {
                if key_down(VK_CONTROL) {
                    self.query.clear();
                } else {
                    self.query.pop();
                }
                self.refilter(true);
            }
            VK_UP => self.move_sel(-1),
            VK_DOWN => self.move_sel(1),
            VK_RETURN => self.act(Act::Open),
            _ => return false,
        }
        true
    }

    /// A typed letter goes into the search.
    fn typed(&mut self, ch: char) {
        if self.view != 1 || self.mem_page != 0 || self.query.chars().count() >= 80 {
            return;
        }
        if self.query.is_empty() && ch == ' ' {
            return;
        }
        if self.query.is_empty() {
            self.pick = Pick::All;
        }
        self.query.push(ch);
        self.refilter(true);
    }

    /// The mouse wheel over one of the browser's panes.
    fn scroll_at(&mut self, x: f32, y: f32, notches: f32) {
        let pane = self.hits.iter().find_map(|(r, h)| match h {
            DHit::Pane(k) if r.contains(x, y) => Some(*k),
            _ => None,
        });
        let rows = (-notches * 3.0).round() as i32;
        let (ffit, lfit) = self.fits.get();
        match pane {
            Some(0) => {
                let max = self.folder_rows().len().saturating_sub(ffit) as i32;
                self.folder_top = (self.folder_top as i32 + rows).clamp(0, max) as usize;
            }
            Some(1) => {
                let max = self.found.len().saturating_sub(lfit) as i32;
                self.list_top = (self.list_top as i32 + rows).clamp(0, max) as usize;
            }
            Some(2) => self.body_top = (self.body_top - notches * 90.0).clamp(0.0, self.body_max.get()),
            _ => {}
        }
    }

    fn paint_notes(&self, ink: &Ink, area: R, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let Some(m) = &self.mem else {
            p.text("Reading the memory vault.", &p.g.body, R::new(area.l, area.t + 10.0, area.r, area.t + 40.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        };
        // One line of health; the old notes have their own button.
        let strip = R::new(area.l, area.t, area.r, area.t + 46.0);
        let issues: Vec<&String> = m.problems.iter().filter(|s| !s.contains("days old")).collect();
        let c = hex(if issues.is_empty() { GREEN } else { GOLD }, 1.0);
        ink.gem(strip, 10.0, c, 0.2);
        ink.halo(strip.l + 22.0, strip.t + 23.0, 12.0, c, 0.5);
        p.circle(strip.l + 22.0, strip.t + 23.0, 4.0, c);
        let old = m.list.iter().filter(|n| n.stale).count();
        let line = match issues.first() {
            None => format!(
                "Memory is working.   {} notes  \u{00B7}  about {} tokens at the start of a chat  \u{00B7}  vault {:.1} MB  \u{00B7}  harvest {}",
                m.list.len(),
                count(start_tokens(m).1),
                m.vault_bytes as f64 / 1_048_576.0,
                ago(m.harvest_at)
            ),
            Some(first) if issues.len() == 1 => format!("Needs a look: {first}"),
            Some(first) => format!("Needs a look: {first}  (and {} more on Stats)", issues.len() - 1),
        };
        let review = format!("Review {old} old notes");
        let bw = p.width(&review, &p.g.small) + 32.0;
        p.text(&line, &p.g.small, R::new(strip.l + 38.0, strip.t, strip.r - bw - 24.0, strip.b), if issues.is_empty() { t.text2 } else { t.text }, DWRITE_TEXT_ALIGNMENT_LEADING);
        if old > 0 && self.pick != Pick::Old {
            button(ink, hits, self.hover, R::new(strip.r - bw - 10.0, strip.t + 8.0, strip.r - 10.0, strip.b - 8.0), &review, DHit::Review, Some(hex(GOLD, 1.0)));
        }

        let top = strip.b + GAP;
        let lw = 250.0;
        let mw = ((area.r - area.l - lw - 2.0 * GAP) * 0.42).max(340.0);
        let search = R::new(area.l, top, area.l + lw + GAP + mw, top + 38.0);
        let left = R::new(area.l, search.b + 10.0, area.l + lw, area.b);
        let mid = R::new(left.r + GAP, search.b + 10.0, search.r, area.b);
        let right = R::new(mid.r + GAP, top, area.r, area.b);
        self.paint_search(ink, search, hits);
        self.paint_folders(ink, left, hits);
        self.paint_list(ink, mid, m, hits);
        self.paint_note(ink, right, m, hits);
    }

    fn paint_search(&self, ink: &Ink, r: R, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let amber = hex(AMBER, 1.0);
        p.fill(r, 8.0, hex(0x000000, 0.38));
        p.stroke(r, 8.0, alpha(amber, if self.query.is_empty() { 0.35 } else { 0.85 }), 1.0);
        p.text("\u{E721}", &p.g.icon, R::new(r.l + 8.0, r.t, r.l + 36.0, r.b), t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
        let tr = R::new(r.l + 42.0, r.t, r.r - 120.0, r.b);
        let caret_x = if self.query.is_empty() {
            p.text("Search every note: just start typing", &p.g.body, tr, t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            tr.l
        } else {
            p.text(&self.query, &p.g.body, tr, t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            let found = format!("{} found", self.found.len());
            p.text(&found, &p.g.small, R::new(r.r - 160.0, r.t, r.r - 42.0, r.b), t.text2, DWRITE_TEXT_ALIGNMENT_TRAILING);
            let cr = R::new(r.r - 36.0, r.t + 5.0, r.r - 6.0, r.b - 5.0);
            if self.hover == Some(DHit::ClearSearch) {
                p.fill(cr, 6.0, hex(0xFFFFFF, 0.10));
            }
            p.text("\u{E711}", &p.g.icon_small, cr, t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
            hits.push((cr, DHit::ClearSearch));
            (tr.l + p.width(&self.query, &p.g.body) + 1.0).min(tr.r)
        };
        if (ink.t * 1.8) as i32 % 2 == 0 {
            p.line(caret_x, r.t + 10.0, caret_x, r.b - 10.0, t.text, 1.2);
        }
    }

    fn paint_folders(&self, ink: &Ink, r: R, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let (teal, amber) = (hex(TEAL, 1.0), hex(AMBER, 1.0));
        ink.gem(r, 14.0, teal, 0.18);
        hits.push((r, DHit::Pane(0)));
        p.text("FOLDERS", &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 16.0, r.t + 28.0), alpha(teal, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        let rows = self.folder_rows();
        let body = R::new(r.l + 8.0, r.t + 36.0, r.r - 10.0, r.b - 10.0);
        let fit = ((body.b - body.t) / FOLDER_ROW).floor().max(1.0) as usize;
        self.fits.set((fit, self.fits.get().1));
        let top = self.folder_top.min(rows.len().saturating_sub(fit));
        for (k, (pick, label, n, old)) in rows.iter().enumerate().skip(top).take(fit) {
            let y = body.t + (k - top) as f32 * FOLDER_ROW;
            let rr = R::new(body.l, y, body.r - 6.0, y + FOLDER_ROW - 3.0);
            let on = &self.pick == pick;
            if on {
                p.fill(rr, 6.0, alpha(amber, 0.18));
                p.fill(R::new(rr.l, rr.t + 6.0, rr.l + 3.0, rr.b - 6.0), 1.5, amber);
            } else if self.hover == Some(DHit::Folder(k)) {
                p.fill(rr, 6.0, hex(0xFFFFFF, 0.06));
            }
            let col = if *pick == Pick::Old { hex(GOLD, 1.0) } else if on { t.text } else { t.text2 };
            p.text(label, &p.g.small, R::new(rr.l + 12.0, rr.t, rr.r - 72.0, rr.b), col, DWRITE_TEXT_ALIGNMENT_LEADING);
            let v = if *old > 0 && matches!(pick, Pick::Folder(_)) { format!("{n}  \u{00B7}  {old} old") } else { n.to_string() };
            p.text(&v, &p.g.tiny, R::new(rr.r - 90.0, rr.t, rr.r - 8.0, rr.b), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
            hits.push((rr, DHit::Folder(k)));
        }
        scrollbar(p, body, top as f32, fit as f32, rows.len() as f32);
    }

    fn paint_list(&self, ink: &Ink, r: R, m: &crate::memory::Health, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let (amber, teal, gold) = (hex(AMBER, 1.0), hex(TEAL, 1.0), hex(GOLD, 1.0));
        ink.gem(r, 14.0, amber, 0.18);
        hits.push((r, DHit::Pane(1)));
        let title = if !self.query.is_empty() {
            format!("MATCHING \"{}\"", self.query.to_uppercase())
        } else {
            match &self.pick {
                Pick::All => "ALL NOTES, NEWEST FIRST".into(),
                Pick::Old => "OVER 90 DAYS OLD, OLDEST FIRST".into(),
                Pick::Folder(f) => format!("{}, NEWEST FIRST", f.to_uppercase()),
            }
        };
        p.text(&title, &p.g.tiny, R::new(r.l + 18.0, r.t + 12.0, r.r - 80.0, r.t + 28.0), alpha(amber, 0.95), DWRITE_TEXT_ALIGNMENT_LEADING);
        p.text(&format!("{} notes", self.found.len()), &p.g.tiny, R::new(r.r - 120.0, r.t + 12.0, r.r - 18.0, r.t + 28.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        let body = R::new(r.l + 8.0, r.t + 36.0, r.r - 10.0, r.b - 10.0);
        let fit = ((body.b - body.t) / NOTE_ROW).floor().max(1.0) as usize;
        self.fits.set((self.fits.get().0, fit));
        if self.found.is_empty() {
            let msg = if !self.query.is_empty() {
                "No note has all of those words."
            } else if self.pick == Pick::Old {
                "Nothing is over 90 days old. All reviewed."
            } else {
                "No notes here."
            };
            p.text(msg, &p.g.body, R::new(body.l + 10.0, body.t + 4.0, body.r, body.t + 30.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        }
        let top = self.list_top.min(self.found.len().saturating_sub(fit));
        let mixed = !matches!(self.pick, Pick::Folder(_)) || !self.query.is_empty();
        for (k, &i) in self.found.iter().enumerate().skip(top).take(fit) {
            let n = &m.list[i];
            let y = body.t + (k - top) as f32 * NOTE_ROW;
            let rr = R::new(body.l, y, body.r - 6.0, y + NOTE_ROW - 4.0);
            let on = self.sel.as_ref() == Some(&n.path);
            if on {
                p.fill(rr, 7.0, alpha(amber, 0.17));
                p.fill(R::new(rr.l, rr.t + 8.0, rr.l + 3.0, rr.b - 8.0), 1.5, amber);
            } else if self.hover == Some(DHit::Note(k)) {
                p.fill(rr, 7.0, hex(0xFFFFFF, 0.05));
            }
            let date = if n.stale { n.updated.clone() } else { n.updated.get(5..).unwrap_or(&n.updated).to_string() };
            let dw = p.width(&date, &p.g.tiny) + 4.0;
            p.text(&n.name, &p.g.bold, R::new(rr.l + 14.0, rr.t + 5.0, rr.r - dw - 18.0, rr.t + 25.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(&date, &p.g.tiny, R::new(rr.r - dw - 12.0, rr.t + 5.0, rr.r - 10.0, rr.t + 25.0), if n.stale { gold } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_TRAILING);
            let mut dx = rr.l + 14.0;
            if mixed {
                let fw = p.width(&n.folder, &p.g.tiny);
                p.text(&n.folder, &p.g.tiny, R::new(dx, rr.t + 26.0, dx + fw + 2.0, rr.t + 44.0), alpha(teal, 0.9), DWRITE_TEXT_ALIGNMENT_LEADING);
                dx += fw + 10.0;
            }
            let (desc, dc) = if n.description.is_empty() { ("No description line, so no index shows it", gold) } else { (n.description.as_str(), t.text2) };
            p.text(desc, &p.g.small, R::new(dx, rr.t + 26.0, rr.r - 10.0, rr.t + 44.0), dc, DWRITE_TEXT_ALIGNMENT_LEADING);
            hits.push((rr, DHit::Note(k)));
        }
        scrollbar(p, body, top as f32, fit as f32, self.found.len() as f32);
    }

    fn paint_note(&self, ink: &Ink, r: R, m: &crate::memory::Health, hits: &mut Vec<(R, DHit)>) {
        let p = &ink.p;
        let t = theme(true);
        let (violet, amber, gold) = (hex(0xB58CFF, 1.0), hex(AMBER, 1.0), hex(GOLD, 1.0));
        ink.gem(r, 16.0, violet, 0.18);
        hits.push((r, DHit::Pane(2)));
        let note = self.sel.as_ref().and_then(|s| m.list.iter().find(|n| &n.path == s));
        let (Some(n), Some((_, text))) = (note, &self.body) else {
            p.text("Choose a note to read it here.", &p.g.body, R::new(r.l + 22.0, r.t + 20.0, r.r - 20.0, r.t + 46.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            return;
        };
        let (x0, x1) = (r.l + 22.0, r.r - 20.0);
        let mut y = r.t + 16.0;
        p.text(&n.name, &p.g.title, R::new(x0, y, x1, y + 24.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
        y += 26.0;
        let place = if n.folder == "shared" { "shared".to_string() } else { format!("projects\\{}", n.folder) };
        let meta = format!("{place}  \u{00B7}  updated {}{}", n.updated, if n.stale { "  \u{00B7}  over 90 days old" } else { "" });
        p.text(&meta, &p.g.tiny, R::new(x0, y, x1, y + 16.0), if n.stale { gold } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_LEADING);
        y += 24.0;
        if let Some(wf) = &self.wrap {
            let (desc, dc) = if n.description.is_empty() { ("No description line, so no chat's index shows this note. Open it and add one.", gold) } else { (n.description.as_str(), t.text) };
            let dh = p.text_height(desc, wf, x1 - x0).min(58.0);
            p.text(desc, wf, R::new(x0, y, x1, y + dh + 2.0), dc, DWRITE_TEXT_ALIGNMENT_LEADING);
            y += dh + 14.0;
        }
        // The buttons. In the review list Keep and Delete move on to the next note.
        let asking = self.confirm.as_ref().is_some_and(|(c, at)| c == &n.path && at.elapsed().as_secs() < 5);
        let del = (Act::Delete, if asking { "Click again to delete" } else { "Delete" }.to_string(), asking.then(|| hex(RED, 1.0)));
        let acts: Vec<(Act, String, Option<C>)> = if self.pick == Pick::Old && self.query.is_empty() {
            vec![(Act::Keep, "Keep: still true".into(), Some(amber)), del, (Act::Skip, "Skip".into(), None), (Act::Open, "Open".into(), None)]
        } else {
            let mut v = vec![(Act::Open, "Open".into(), Some(amber)), (Act::Reveal, "Show in folder".into(), None)];
            if n.stale {
                v.push((Act::Keep, "Mark checked".into(), None));
            }
            v.push(del);
            v
        };
        let mut bx = x0;
        for (a, label, tone) in &acts {
            let bw = p.width(label, &p.g.small) + 32.0;
            button(ink, hits, self.hover, R::new(bx, y, bx + bw, y + 30.0), label, DHit::Act(*a), *tone);
            bx += bw + 8.0;
        }
        y += 30.0 + 14.0;
        p.line(x0, y, x1, y, hex(0xFFFFFF, 0.08), 1.0);
        y += 12.0;
        self.paint_body(p, R::new(x0, y, x1, r.b - 14.0), &n.path, text);
    }

    /// The note's text, wrapped and scrolled (laid out once per note and width).
    fn paint_body(&self, p: &Painter, view: R, path: &PathBuf, text: &str) {
        let Some(wf) = &self.wrap else { return };
        let w = view.r - view.l - 10.0;
        if view.b - view.t < 20.0 {
            return;
        }
        let mut cache = self.layout.borrow_mut();
        if cache.as_ref().map_or(true, |c| &c.0 != path || (c.1 - w).abs() > 0.5 || c.2 != text.len()) {
            let body = if text.trim().is_empty() { "(This note has no text after its frontmatter.)" } else { text };
            let wt = wtext(body);
            *cache = unsafe {
                let _ = wf.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
                p.g.dwrite.CreateTextLayout(&wt, wf, w, 1.0e6).ok().map(|l| {
                    let mut tm = DWRITE_TEXT_METRICS::default();
                    let hgt = if l.GetMetrics(&mut tm).is_ok() { tm.height } else { 0.0 };
                    (path.clone(), w, text.len(), l, hgt)
                })
            };
        }
        let Some((_, _, _, layout, hgt)) = cache.as_ref() else { return };
        let max = (hgt - (view.b - view.t) + 8.0).max(0.0);
        self.body_max.set(max);
        let top = self.body_top.min(max);
        unsafe {
            p.rt.PushAxisAlignedClip(&view.d2(), D2D1_ANTIALIAS_MODE_ALIASED);
            p.color(hex(0xFFFFFF, 0.80));
            p.rt.DrawTextLayout(pt(view.l, view.t - top), layout, p.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            p.rt.PopAxisAlignedClip();
        }
        scrollbar(p, R::new(view.l, view.t, view.r + 6.0, view.b), top, view.b - view.t, *hgt + 8.0);
    }
}

/// A button: filled with `tone` for the main action, else glass.
fn button(ink: &Ink, hits: &mut Vec<(R, DHit)>, hover: Option<DHit>, r: R, label: &str, id: DHit, tone: Option<C>) {
    let p = &ink.p;
    let cut = 6.0;
    let shape = [(r.l + cut, r.t), (r.r, r.t), (r.r, r.b - cut), (r.r - cut, r.b), (r.l, r.b), (r.l, r.t + cut)];
    let on = hover == Some(id);
    match tone {
        Some(c) => {
            if on {
                ink.halo((r.l + r.r) / 2.0, r.b, (r.r - r.l) * 0.5, c, 0.4);
            }
            ink.poly(&shape, alpha(c, if on { 1.0 } else { 0.86 }));
            p.text(label, &p.g.small, r, hex(0x1A1208, 1.0), DWRITE_TEXT_ALIGNMENT_CENTER);
        }
        None => {
            ink.poly(&shape, hex(0xFFFFFF, if on { 0.13 } else { 0.06 }));
            ink.poly_edge(&shape, hex(0xFFFFFF, 0.14), 1.0);
            p.text(label, &p.g.small, r, hex(0xFFFFFF, 0.9), DWRITE_TEXT_ALIGNMENT_CENTER);
        }
    }
    hits.push((r, id));
}

/// A thin bar on the right of `r` when there is more than fits: `top` and `view` of
/// `total`, in any one unit.
fn scrollbar(p: &Painter, r: R, top: f32, view: f32, total: f32) {
    if total <= view + 0.5 {
        return;
    }
    let track = R::new(r.r - 4.0, r.t, r.r - 1.0, r.b);
    let h = ((r.b - r.t) * view / total).max(24.0);
    let y = r.t + (r.b - r.t - h) * (top / (total - view)).clamp(0.0, 1.0);
    p.fill(track, 1.5, hex(0xFFFFFF, 0.05));
    p.fill(R::new(track.l, y, track.r, y + h), 1.5, hex(0xFFFFFF, 0.30));
}

fn as_stale(as_of: u64) -> bool {
    as_of > 0 && crate::time::now().saturating_sub(as_of) > 45 * 60
}

/// A row of choices; returns the x after the last one.
fn chips(ink: &Ink, hits: &mut Vec<(R, DHit)>, hover: Option<DHit>, mut x: f32, y: f32, names: &[String], selected: Option<usize>, id: impl Fn(usize) -> DHit) -> f32 {
    let p = &ink.p;
    let amber = hex(AMBER, 1.0);
    for (i, name) in names.iter().enumerate() {
        let cw = (p.width(name, &p.g.small) + 22.0).max(46.0);
        let r = R::new(x, y, x + cw, y + 28.0);
        let hid = id(i);
        let cut = 7.0;
        let shape = [(r.l + cut, r.t), (r.r, r.t), (r.r, r.b - cut), (r.r - cut, r.b), (r.l, r.b), (r.l, r.t + cut)];
        if selected == Some(i) {
            ink.halo((r.l + r.r) / 2.0, r.b, cw * 0.6, amber, 0.35);
            ink.poly(&shape, amber);
            p.text(name, &p.g.small, r, hex(0x1A1208, 1.0), DWRITE_TEXT_ALIGNMENT_CENTER);
        } else {
            ink.poly(&shape, hex(0xFFFFFF, if hover == Some(hid) { 0.10 } else { 0.045 }));
            ink.poly_edge(&shape, hex(0xFFFFFF, 0.10), 1.0);
            p.text(name, &p.g.small, r, hex(0xFFFFFF, 0.85), DWRITE_TEXT_ALIGNMENT_CENTER);
        }
        hits.push((r, hid));
        x += cw + 4.0;
    }
    x
}

/// A round number at or above `v` for the chart's top line.
fn nice_ceiling(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let mag = 10f64.powf(v.log10().floor());
    for step in [1.0, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0] {
        if step * mag >= v {
            return step * mag;
        }
    }
    10.0 * mag
}

// ---- the window --------------------------------------------------------------------

impl Dash {
    /// Draw a frame on the window's GPU render target.
    fn frame(&mut self) {
        if self.gfx.is_none() {
            self.gfx = make_gfx().ok();
        }
        let Some(g) = self.gfx.as_ref() else { return };
        if self.big.is_none() {
            self.big = make_format(&g.dwrite, w!("Segoe UI Variable Display"), 26.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, false).ok();
        }
        if self.wrap.is_none() {
            self.wrap = make_format(&g.dwrite, w!("Segoe UI Variable Text"), 13.5, DWRITE_FONT_WEIGHT_NORMAL, true).ok();
        }
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut rc) };
        let size = D2D_SIZE_U { width: (rc.right - rc.left).max(1) as u32, height: (rc.bottom - rc.top).max(1) as u32 };
        if self.fx.is_none() {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES { hwnd: windows::Win32::Foundation::HWND(self.hwnd as _), pixelSize: size, presentOptions: D2D1_PRESENT_OPTIONS_NONE };
            let Ok(rt) = (unsafe { g.factory.CreateHwndRenderTarget(&props, &hp) }) else { return };
            unsafe { rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE) };
            let Some(res) = Res::new(&rt) else { return };
            self.fx = Some(Fx { rt, res, size });
        }
        if let Some(fx) = self.fx.as_mut() {
            if (fx.size.width, fx.size.height) != (size.width, size.height) {
                unsafe {
                    let _ = fx.rt.Resize(&size);
                }
                fx.size = size;
            }
        }
        let Some(fx) = self.fx.as_ref() else { return };
        // Fully covered by other windows: nothing to see, so nothing to draw.
        if unsafe { fx.rt.CheckWindowState() }.0 & D2D1_WINDOW_STATE_OCCLUDED.0 != 0 {
            return;
        }
        let started = Instant::now();
        unsafe {
            fx.rt.SetDpi(96.0 * self.scale, 96.0 * self.scale);
            fx.rt.BeginDraw();
        }
        let base: &ID2D1RenderTarget = &fx.rt;
        let ink = Ink { p: Painter { g, rt: base, brush: &fx.res.brush }, res: &fx.res, t: self.born.elapsed().as_secs_f32() };
        let (w, h) = (size.width as f32 / self.scale, size.height as f32 / self.scale);
        let mut hits = Vec::new();
        self.paint(&ink, w, h, &mut hits);
        let failed = unsafe { fx.rt.EndDraw(None, None) }.is_err();
        self.hits = hits;
        if failed {
            self.fx = None; // the device was lost: make it again next frame
        }
        self.frames.0 += 1;
        self.frames.1 += started.elapsed().as_secs_f64() * 1000.0;
        if self.frames.0 == 150 {
            crate::log(&format!("dashboard: {:.1} ms a frame on average ({}x{} px)", self.frames.1 / 150.0, size.width, size.height));
        }
    }
}

unsafe fn create_window() -> Option<HWND> {
    let hinst = GetModuleHandleW(null());
    let class = wide(DCLASS);
    let icon = LoadImageW(hinst, 1usize as *const u16, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE) as HICON;
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(dproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: icon,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);
    let mut pnt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pnt);
    let (wk, scale) = monitor_info(MonitorFromPoint(pnt, MONITOR_DEFAULTTONEAREST));
    let (sw, sh) = ((wk.right - wk.left) as f32, (wk.bottom - wk.top) as f32);
    let w = (1320.0 * scale).min(sw - 40.0);
    let h = (940.0 * scale).min(sh - 40.0);
    let x = wk.left + ((sw - w) / 2.0) as i32;
    let y = wk.top + ((sh - h) / 2.0) as i32;
    let title = wide(&format!("{}: dashboard", crate::APP_NAME));
    let hwnd = CreateWindowExW(WS_EX_APPWINDOW, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW, x, y, w as i32, h as i32, null_mut(), null_mut(), hinst, null());
    if hwnd.is_null() {
        return None;
    }
    crate::taskbar::own_window(hwnd);
    apply_theme(hwnd, true);
    DASH.with(|c| {
        *c.borrow_mut() = Some(Dash {
            hwnd,
            gfx: None,
            fx: None,
            big: None,
            scale,
            range: 1,
            metric: 0,
            account: None,
            accounts: Vec::new(),
            pods: Vec::new(),
            cpu: HashMap::new(),
            summary: None,
            bottom: 0,
            top: TopPanel::default(),
            week: std::collections::BTreeMap::new(),
            days: Vec::new(),
            hits: Vec::new(),
            hover: None,
            tracking: false,
            updated: None,
            born: Instant::now(),
            shown_at: Instant::now(),
            frames: (0, 0.0),
            view: 0,
            mem: None,
            mem_at: None,
            mem_shown: Instant::now(),
            mem_page: 0,
            query: String::new(),
            pick: Pick::All,
            found: Vec::new(),
            sel: None,
            body: None,
            folder_top: 0,
            list_top: 0,
            body_top: 0.0,
            fits: Cell::new((1, 1)),
            body_max: Cell::new(0.0),
            confirm: None,
            toast: None,
            wrap: None,
            layout: RefCell::new(None),
        })
    });
    Some(hwnd)
}

fn hit_at(lp: LPARAM) -> Option<DHit> {
    let (x, y) = ((lp & 0xffff) as i16 as f32, ((lp >> 16) & 0xffff) as i16 as f32);
    dwith(|d| d.hits.iter().rev().find(|(r, _)| r.contains(x / d.scale, y / d.scale)).map(|(_, h)| *h)).flatten()
}

unsafe extern "system" fn dproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            dwith(|d| d.frame());
            EndPaint(hwnd, &ps);
            0
        }
        WM_SIZE => {
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_TIMER if wp == TIMER_FRAME => {
            // Animate only while it can be seen, and at a third of the rate while you
            // work in another window (it is still moving, at a fraction of the cost).
            let n = BACK_TICK.fetch_add(1, Ordering::Relaxed);
            let front = windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() == hwnd;
            if IsIconic(hwnd) == 0 && IsWindowVisible(hwnd) != 0 && (front || n % 3 == 0) {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_TIMER if wp == TIMER_LIVE => {
            if IsIconic(hwnd) == 0 && IsWindowVisible(hwnd) != 0 {
                poll_live();
                // The Memory view reads the vault again every minute while it shows.
                if dwith(|d| d.view == 1 && d.mem_at.map_or(true, |a| a.elapsed().as_secs() >= 60)).unwrap_or(false) {
                    load_mem();
                }
            }
            0
        }
        WM_DASH_MEM => {
            let h = MEM.lock().ok().and_then(|mut m| m.take());
            let toast = TOAST.lock().ok().and_then(|mut t| t.take());
            dwith(|d| {
                if let Some(h) = h {
                    if d.mem.is_none() {
                        d.mem_shown = Instant::now();
                    }
                    d.mem = Some(h);
                    d.mem_at = Some(Instant::now());
                    d.refilter(false);
                    d.load_body(true);
                }
                if let Some((msg, ok)) = toast {
                    d.toast = Some((msg, ok, Instant::now()));
                }
            });
            0
        }
        WM_MOUSEWHEEL_ => {
            let notches = ((wp >> 16) & 0xffff) as i16 as f32 / 120.0;
            let mut pnt = POINT { x: (lp & 0xffff) as i16 as i32, y: ((lp >> 16) & 0xffff) as i16 as i32 };
            ScreenToClient(hwnd, &mut pnt);
            dwith(|d| d.scroll_at(pnt.x as f32 / d.scale, pnt.y as f32 / d.scale, notches));
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_CHAR => {
            if let Some(ch) = char::from_u32(wp as u32).filter(|c| !c.is_control()) {
                dwith(|d| d.typed(ch));
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_DASH_LIVE => {
            apply_live();
            0
        }
        WM_GETMINMAXINFO => {
            let scale = dwith(|d| d.scale).unwrap_or(1.0);
            let mm = &mut *(lp as *mut MINMAXINFO);
            mm.ptMinTrackSize.x = (MIN_W * scale) as i32;
            mm.ptMinTrackSize.y = (MIN_H * scale) as i32;
            0
        }
        WM_DPICHANGED => {
            let dpi = (wp & 0xffff) as f32;
            dwith(|d| d.scale = crate::win::debug_scale().unwrap_or(dpi / 96.0));
            let r = &*(lp as *const RECT);
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            0
        }
        WM_MOUSEMOVE => {
            let h = hit_at(lp);
            dwith(|d| {
                if !d.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut tme);
                    d.tracking = true;
                }
                d.hover = h;
            });
            0
        }
        WM_MOUSELEAVE => {
            dwith(|d| {
                d.tracking = false;
                d.hover = None;
            });
            0
        }
        WM_LBUTTONUP => {
            match hit_at(lp) {
                Some(DHit::Range(i)) => {
                    dwith(|d| d.range = i);
                    recompute();
                }
                Some(DHit::Metric(i)) => {
                    dwith(|d| d.metric = i);
                    recompute();
                }
                Some(DHit::Bottom(i)) => {
                    dwith(|d| d.bottom = i);
                }
                Some(DHit::Account(i)) => {
                    dwith(|d| d.account = i);
                    recompute();
                }
                Some(DHit::Refresh) => {
                    if dwith(|d| d.view).unwrap_or(0) == 1 {
                        load_mem();
                    } else {
                        refresh_pods();
                        poll_live();
                        load(hwnd);
                    }
                }
                Some(DHit::View(i)) => {
                    dwith(|d| d.view = i);
                    if i == 1 {
                        load_mem();
                    }
                }
                Some(DHit::MemPage(i)) => {
                    dwith(|d| d.mem_page = i);
                }
                Some(DHit::Folder(k)) => {
                    dwith(|d| {
                        if let Some(row) = d.folder_rows().into_iter().nth(k) {
                            d.choose(row.0);
                        }
                    });
                }
                Some(DHit::Review) => {
                    dwith(|d| d.choose(Pick::Old));
                }
                Some(DHit::ClearSearch) => {
                    dwith(|d| {
                        d.query.clear();
                        d.refilter(true);
                    });
                }
                Some(DHit::Note(k)) => {
                    dwith(|d| d.select(k));
                }
                Some(DHit::Act(a)) => {
                    dwith(|d| d.act(a));
                }
                _ => {}
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_DASH_PROGRESS => 0,
        WM_DASH_TOP => {
            let panel = TOP.lock().ok().and_then(|t| t.clone());
            if let Some(panel) = panel {
                dwith(|d| d.top = panel);
            }
            0
        }
        WM_DASH_READY => {
            refresh_pods();
            recompute();
            0
        }
        WM_KEYDOWN => {
            let k = wp as u16;
            let used = dwith(|d| d.key(k)).unwrap_or(false);
            if used {
                InvalidateRect(hwnd, null(), 0);
            } else if k == VK_ESCAPE {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_FRAME);
            KillTimer(hwnd, TIMER_LIVE);
            // The read data goes too (about 40 MB): the tray stays small, and the next
            // opening reads the saved index back in under a second.
            DASH.with(|c| *c.borrow_mut() = None);
            if !LOADING.load(Ordering::SeqCst) {
                if let Ok(mut d) = DATA.lock() {
                    *d = None;
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Sample rows for `--render` (invented titles and numbers).
fn sample_top() -> TopPanel {
    let row = |title: &str, account: &str, mix: [u64; 5], context: u64| {
        let tokens: u64 = mix.iter().sum();
        TopRow { title: title.into(), account: account.into(), mix, tokens, context, past: context > stats::HANDOFF_AT }
    };
    TopPanel {
        rows: vec![
            row("Quarterly planning notes", "Work", [3_900_000_000, 400_000_000, 10_000_000, 0, 0], 612_000),
            row("Build the tray usage nudge", "Personal", [2_100_000_000, 900_000_000, 40_000_000, 0, 0], 148_000),
            row("Memory vault cleanup", "Personal", [800_000_000, 1_700_000_000, 0, 0, 0], 331_000),
            row("Terminal chat 3f9a21c4", "Terminal and other", [2_300_000_000, 0, 0, 0, 0], 91_000),
            row("Docs and release notes", "Work", [1_500_000_000, 200_000_000, 0, 120_000_000, 0], 205_000),
            row("Card 118: limit log", "Personal", [900_000_000, 600_000_000, 0, 0, 0], 77_000),
            row("Hive installer notes", "Freelance", [100_000_000, 1_100_000_000, 80_000_000, 0, 0], 304_000),
            row("Bug triage", "Work", [700_000_000, 300_000_000, 0, 0, 0], 52_000),
            row("Dashboard polish", "Personal", [500_000_000, 400_000_000, 0, 0, 0], 120_000),
            row("Weekly team call prep", "Work", [300_000_000, 500_000_000, 20_000_000, 0, 0], 66_000),
        ],
        left: 0,
        ready: true,
        missed: 3,
    }
}

/// Off-screen picture of the dashboard for `--render`, from sample numbers.
pub fn render_sample(scale: f32, view: usize) -> Option<(i32, i32, Vec<u8>)> {
    // HIVE_DASH_BOTTOM=1 draws the usage view with the top-chats panel showing.
    let bottom: usize = std::env::var("HIVE_DASH_BOTTOM").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    let accounts: Vec<String> = ["Personal", "Work", "Side project", "Freelance", "Codex", stats::OTHER].iter().map(|s| s.to_string()).collect();
    let today = crate::time::now();
    let mut s = Summary::default();
    let mut days = Vec::new();
    for i in (0..30u64).rev() {
        let day = ymd(today - i * 86_400);
        days.push(day);
        for (k, a) in accounts.iter().enumerate() {
            let wave = ((i as f64 * 0.7 + k as f64).sin() + 1.3) * (5 - k.min(4)) as f64;
            let t = stats::Tokens { input: 1000, w5: 0, w1h: 2_000_000, read: (wave * 40e6) as u64, output: (wave * 300e3) as u64, replies: (wave * 60.0) as u64 };
            let cost = if a == "Codex" { 0.0 } else { wave * 110.0 };
            let e = s.by_day.entry(day).or_default().entry(a.clone()).or_default();
            e.tokens.add(&t);
            e.cost += cost;
            let b = s.by_account.entry(a.clone()).or_default();
            b.tokens.add(&t);
            b.cost += cost;
            s.total.tokens.add(&t);
            s.total.cost += cost;
        }
    }
    // Model share per day: Opus drifts up and down with a few heavy days.
    for (i, day) in days.iter().enumerate() {
        let opus = (0.35 + 0.3 * ((i as f64 * 0.55).sin() * 0.5 + 0.5) + if i % 9 == 4 { 0.2 } else { 0.0 }).min(0.9);
        let rest = 1.0 - opus;
        let total = 1_000_000_000.0;
        s.by_day_family.insert(*day, [(total * opus) as u64, (total * rest * 0.62) as u64, (total * rest * 0.14) as u64, (total * rest * 0.14) as u64, (total * rest * 0.10) as u64]);
    }
    let week: std::collections::BTreeMap<String, [u64; 5]> = [
        ("Personal", [5_200_000_000, 2_900_000_000, 90_000_000, 0, 0]),
        ("Work", [4_100_000_000, 1_300_000_000, 0, 160_000_000, 0]),
        ("Side project", [900_000_000, 1_100_000_000, 40_000_000, 0, 0]),
        ("Codex", [0, 0, 0, 0, 2_400_000_000]),
        ("Freelance", [300_000_000, 500_000_000, 20_000_000, 0, 0]),
    ]
    .iter()
    .map(|(a, f)| (a.to_string(), *f))
    .collect();
    for (d, row) in s.by_hour.iter_mut().enumerate() {
        for (hr, a) in row.iter_mut().enumerate() {
            let work = if (9..=18).contains(&hr) && (1..=5).contains(&d) { 1.0 } else if (20..=23).contains(&hr) { 0.45 } else { 0.05 };
            a.cost = work * (1.0 + ((d * 24 + hr) as f64 * 1.3).sin().abs()) * 40.0;
        }
    }
    for (mdl, c) in [("Opus 5.5", 0.5), ("Sonnet 5.5", 0.2), ("Opus 5", 0.15), ("GPT-6.1 Sol", 0.0), ("Fable 5.1", 0.04), ("Haiku 5.5", 0.01)] {
        s.by_model.insert(mdl.into(), Agg { tokens: stats::Tokens { input: 1, w5: 0, w1h: 0, read: 5_000_000_000, output: 0, replies: 1 }, cost: s.total.cost * c });
    }
    for (pj, c) in [("web-dashboard", 0.31), ("hive", 0.18), ("sample-game", 0.14), ("mobile-app", 0.11), ("docs-site", 0.08), ("asset-library", 0.07), ("reporting", 0.06)] {
        s.by_project.insert(pj.into(), Agg { tokens: stats::Tokens::default(), cost: s.total.cost * c });
    }
    let hh = 3600;
    let hours = |k: usize| {
        let mut a = [0.0; 24];
        for (i, v) in a.iter_mut().enumerate() {
            *v = ((i as f64 * 0.6 + k as f64).sin() + 1.1).max(0.0) * if i > 8 { 1.0 } else { 0.2 };
        }
        a
    };
    let pods = vec![
        Pod { name: "Personal".into(), color: color_of(&accounts, "Personal"), codex: false, open: true, chats: 4, busy: 0.6, five: Some(64), five_fc: Forecast::At(today + 2 * hh), week: Some(81), week_fc: Forecast::At(today + 30 * hh), as_of: today, past95: 3, last95: Some(today - 5 * hh), hours: hours(0), today: Agg { tokens: stats::Tokens::default(), cost: 412.0 } },
        Pod { name: "Work".into(), color: color_of(&accounts, "Work"), codex: false, open: true, chats: 4, busy: 0.0, five: Some(22), five_fc: Forecast::Lasts, week: Some(58), week_fc: Forecast::Lasts, as_of: today, past95: 0, last95: Some(today - 9 * 24 * hh), hours: hours(1), today: Agg { tokens: stats::Tokens::default(), cost: 288.5 } },
        Pod { name: "Side project".into(), color: color_of(&accounts, "Side project"), codex: false, open: true, chats: 14, busy: 0.3, five: None, five_fc: Forecast::Unknown, week: Some(9), week_fc: Forecast::Lasts, as_of: today - 6 * hh, past95: 0, last95: None, hours: hours(2), today: Agg { tokens: stats::Tokens::default(), cost: 96.0 } },
        Pod { name: "Freelance".into(), color: color_of(&accounts, "Freelance"), codex: false, open: true, chats: 4, busy: 0.0, five: Some(100), five_fc: Forecast::Reached, week: Some(37), week_fc: Forecast::Unknown, as_of: today, past95: 1, last95: Some(today - 26 * hh), hours: hours(3), today: Agg::default() },
        Pod { name: "Codex".into(), color: color_of(&accounts, "Codex"), codex: true, open: false, chats: 0, busy: 0.0, five: Some(12), five_fc: Forecast::Unknown, week: Some(41), week_fc: Forecast::Unknown, as_of: 0, past95: 0, last95: None, hours: hours(4), today: Agg { tokens: stats::Tokens { input: 0, w5: 0, w1h: 0, read: 3_200_000_000, output: 23_000_000, replies: 1 }, cost: 0.0 } },
    ];
    let d = Dash {
        hwnd: null_mut(),
        gfx: make_gfx().ok(),
        fx: None,
        big: None,
        scale,
        range: 1,
        metric: 0,
        account: None,
        accounts,
        pods,
        cpu: HashMap::new(),
        summary: Some(s),
        bottom: bottom.min(2),
        top: sample_top(),
        week,
        days,
        hits: Vec::new(),
        hover: Some(DHit::Bar(26)),
        tracking: false,
        updated: Some(today),
        born: Instant::now() - std::time::Duration::from_secs(3),
        shown_at: Instant::now() - std::time::Duration::from_secs(5),
        frames: (0, 0.0),
        view: view.min(1),
        mem: (view >= 1).then(crate::memory::health),
        mem_at: Some(Instant::now()),
        mem_shown: Instant::now() - std::time::Duration::from_secs(5),
        mem_page: 0,
        query: String::new(),
        pick: if view == 2 { Pick::Old } else { Pick::All },
        found: Vec::new(),
        sel: None,
        body: None,
        folder_top: 0,
        list_top: 0,
        body_top: 0.0,
        fits: Cell::new((1, 1)),
        body_max: Cell::new(0.0),
        confirm: None,
        toast: None,
        wrap: None,
        layout: RefCell::new(None),
    };
    let g = d.gfx.as_ref()?;
    let big = make_format(&g.dwrite, w!("Segoe UI Variable Display"), 26.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, false).ok();
    let wrap = make_format(&g.dwrite, w!("Segoe UI Variable Text"), 13.5, DWRITE_FONT_WEIGHT_NORMAL, true).ok();
    let mut d = Dash { big, wrap, ..d };
    d.refilter(true);
    let g = d.gfx.as_ref()?;
    let (w, hgt) = ((1320.0 * scale) as i32, (940.0 * scale) as i32);
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -hgt, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
        let mut bits: *mut core::ffi::c_void = null_mut();
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, dib as _);
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 0.0,
            dpiY: 0.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = g.factory.CreateDCRenderTarget(&props).ok()?;
        let wr = windows::Win32::Foundation::RECT { left: 0, top: 0, right: w, bottom: hgt };
        rt.BindDC(windows::Win32::Graphics::Gdi::HDC(dc as _), &wr).ok()?;
        rt.SetDpi(96.0 * scale, 96.0 * scale);
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let base: &ID2D1RenderTarget = &rt;
        let res = Res::new(base)?;
        rt.BeginDraw();
        let ink = Ink { p: Painter { g, rt: base, brush: &res.brush }, res: &res, t: 3.0 };
        let mut hits = Vec::new();
        d.paint(&ink, w as f32 / scale, hgt as f32 / scale, &mut hits);
        let _ = rt.EndDraw(None, None);
        let px = std::slice::from_raw_parts(bits as *const u8, (w * hgt * 4) as usize).to_vec();
        SelectObject(dc, old);
        DeleteObject(dib as _);
        DeleteDC(dc);
        Some((w, hgt, px))
    }
}
