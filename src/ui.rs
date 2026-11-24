//! Frosted-glass flyout (Windows 11 acrylic backdrop, drawn with Direct2D):
//! the account list with live status, usage and reset times, quick actions,
//! and the Add / Edit account form. Opens from the tray icon or the hotkey;
//! pinned, it stays open as a normal window with a taskbar button.

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::BOOL as WBOOL;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Bitmap, ID2D1RenderTarget, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_HIT_TEST_METRICS, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmFlush, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, RedrawWindow,
    RDW_INVALIDATE, RDW_UPDATENOW,
    ScreenToClient, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
    PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    BeginBufferedPaint, BufferedPaintInit, EndBufferedPaint, BPBF_TOPDOWNDIB, BPPF_ERASE,
    BP_PAINTPARAMS, MARGINS,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::tray;
use crate::win::{get_clipboard, reg_get_dword, set_clipboard, wide};

// ---- data the tray hands us ----------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Claude,
    Codex,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Normal,
    Warn,
    Bad,
}

pub struct Row {
    pub kind: Kind,
    pub name: String,
    pub status: String,
    pub running: bool,
    pub signed_out: bool,
    pub chrome: bool,
    /// Usage bars: label and percent (None = window has reset).
    pub bars: Vec<(String, Option<u8>)>,
    /// Reset line, e.g. "Resets Tue ~3 AM (5d 11h)".
    pub note: Option<(String, Tone)>,
    pub best: bool,
    /// Weekly % over the last 7 days, for the trend line.
    pub trend: Vec<u8>,
}

/// A Code-tab chat in some account's sidebar, for search.
#[derive(Clone)]
pub struct ChatItem {
    pub account: String,
    pub id: String,
    pub title: String,
    pub last: u64,
    pub archived: bool,
    /// Claude Code conversation id (for continuing it in another account).
    pub cli: String,
}

#[derive(Default)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    pub waiting: Option<String>,
    pub drive: Option<bool>,
    pub best: Option<String>,
    pub pinned: bool,
    pub pinned_at: Option<[i32; 2]>,
    /// Color of each account's avatar and name tag.
    pub palette: Vec<(String, u32)>,
    pub opts: Opts,
}

/// Current settings, for the Settings screen.
#[derive(Default, Clone)]
pub struct Opts {
    /// Every shortcut, for the Shortcuts page.
    pub hotkeys: Vec<KeyRow>,
    pub start_with_windows: bool,
    pub wait_drive: bool,
    pub drive_path: String,
    pub tags: bool,
    pub taskbar: bool,
    pub layout_auto: bool,
    pub codex: bool,
    pub alert_percent: u8,
    pub use_next: u8,
    pub usage_bars: String,
    pub alert_sound: String,
    pub alert_card: String,
    pub alert_style: String,
    pub daily_summary: String,
    pub usage_nudge: String,
    pub notify: bool,
    pub notify_waiting: bool,
    pub questions: bool,
    pub codex_alerts: bool,
    pub card_seen: bool,
    /// Where alert cards sit: monitor device name, its left-to-right index, corner setting.
    pub card_monitor: String,
    pub card_monitor_index: Option<usize>,
    pub card_corner: String,
}

/// One shortcut on the Shortcuts page.
#[derive(Clone, Default)]
pub struct KeyRow {
    /// "show", "account:<name>", "use_next" or "open_all".
    pub key: String,
    pub label: String,
    /// "" = not set.
    pub combo: String,
    /// Why it isn't working (taken by another app, a duplicate, ...).
    pub error: Option<String>,
    /// 0 this app, 1 accounts, 2 more.
    pub section: u8,
}

// ---- look -----------------------------------------------------------------

#[derive(Clone, Copy)]
struct C(f32, f32, f32, f32);

const fn hex(v: u32, a: f32) -> C {
    C(((v >> 16) & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0, a)
}

struct Theme {
    tint: C,
    card: C,
    card_hover: C,
    border: C,
    text: C,
    text2: C,
    text3: C,
    accent: C,
    on_accent: C,
    green: C,
    amber: C,
    red: C,
    ring: C,
    input: C,
    track: C,
}

/// The app's accent, from the logo's main amber (#F57A08). Dark text on it reads
/// better than white (6.4:1 against 2.7:1).
const ACCENT: u32 = 0xF57A08;
/// The same hue, deeper, for the light theme (white text 5.2:1).
const ACCENT_LIGHT: u32 = 0xC2410C;
const CODEX: u32 = 0x10A37F;

fn theme(dark: bool) -> Theme {
    if dark {
        Theme {
            tint: hex(0x1C1C1C, 0.46),
            card: hex(0xFFFFFF, 0.045),
            card_hover: hex(0xFFFFFF, 0.085),
            border: hex(0xFFFFFF, 0.08),
            text: hex(0xFFFFFF, 0.95),
            text2: hex(0xFFFFFF, 0.68),
            text3: hex(0xFFFFFF, 0.45),
            accent: hex(ACCENT, 1.0),
            on_accent: hex(0x1A1A1A, 1.0),
            green: hex(0x6CCB5F, 1.0),
            amber: hex(0xF2B33D, 1.0),
            red: hex(0xFF7A7A, 1.0),
            ring: hex(0x2A2A2A, 1.0),
            input: hex(0xFFFFFF, 0.06),
            track: hex(0xFFFFFF, 0.12),
        }
    } else {
        Theme {
            tint: hex(0xF3F3F3, 0.50),
            card: hex(0xFFFFFF, 0.50),
            card_hover: hex(0xFFFFFF, 0.80),
            border: hex(0x000000, 0.07),
            text: hex(0x1B1B1B, 1.0),
            text2: hex(0x1B1B1B, 0.64),
            text3: hex(0x1B1B1B, 0.45),
            accent: hex(ACCENT_LIGHT, 1.0),
            on_accent: hex(0xFFFFFF, 1.0),
            green: hex(0x0F7B0F, 1.0),
            amber: hex(0x9D5D00, 1.0),
            red: hex(0xC42B1C, 1.0),
            ring: hex(0xF0F0F0, 1.0),
            input: hex(0xFFFFFF, 0.75),
            track: hex(0x000000, 0.10),
        }
    }
}

const AVATARS: [u32; 6] = [0xD97757, 0x4A8FD9, 0x4FA36A, 0x8E6BC9, 0x2E9E99, 0xC9922E];

fn is_dark() -> bool {
    reg_get_dword("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize", "AppsUseLightTheme") == Some(0)
}

// ---- geometry ---------------------------------------------------------------

const W: f32 = 380.0;
const PAD: f32 = 12.0;
const HEADER: f32 = 56.0;

#[derive(Clone, Copy, Default)]
struct R {
    l: f32,
    t: f32,
    r: f32,
    b: f32,
}

impl R {
    fn new(l: f32, t: f32, r: f32, b: f32) -> R {
        R { l, t, r, b }
    }
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    fn d2(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.l, top: self.t, right: self.r, bottom: self.b }
    }
    fn inset(&self, x: f32, y: f32) -> R {
        R::new(self.l + x, self.t + y, self.r - x, self.b - y)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Row(usize),
    More(usize),
    Chrome(usize),
    OpenAll,
    OpenBest,
    SaveLayout,
    RestoreLayout,
    Add,
    Settings,
    Pin,
    Close,
    SkipWait,
    Back,
    Field(usize),
    Toggle(usize),
    Submit,
    Cancel,
    Search,
    SearchBox,
    Result(usize),
    Opt(OptHit),
    /// A Tidy up row (index into its whole list).
    TidyRow(usize),
    TidyAll,
    TidyGo,
    Dashboard,
    /// A row of the Waiting list (index into `Fly::waiting`).
    Wait(usize),
    WaitDismiss,
    /// The small x on a waiting row (index into `Fly::waiting`).
    WaitX(usize),
}

/// Something on the Settings screen.
#[derive(Clone, Copy, PartialEq, Debug)]
enum OptHit {
    Switch(Sw),
    /// A choice in a row of buttons: (row, index).
    Chip(Group, usize),
    /// A tab of the Settings screen.
    Tab(u8),
    /// An (i) explainer, by row: shows its text while hovered.
    Info(u8),
    /// Record the shortcut in row i / clear it.
    Key(usize),
    KeyClear(usize),
    DrivePick,
    Action(u8),
    /// A number box: 0 usage alert, 1 use next.
    Number(usize),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sw {
    Start,
    Drive,
    Tags,
    Taskbar,
    LayoutAuto,
    Codex,
    Notify,
    NotifyWaiting,
    Questions,
    CodexAlerts,
    CardSeen,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Group {
    Nudge,
    Alert,
    UseNext,
    Summary,
    Bars,
    Sound,
    Stay,
    Style,
    CardMonitor,
    CardCorner,
}

/// Alert style: (label, setting).
const STYLE_CHOICES: [(&str, &str); 2] = [("Card", "card"), ("Windows", "windows")];

/// How long alerts stay up: (label, setting).
const STAY_CHOICES: [(&str, &str); 3] = [("30 s", "30"), ("2 min", "120"), ("Until closed", "until")];

/// Alert sound choices: (label, setting).
const SOUND_CHOICES: [(&str, &str); 3] = [("Soft", "soft"), ("Windows", "windows"), ("None", "none")];

/// Usage bar choices: (label, setting).
const BAR_CHOICES: [(&str, &str); 4] = [("Week+5h", "week_5h"), ("Week", "week"), ("5h+Week", "5h_week"), ("5h", "5h")];

/// The bars a row shows, in the chosen order (the first one is drawn big).
fn arrange_bars<'a>(bars: &'a [(String, Option<u8>)], view: &str) -> Vec<&'a (String, Option<u8>)> {
    let pick = |l: &str| bars.iter().find(|b| b.0 == l);
    let order: &[&str] = match view {
        "week" => &["Week"],
        "5h" => &["5h"],
        "5h_week" => &["5h", "Week"],
        _ => &["Week", "5h"],
    };
    let mut out: Vec<_> = order.iter().filter_map(|l| pick(l)).collect();
    if out.is_empty() {
        out = bars.iter().collect(); // other windows (some Codex plans): show what there is
    }
    out
}

/// Keep usage updating: (label, setting).
const NUDGE_CHOICES: [(&str, &str); 4] = [("Off", ""), ("When away", "away"), ("3 AM", "3:00 AM"), ("7 AM", "7:00 AM")];
const SUMMARY_CHOICES: [(&str, &str); 4] = [("Off", ""), ("8 AM", "8:00 AM"), ("9 AM", "9:00 AM"), ("10 AM", "10:00 AM")];

fn row_height(r: &Row) -> f32 {
    let mut h = 52.0;
    if !r.bars.is_empty() {
        h += 18.0;
    }
    if r.note.is_some() {
        h += 18.0;
    }
    h + 6.0
}

// The Waiting list (chats waiting on you) above the account rows.
const WAIT_HEAD: f32 = 26.0;
const WAIT_ROW: f32 = 38.0;
const WAIT_GROUP: f32 = 20.0;
const WAIT_MORE: f32 = 20.0;

/// Height of the Waiting section, 0 when nothing waits.
fn wait_height(items: &[waiting::Item]) -> f32 {
    if items.is_empty() {
        return 0.0;
    }
    let plan = waiting::plan(items);
    let mut h = WAIT_HEAD;
    for e in &plan.entries {
        h += match e {
            waiting::Entry::Chat { .. } => WAIT_ROW + 3.0,
            waiting::Entry::Group(_) => WAIT_GROUP,
        };
    }
    if plan.more > 0 {
        h += WAIT_MORE;
    }
    h + 10.0
}

fn list_height(s: &Snapshot, w: &[waiting::Item]) -> f32 {
    let mut y = 64.0;
    if s.waiting.is_some() {
        y += 58.0;
    }
    y += wait_height(w);
    if s.rows.is_empty() {
        y += 60.0;
    }
    for r in &s.rows {
        y += row_height(r) + 4.0;
    }
    y + 8.0 + 36.0 + 40.0 + PAD + 2.0
}

const FIELDS: usize = 4;

fn form_height(form: &Form) -> f32 {
    let fields = form.fields().len() as f32;
    let toggles = form.switches().len() as f32;
    60.0 + fields * 64.0 + 6.0 + toggles * 36.0 + 62.0 + 24.0 + 36.0 + PAD + 4.0
}

// ---- text box ---------------------------------------------------------------

#[derive(Default, Clone)]
struct TextBox {
    text: Vec<char>,
    caret: usize,
    anchor: usize,
    scroll: f32,
    read_only: bool,
}

impl TextBox {
    fn with(s: &str) -> TextBox {
        let text: Vec<char> = s.chars().collect();
        let n = text.len();
        TextBox { text, caret: n, anchor: n, ..Default::default() }
    }
    fn value(&self) -> String {
        self.text.iter().collect()
    }
    fn sel(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }
    fn delete_sel(&mut self) -> bool {
        let (a, b) = self.sel();
        if a == b {
            return false;
        }
        self.text.drain(a..b);
        self.caret = a;
        self.anchor = a;
        true
    }
    fn insert(&mut self, s: &str) {
        if self.read_only {
            return;
        }
        self.delete_sel();
        for c in s.chars().filter(|c| !c.is_control()) {
            if self.text.len() >= 200 {
                break;
            }
            self.text.insert(self.caret, c);
            self.caret += 1;
        }
        self.anchor = self.caret;
    }
    fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && self.text[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && self.text[i - 1] != ' ' {
            i -= 1;
        }
        i
    }
    fn word_right(&self, mut i: usize) -> usize {
        let n = self.text.len();
        while i < n && self.text[i] != ' ' {
            i += 1;
        }
        while i < n && self.text[i] == ' ' {
            i += 1;
        }
        i
    }
    fn selected(&self) -> String {
        let (a, b) = self.sel();
        self.text[a..b].iter().collect()
    }
}

// ---- state ------------------------------------------------------------------

// Form focus order: text boxes 0..FIELDS, then the switches, then Cancel, then Add/Save.
const F_TOGGLE: usize = FIELDS;
const F_CANCEL: usize = FIELDS + 3;
const F_SUBMIT: usize = FIELDS + 4;

struct Form {
    edit: Option<String>,
    boxes: [TextBox; FIELDS],
    toggles: [bool; 3],
    focus: usize,
    error: String,
    /// Adding an extra Codex sign-in: name and note only, no email or reset.
    codex: bool,
}

impl Form {
    fn add_mode(&self) -> bool {
        self.edit.is_none()
    }
    /// The text boxes shown.
    fn fields(&self) -> Vec<usize> {
        if self.codex { vec![0, 2] } else { (0..FIELDS).collect() }
    }
    /// The switches shown: 0 startup, 1 keep running, 2 open now.
    fn switches(&self) -> Vec<usize> {
        match (self.codex, self.add_mode()) {
            (true, _) => vec![0, 2],
            (false, true) => vec![0, 1, 2],
            (false, false) => vec![0, 1],
        }
    }
    fn focusable(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.fields().into_iter().filter(|&k| k != 0 || self.add_mode()).collect();
        v.extend(self.switches().into_iter().map(|k| F_TOGGLE + k));
        v.extend([F_CANCEL, F_SUBMIT]);
        v
    }
}

/// The Settings screen's own state.
struct Options {
    /// The shortcut waiting for its new keys to be pressed (its KeyRow key).
    capturing: Option<String>,
    /// The tab showing: 0 General, 1 Usage, 2 Alerts, 3 Shortcuts (remembered).
    tab: u8,
    /// Number boxes: usage alert %, use-next %.
    boxes: [TextBox; 2],
    /// The number box being typed in.
    focus: Option<usize>,
    /// How far the rows are scrolled up (DIPs), when the window is shorter than the tallest tab.
    scroll: f32,
    /// A drag that scrolls: (pointer y, scroll at the press, moved far enough to count as a drag).
    drag: Option<(f32, f32, bool)>,
}

impl Options {
    fn new() -> Options {
        let (alert, next) = tray::number_settings();
        // When a setting is off its box still shows a number: the one turning it on would use.
        let shown = |v: u8, k: usize| if v == 0 { NUMBER_DEFAULTS[k] } else { v }.to_string();
        Options { capturing: None, tab: tray::settings_tab().min(3), boxes: [TextBox::with(&shown(alert, 0)), TextBox::with(&shown(next, 1))], focus: None, scroll: 0.0, drag: None }
    }

    /// The number in box `k` (its default when empty).
    fn number(&self, k: usize) -> u8 {
        let digits: String = self.boxes[k].value().chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() { NUMBER_DEFAULTS[k] } else { digits.parse::<u64>().unwrap_or(u64::MAX).clamp(1, 100) as u8 }
    }

    /// Finish typing in a number box: (which, value; 0 = off).
    fn commit(&mut self) -> Option<(usize, u8)> {
        let i = self.focus.take()?;
        let typed = self.boxes[i].value();
        if !typed.trim().chars().all(|c| c.is_ascii_digit()) {
            // Not a whole number (a pasted "9.5" or "-5"): keep the setting as it was.
            let (alert, next) = tray::number_settings();
            let v = [alert, next][i];
            self.boxes[i] = TextBox::with(&if v == 0 { NUMBER_DEFAULTS[i] } else { v }.to_string());
            return None;
        }
        let digits: String = self.boxes[i].value().chars().filter(|c| c.is_ascii_digit()).collect();
        // Typing a number turns the setting on. Emptied: back to the default. Too many digits: 100.
        let v = if digits.is_empty() { NUMBER_DEFAULTS[i] } else { digits.parse::<u64>().unwrap_or(u64::MAX).clamp(1, 100) as u8 };
        self.boxes[i] = TextBox::with(&v.to_string());
        Some((i, v))
    }
}

/// Usage alert and use-next numbers used when turned on with nothing typed.
const NUMBER_DEFAULTS: [u8; 2] = [90, 95];

thread_local! {
    /// A number typed on the Settings screen, saved once the event that ended the typing is handled.
    static NUMBER: std::cell::Cell<Option<(usize, u8)>> = const { std::cell::Cell::new(None) };
}

fn save_number(n: Option<(usize, u8)>) {
    if let Some((i, v)) = n {
        tray::update_settings(|c| if i == 0 { c.alert_percent = v } else { c.use_next_below = v });
    }
}

mod dash;
mod waiting;
pub use dash::{open as open_dashboard, open_view as open_dashboard_on};

enum View {
    Options(Box<Options>),
    List,
    Form(Box<Form>),
    Search(Box<Search>),
    Tidy(Box<Tidy>),
}

/// Tidy up: one account's finished chats, each ticked to be archived.
struct Tidy {
    account: String,
    items: Vec<(crate::claude::TidyChat, bool)>,
    held: crate::claude::TidyHeld,
    loading: bool,
    /// First row shown (the list scrolls with the mouse wheel).
    top: usize,
}

const TIDY_ROWS: usize = 8;
const TIDY_H: f32 = 64.0 + 48.0 + TIDY_ROWS as f32 * 48.0 + 130.0;
/// Most chats one Tidy up chat is asked to archive (its prompt stays short).
const TIDY_MAX: usize = 40;

impl Tidy {
    fn ticked(&self) -> Vec<crate::claude::TidyChat> {
        self.items.iter().filter(|(_, on)| *on).map(|(c, _)| c.clone()).take(TIDY_MAX).collect()
    }
}

const RESULTS: usize = 8;
const SEARCH_H: f32 = 64.0 + RESULTS as f32 * 48.0 + 40.0;

struct Search {
    query: TextBox,
    all: Vec<ChatItem>,
    results: Vec<usize>,
    sel: usize,
    /// Query the results were last filtered for: a new query selects the best match again.
    shown_for: String,
    /// The chat files are still being read (on a worker thread).
    loading: bool,
}

impl Search {
    /// Every word must appear in the title or the account name; newest first.
    fn refilter(&mut self) {
        let q = self.query.value().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        self.results = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let (t, a) = (c.title.to_lowercase(), c.account.to_lowercase());
                words.iter().all(|w| t.contains(w) || a.contains(w))
            })
            .map(|(i, _)| i)
            .take(RESULTS)
            .collect();
        if q != self.shown_for {
            self.sel = 0;
            self.shown_for = q;
        }
        self.sel = self.sel.min(self.results.len().saturating_sub(1));
    }
}

struct Gfx {
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    rt: Option<ID2D1DCRenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    /// The app's mark, made for the current render target.
    logo: Option<ID2D1Bitmap>,
    round: ID2D1StrokeStyle,
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    bold: IDWriteTextFormat,
    small: IDWriteTextFormat,
    tiny: IDWriteTextFormat,
    hint: IDWriteTextFormat,
    icon: IDWriteTextFormat,
    icon_small: IDWriteTextFormat,
    avatar: IDWriteTextFormat,
}

struct Fly {
    hwnd: HWND,
    gfx: Option<Gfx>,
    scale: f32,
    h: f32,
    dark: bool,
    view: View,
    snap: Snapshot,
    /// The Waiting list, loaded when the flyout opens or refreshes (never while painting).
    waiting: Vec<waiting::Item>,
    hits: Vec<(R, Hit)>,
    field_rects: [R; FIELDS],
    search_rect: R,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    /// Keyboard focus in the list: "Dismiss all", a waiting row, or an account row.
    focus: Option<waiting::Slot>,
    kb_used: bool,
    caret_on: bool,
    selecting: bool,
    hidden_at: Option<Instant>,
    tracking: bool,
    mode_pinned: Option<bool>,
}

thread_local! {
    static FLY: RefCell<Option<Fly>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Fly) -> R) -> Option<R> {
    FLY.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

const CLASS: &str = "HiveFlyout";
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_EXITSIZEMOVE_: u32 = 0x0232;
const WM_MOUSEWHEEL_: u32 = 0x020A;
const TIMER_CARET: usize = 1;
const HTCLIENT_: LRESULT = 1;
const HTCAPTION_: LRESULT = 2;

fn make_format(dw: &IDWriteFactory, family: windows::core::PCWSTR, size: f32, weight: DWRITE_FONT_WEIGHT, wrap: bool) -> windows::core::Result<IDWriteTextFormat> {
    unsafe {
        let f = dw.CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))?;
        if wrap {
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        } else {
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let sign = dw.CreateEllipsisTrimmingSign(&f)?;
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            f.SetTrimming(&trim, &sign)?;
        }
        Ok(f)
    }
}

fn make_gfx() -> windows::core::Result<Gfx> {
    unsafe {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let round = factory.CreateStrokeStyle(
            &D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
            },
            None,
        )?;
        let text = w!("Segoe UI Variable Text");
        let icons = w!("Segoe Fluent Icons");
        Ok(Gfx {
            title: make_format(&dwrite, w!("Segoe UI Variable Display"), 15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, false)?,
            body: make_format(&dwrite, text, 14.0, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            bold: make_format(&dwrite, text, 14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, false)?,
            small: make_format(&dwrite, text, 12.0, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            tiny: make_format(&dwrite, text, 11.0, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            hint: make_format(&dwrite, text, 12.0, DWRITE_FONT_WEIGHT_NORMAL, true)?,
            icon: make_format(&dwrite, icons, 16.0, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            icon_small: make_format(&dwrite, icons, 11.0, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            avatar: make_format(&dwrite, w!("Segoe UI Variable Display"), 14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, false)?,
            factory,
            dwrite,
            rt: None,
            logo: None,
            brush: None,
            round,
        })
    }
}

// ---- drawing helpers ----------------------------------------------------------

struct Painter<'a> {
    g: &'a Gfx,
    rt: &'a ID2D1RenderTarget,
    brush: &'a ID2D1SolidColorBrush,
}

/// The mark as 64x64 premultiplied BGRA (made from the logo PNG), as a bitmap for `rt`.
fn logo_bitmap(rt: &ID2D1RenderTarget) -> Option<ID2D1Bitmap> {
    static PIXELS: &[u8] = include_bytes!("../res/logo-64.bgra");
    let props = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    unsafe { rt.CreateBitmap(D2D_SIZE_U { width: 64, height: 64 }, Some(PIXELS.as_ptr() as *const _), 64 * 4, &props).ok() }
}

fn d2c(c: C) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.0, g: c.1, b: c.2, a: c.3 }
}

fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}

fn wtext(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

impl Painter<'_> {
    fn color(&self, c: C) {
        unsafe { self.brush.SetColor(&d2c(c)) };
    }
    fn fill(&self, r: R, radius: f32, c: C) {
        self.color(c);
        unsafe {
            if radius > 0.0 {
                self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r.d2(), radiusX: radius, radiusY: radius }, self.brush);
            } else {
                self.rt.FillRectangle(&r.d2(), self.brush);
            }
        }
    }
    fn stroke(&self, r: R, radius: f32, c: C, width: f32) {
        self.color(c);
        let h = width / 2.0;
        let rr = D2D1_ROUNDED_RECT { rect: r.inset(h, h).d2(), radiusX: radius, radiusY: radius };
        unsafe { self.rt.DrawRoundedRectangle(&rr, self.brush, width, None) };
    }
    fn circle(&self, x: f32, y: f32, rad: f32, c: C) {
        self.color(c);
        let e = D2D1_ELLIPSE { point: pt(x, y), radiusX: rad, radiusY: rad };
        unsafe { self.rt.FillEllipse(&e, self.brush) };
    }
    fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, c: C, width: f32) {
        self.color(c);
        unsafe { self.rt.DrawLine(pt(x0, y0), pt(x1, y1), self.brush, width, &self.g.round) };
    }
    fn text(&self, s: &str, f: &IDWriteTextFormat, r: R, c: C, align: DWRITE_TEXT_ALIGNMENT) {
        self.color(c);
        let t = wtext(s);
        unsafe {
            let _ = f.SetTextAlignment(align);
            self.rt.DrawText(&t, f, &r.d2(), self.brush, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
        }
    }
    /// Height of `s` wrapped to `w`.
    fn text_height(&self, s: &str, f: &IDWriteTextFormat, w: f32) -> f32 {
        let t = wtext(s);
        unsafe {
            let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            self.g
                .dwrite
                .CreateTextLayout(&t, f, w, 1000.0)
                .and_then(|l| {
                    let mut m = DWRITE_TEXT_METRICS::default();
                    l.GetMetrics(&mut m).map(|_| m.height)
                })
                .unwrap_or(18.0)
        }
    }

    fn width(&self, s: &str, f: &IDWriteTextFormat) -> f32 {
        let t = wtext(s);
        unsafe {
            let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            self.g
                .dwrite
                .CreateTextLayout(&t, f, 2000.0, 100.0)
                .and_then(|l| {
                    let mut m = DWRITE_TEXT_METRICS::default();
                    l.GetMetrics(&mut m).map(|_| m)
                })
                .map(|m| m.widthIncludingTrailingWhitespace)
                .unwrap_or(0.0)
        }
    }
    /// The app's mark (the faceted amber pinwheel), centered on (cx, cy).
    fn logo(&self, cx: f32, cy: f32, size: f32) {
        let Some(b) = &self.g.logo else { return };
        let h = size / 2.0;
        let r = D2D_RECT_F { left: cx - h, top: cy - h, right: cx + h, bottom: cy + h };
        unsafe { self.rt.DrawBitmap(b, Some(&r), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None) };
    }
}

// ---- the window ---------------------------------------------------------------

fn tray_anchor() -> RECT {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    let id = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: tray::main_hwnd(),
        uID: 1,
        guidItem: unsafe { std::mem::zeroed() },
    };
    if unsafe { Shell_NotifyIconGetRect(&id, &mut r) } != 0 || r.right <= r.left {
        let mut p = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut p) };
        r = RECT { left: p.x, top: p.y, right: p.x + 1, bottom: p.y + 1 };
    }
    r
}

fn monitor_info(mon: HMONITOR) -> (RECT, f32) {
    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (crate::win::usable_area(mi.rcWork), crate::win::debug_scale().unwrap_or(dx as f32 / 96.0))
    }
}

/// The drawing scale, shrunk when the window would be taller than the screen's work area.
fn fit_scale(h: f32, scale: f32, wk: &RECT) -> f32 {
    let avail = (wk.bottom - wk.top) as f32 - 24.0;
    if h * scale > avail && avail > 200.0 { avail / h } else { scale }
}

/// The Settings window's height (DIPs) on a work area `work_px` tall: the full height when it
/// fits, otherwise what fits at full scale (the rows scroll under the fixed tab strip).
fn settings_view_h(full: f32, scale: f32, work_px: f32) -> f32 {
    let avail = work_px - 24.0;
    if full * scale > avail && avail > 200.0 { (avail / scale).max(OPT_TOP + 120.0).min(full) } else { full }
}

/// The furthest the Settings rows scroll: the part of the tallest tab the window cannot show.
fn settings_max_scroll(full: f32, view: f32) -> f32 {
    (full - view).max(0.0)
}

/// A press starts a scroll drag only on the rows, never on a number box (dragging there selects digits).
fn drag_may_start(hit: Option<Hit>, y: f32, max_scroll: f32) -> bool {
    max_scroll > 0.0 && y >= OPT_VIEW_TOP && !matches!(hit, Some(Hit::Field(_) | Hit::SearchBox | Hit::Opt(OptHit::Number(_))))
}

/// Where the rows start on screen and where the visible rows end (the tab strip stays put).
const OPT_VIEW_TOP: f32 = 94.0;

/// The scale to draw at. Settings keeps `scale` and shortens the window instead (`f.h`), so its
/// rows scroll; every other screen shrinks to fit.
fn fit_for(f: &mut Fly, scale: f32, wk: &RECT) -> f32 {
    if matches!(f.view, View::Options(_)) {
        let full = f.h;
        f.h = settings_view_h(full, scale, (wk.bottom - wk.top) as f32);
        // A new size or scale can leave the old offset past the end.
        let max = settings_max_scroll(full, f.h);
        if let View::Options(o) = &mut f.view {
            o.scroll = o.scroll.clamp(0.0, max);
        }
        scale
    } else {
        fit_scale(f.h, scale, wk)
    }
}

/// Size and place the window: pinned, where you left it; otherwise next to the
/// tray icon, above (or below) the taskbar.
unsafe fn place(f: &mut Fly) {
    if IsIconic(f.hwnd) != 0 {
        return;
    }
    f.h = match &f.view {
        View::List => list_height(&f.snap, &f.waiting),
        View::Form(form) => form_height(form),
        View::Search(_) => SEARCH_H,
        View::Tidy(_) => TIDY_H,
        View::Options(_) => settings_height(&f.snap.opts),
    };
    if f.snap.pinned {
        if let Some([x, y]) = f.snap.pinned_at {
            let mon = MonitorFromPoint(POINT { x: x + 20, y: y + 20 }, MONITOR_DEFAULTTONULL);
            if !mon.is_null() {
                let (wk, scale) = monitor_info(mon);
                f.scale = fit_for(f, scale, &wk);
                let scale = f.scale;
                let (w, h) = ((W * scale).round() as i32, (f.h * scale).round() as i32);
                let y = y.min(wk.bottom - h).max(wk.top);
                set_pos(f.hwnd, null_mut(), x, y, w, h, SWP_NOACTIVATE | SWP_NOZORDER);
                return;
            }
        }
    }
    let a = tray_anchor();
    let (cx, cy) = ((a.left + a.right) / 2, (a.top + a.bottom) / 2);
    let (wk, scale) = monitor_info(MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST));
    f.scale = fit_for(f, scale, &wk);
    let scale = f.scale;
    let (w, h) = ((W * scale).round() as i32, (f.h * scale).round() as i32);
    let m = (12.0 * scale) as i32;
    let x = (cx - w / 2).clamp(wk.left + m, (wk.right - w - m).max(wk.left + m));
    let y = if cy >= wk.bottom {
        wk.bottom - h - m
    } else if cy <= wk.top {
        wk.top + m
    } else {
        (cy - h / 2).clamp(wk.top + m, (wk.bottom - h - m).max(wk.top + m))
    };
    let z = if f.snap.pinned { SWP_NOZORDER } else { 0 };
    set_pos(f.hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | z);
}

/// Move or resize the window. A showing window that changes size first waits for
/// the next screen refresh and drops its old pixels, so the `paint_now` that follows
/// lands in the same frame: no frame of the old drawing at the new size (the flicker
/// between Settings tabs).
unsafe fn set_pos(hwnd: HWND, after: HWND, x: i32, y: i32, w: i32, h: i32, flags: u32) {
    let mut r: RECT = std::mem::zeroed();
    GetWindowRect(hwnd, &mut r);
    let mut flags = flags;
    if (r.right - r.left != w || r.bottom - r.top != h) && IsWindowVisible(hwnd) != 0 {
        DwmFlush();
        flags |= SWP_NOCOPYBITS;
    }
    SetWindowPos(hwnd, after, x, y, w, h, flags);
}

/// Paint straight away instead of on a later WM_PAINT.
unsafe fn paint_now(hwnd: HWND) {
    RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_UPDATENOW);
}

/// Flyout (tool window, always on top, closes on click-away) or pinned
/// (normal window with a taskbar button that you can drag and minimize).
unsafe fn apply_mode(hwnd: HWND, pinned: bool) {
    let ex = if pinned { WS_EX_APPWINDOW } else { WS_EX_TOOLWINDOW | WS_EX_TOPMOST };
    let style = WS_POPUP | WS_CAPTION | if pinned { WS_MINIMIZEBOX } else { 0 };
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
    SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
    let after = if pinned { HWND_NOTOPMOST } else { HWND_TOPMOST };
    SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    // Changing the frame styles drops the backdrop: ask for it again.
    apply_glass(hwnd, is_dark());
}

/// Frosted glass: extend the frame over the whole window and ask DWM for the
/// acrylic ("transient window") backdrop with rounded corners.
unsafe fn apply_glass(hwnd: HWND, dark: bool) {
    let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
    DwmExtendFrameIntoClientArea(hwnd, &m);
    let backdrop: i32 = 3; // DWMSBT_TRANSIENTWINDOW
    DwmSetWindowAttribute(hwnd, 38, &backdrop as *const _ as *const _, 4);
    let corner: i32 = 2; // DWMWCP_ROUND
    DwmSetWindowAttribute(hwnd, 33, &corner as *const _ as *const _, 4);
    apply_theme(hwnd, dark);
}

unsafe fn apply_theme(hwnd: HWND, dark: bool) {
    let dark_val: i32 = dark as i32;
    DwmSetWindowAttribute(hwnd, 20, &dark_val as *const _ as *const _, 4); // DWMWA_USE_IMMERSIVE_DARK_MODE
}

unsafe fn create() -> Option<HWND> {
    let hinst = GetModuleHandleW(null());
    let class = wide(CLASS);
    let icon = LoadImageW(hinst, 1usize as *const u16, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE) as HICON;
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
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
    let title = wide(crate::APP_NAME);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST, class.as_ptr(), title.as_ptr(), WS_POPUP | WS_CAPTION,
        0, 0, 10, 10, null_mut(), null_mut(), hinst, null(),
    );
    if hwnd.is_null() {
        return None;
    }
    crate::taskbar::own_window(hwnd);
    BufferedPaintInit();
    let dark = is_dark();
    apply_glass(hwnd, dark);
    FLY.with(|c| {
        *c.borrow_mut() = Some(Fly {
            hwnd,
            gfx: None,
            scale: 1.0,
            h: 300.0,
            dark,
            view: View::List,
            snap: Snapshot::default(),
            waiting: Vec::new(),
            hits: Vec::new(),
            field_rects: [R::default(); FIELDS],
            search_rect: R::default(),
            hover: None,
            pressed: None,
            focus: None,
            kb_used: false,
            caret_on: true,
            selecting: false,
            hidden_at: None,
            tracking: false,
            mode_pinned: None,
        })
    });
    Some(hwnd)
}

fn hwnd() -> HWND {
    with(|f| f.hwnd).unwrap_or(null_mut())
}

pub fn visible() -> bool {
    let h = hwnd();
    !h.is_null() && unsafe { IsWindowVisible(h) } != 0
}

fn pinned() -> bool {
    with(|f| f.snap.pinned).unwrap_or(false)
}

fn show_view(view: View) {
    let typed = with(|f| match &mut f.view {
        View::Options(o) => o.commit(),
        _ => None,
    })
    .flatten();
    save_number(typed);
    unsafe {
        let h = if hwnd().is_null() { create() } else { Some(hwnd()) };
        let Some(h) = h else { return };
        let snap = tray::snapshot();
        let wait = if matches!(view, View::List) { waiting::current() } else { Vec::new() };
        let is_form = matches!(view, View::Form(_) | View::Search(_) | View::Options(_));
        let want_pinned = snap.pinned;
        let restyle = with(|f| f.mode_pinned != Some(want_pinned)).unwrap_or(true);
        if restyle {
            ShowWindow(h, SW_HIDE);
            apply_mode(h, want_pinned);
        }
        with(|f| {
            f.mode_pinned = Some(want_pinned);
            f.view = view;
            f.snap = snap;
            f.waiting = wait;
            f.hover = None;
            f.pressed = None;
            f.focus = None;
            f.kb_used = false;
            f.caret_on = true;
            place(f);
        });
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
            with(|f| place(f)); // place skips a minimized window: size it for the new view now
        } else {
            ShowWindow(h, SW_SHOW);
        }
        crate::win::bring_forward(h);
        if is_form {
            SetTimer(h, TIMER_CARET, 530, None);
        } else {
            KillTimer(h, TIMER_CARET);
        }
        paint_now(h);
    }
}

/// Tray icon click / hotkey: show the list, or hide it if it is showing.
pub fn toggle() {
    if visible() {
        if pinned() && unsafe { IsIconic(hwnd()) } != 0 {
            show_view(View::List);
        } else {
            hide();
        }
        return;
    }
    // A click on the tray icon first deactivates (hides) the flyout; don't reopen it.
    let just_hidden = with(|f| f.hidden_at.is_some_and(|t| t.elapsed() < Duration::from_millis(350))).unwrap_or(false);
    if !just_hidden {
        show_view(View::List);
    }
}

pub fn show() {
    show_view(View::List);
}

/// Open the Add form (None) or the Edit form for an account.
pub fn open_form(edit: Option<&str>) {
    let form = match edit {
        None => Form {
            edit: None,
            boxes: Default::default(),
            toggles: [true, false, true],
            focus: 0,
            error: String::new(),
            codex: false,
        },
        Some(name) => {
            let Some(a) = tray::account(name) else { return };
            let mut nb = TextBox::with(&a.name);
            nb.read_only = true;
            Form {
                edit: Some(a.name.clone()),
                boxes: [nb, TextBox::with(&a.email), TextBox::with(&a.purpose), TextBox::with(&a.weekly_reset)],
                toggles: [a.startup, a.keep_running, false],
                focus: 1,
                error: String::new(),
                codex: false,
            }
        }
    };
    show_view(View::Form(Box::new(form)));
}

/// The Add form for an extra Codex sign-in.
pub fn open_codex_form() {
    let form = Form { edit: None, boxes: Default::default(), toggles: [false, false, true], focus: 0, error: String::new(), codex: true };
    show_view(View::Form(Box::new(form)));
}

pub fn hide() {
    let h = hwnd();
    if h.is_null() {
        return;
    }
    unsafe {
        KillTimer(h, TIMER_CARET);
        ShowWindow(h, SW_HIDE);
    }
    let typed = with(|f| match &mut f.view {
        View::Options(o) => o.commit(),
        _ => None,
    })
    .flatten();
    save_number(typed);
    with(|f| {
        f.hidden_at = Some(Instant::now());
        f.view = View::List;
        // Free every drawing resource while hidden: idle memory stays near zero.
        f.gfx = None;
    });
}

/// New data (status, usage, settings): repaint if showing.
pub fn refresh() {
    if !visible() {
        return;
    }
    let snap = tray::snapshot();
    let wait = waiting::current();
    let h = with(|f| {
        f.snap = snap;
        f.waiting = wait;
        keep_focus_valid(f);
        unsafe { place(f) };
        f.hwnd
    });
    if let Some(h) = h {
        unsafe { paint_now(h) };
    }
}

pub fn theme_changed() {
    let dark = is_dark();
    with(|f| {
        f.dark = dark;
        unsafe {
            apply_theme(f.hwnd, dark);
            InvalidateRect(f.hwnd, null(), 0);
        }
    });
}

fn toggle_pin() {
    let now_pinned = !pinned();
    tray::set_pinned(now_pinned);
    show_view(View::List);
    if now_pinned {
        save_position();
    }
}

fn save_position() {
    let h = hwnd();
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if !h.is_null() && unsafe { GetWindowRect(h, &mut r) } != 0 {
        tray::set_pinned_at(r.left, r.top);
        with(|f| f.snap.pinned_at = Some([r.left, r.top]));
    }
}

// ---- painting ---------------------------------------------------------------

/// A card painted into a plain bitmap (premultiplied BGRA), for layout checks.
fn draw_card_pixels(v: &CardView, scale: f32, w: i32, hp: i32) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -hp, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
        let mut bits: *mut core::ffi::c_void = null_mut();
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, dib as _);
        let mut hits = Vec::new();
        draw_card(dc as _, &RECT { left: 0, top: 0, right: w, bottom: hp }, scale, v, &mut hits);
        let px = std::slice::from_raw_parts(bits as *const u8, (w * hp * 4) as usize).to_vec();
        SelectObject(dc, old);
        DeleteObject(dib as _);
        DeleteDC(dc);
        Some(px)
    }
}

/// Draw the Settings tabs (and the list) off screen at 100% and 150%, as raw
/// premultiplied BGRA files named `<what>-<scale>-<w>x<h>.bgra`. For checking
/// layouts (`--render <folder>`); nothing is shown.
pub fn render_preview(opts: Opts, dir: &std::path::Path) -> Vec<String> {
    use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
    let mut made = Vec::new();
    let _ = std::fs::create_dir_all(dir);
    let mut cases: Vec<(String, View, Opts, Option<Hit>)> = Vec::new();
    for tab in 0..4u8 {
        cases.push((format!("tab{tab}"), View::Options(Box::new(Options { tab, ..Options::new() })), opts.clone(), None));
    }
    // The hidden rows: Windows alert style, needs-me off.
    let mut quiet = opts.clone();
    quiet.alert_style = "windows".into();
    quiet.notify_waiting = false;
    cases.push(("tab2-hidden".into(), View::Options(Box::new(Options { tab: 2, ..Options::new() })), quiet, None));
    // An explainer open: the usage alert's (i).
    let tip = tab_rows(1, &opts).iter().position(|r| matches!(r, ORow::Number(..))).map(|n| Hit::Opt(OptHit::Info(n as u8)));
    cases.push(("tab1-tip".into(), View::Options(Box::new(Options { tab: 1, ..Options::new() })), opts.clone(), tip));
    // A short work area (720 px tall): Shortcuts scrolled to the top and to the bottom, General at the bottom.
    for (name, tab, scroll) in [("short-tab3-top", 3u8, 0.0f32), ("short-tab3-bottom", 3, 1.0e6), ("short-tab0-bottom", 0, 1.0e6)] {
        cases.push((name.into(), View::Options(Box::new(Options { tab, scroll, ..Options::new() })), opts.clone(), None));
    }
    cases.push(("list".into(), View::List, opts.clone(), None));
    // Keyboard focus on a waiting row (the second one: a card chat under its coordinator).
    cases.push(("list-focus".into(), View::List, opts.clone(), None));
    cases.push(("tidy".into(), View::Tidy(Box::new(sample_tidy())), opts.clone(), Some(Hit::TidyRow(1))));
    for scale in [1.0f32, 1.5] {
        for (name, view, o, hover) in &cases {
            let snap = crate::demo::snapshot(o.clone(), false, None);
            let demo_wait = if matches!(view, View::List) { waiting::demo() } else { Vec::new() };
            let h = match view {
                View::Options(_) if name.starts_with("short-") => settings_view_h(settings_height(&snap.opts), scale, 720.0),
                View::Options(_) => settings_height(&snap.opts),
                View::Tidy(_) => TIDY_H,
                _ => list_height(&snap, &demo_wait),
            };
            let view = match view {
                View::Options(op) => View::Options(Box::new(Options { tab: op.tab, scroll: op.scroll, ..Options::new() })),
                View::Tidy(_) => View::Tidy(Box::new(sample_tidy())),
                _ => View::List,
            };
            let mut fly = Fly {
                hwnd: null_mut(),
                gfx: None,
                scale,
                h,
                dark: true,
                view,
                snap,
                waiting: demo_wait.clone(),
                hits: Vec::new(),
                field_rects: [R::default(); FIELDS],
                search_rect: R::default(),
                hover: *hover,
                pressed: None,
                focus: (name == "list-focus").then(|| waiting::Slot::Chat(demo_wait[1].key.clone())),
                kb_used: name == "list-focus",
                caret_on: false,
                selecting: false,
                hidden_at: None,
                tracking: false,
                mode_pinned: None,
            };
            let (w, hp) = ((W * scale).round() as i32, (h * scale).round() as i32);
            unsafe {
                let dc = CreateCompatibleDC(null_mut());
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -hp, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
                let mut bits: *mut core::ffi::c_void = null_mut();
                let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
                if dib.is_null() || bits.is_null() {
                    DeleteDC(dc);
                    continue;
                }
                let old = SelectObject(dc, dib as _);
                fly.draw(dc as _, RECT { left: 0, top: 0, right: w, bottom: hp });
                let t = std::time::Instant::now();
                fly.draw(dc as _, RECT { left: 0, top: 0, right: w, bottom: hp });
                eprintln!("{name} at {}%: {:.1} ms to draw", (scale * 100.0) as u32, t.elapsed().as_secs_f64() * 1000.0);
                let px = std::slice::from_raw_parts(bits as *const u8, (w * hp * 4) as usize);
                let file = dir.join(format!("{name}-{}-{w}x{hp}.bgra", (scale * 100.0) as u32));
                if std::fs::write(&file, px).is_ok() {
                    made.push(file.display().to_string());
                }
                SelectObject(dc, old);
                DeleteObject(dib as _);
                DeleteDC(dc);
            }
        }
    }
    // A question card with both second-row buttons (the card paint code, off screen).
    for scale in [1.0f32, 1.5] {
        let text = "Which of the two layouts should I use for the settings page? A is denser, B has more room.";
        let h = card_height(text, true, true);
        let (w, hp) = ((CARD_W * scale).round() as i32, (h * scale).round() as i32);
        let v = CardView { title: "Work has a question", text, badge: Some(("W", 0xC9922E)), primary: Some("Open chat"), stay: 0, left: 0, hover: None, count: 1, snooze: true, mute: true };
        if let Some(px) = draw_card_pixels(&v, scale, w, hp) {
            let file = dir.join(format!("card-ask-{}-{w}x{hp}.bgra", (scale * 100.0) as u32));
            if std::fs::write(&file, px).is_ok() {
                made.push(file.display().to_string());
            }
        }
    }
    for (view, name) in [(0, "dashboard"), (1, "memory"), (2, "memory-review")] {
        for scale in [1.0f32, 1.5] {
            if let Some((w, h, px)) = dash::render_sample(scale, view) {
                let file = dir.join(format!("{name}-{}-{w}x{h}.bgra", (scale * 100.0) as u32));
                if std::fs::write(&file, px).is_ok() {
                    made.push(file.display().to_string());
                }
            }
        }
    }
    made
}

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let params = BP_PAINTPARAMS {
        cbSize: std::mem::size_of::<BP_PAINTPARAMS>() as u32,
        dwFlags: BPPF_ERASE,
        prcExclude: null(),
        pBlendFunction: null(),
    };
    let mut mem = null_mut();
    let pb = BeginBufferedPaint(hdc, &rc, BPBF_TOPDOWNDIB, &params, &mut mem);
    if pb as usize != 0 {
        with(|f| f.draw(mem, rc));
        EndBufferedPaint(pb, 1);
    }
    EndPaint(hwnd, &ps);
}

impl Fly {
    fn draw(&mut self, hdc: *mut core::ffi::c_void, rc: RECT) {
        if self.gfx.is_none() {
            self.gfx = make_gfx().ok();
        }
        let Some(g) = self.gfx.as_mut() else { return };
        unsafe {
            if g.rt.is_none() {
                let props = D2D1_RENDER_TARGET_PROPERTIES {
                    // A few hundred pixels: the CPU draws this instantly, and skipping the GPU
                    // keeps graphics-driver memory out of an always-running tray app.
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                    dpiX: 0.0,
                    dpiY: 0.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                };
                let Ok(rt) = g.factory.CreateDCRenderTarget(&props) else { return };
                let Ok(brush) = rt.CreateSolidColorBrush(&d2c(C(1.0, 1.0, 1.0, 1.0)), None) else { return };
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                g.logo = logo_bitmap(&rt);
                g.rt = Some(rt);
                g.brush = Some(brush);
            }
            let rt = g.rt.clone().unwrap();
            let wr = windows::Win32::Foundation::RECT { left: rc.left, top: rc.top, right: rc.right, bottom: rc.bottom };
            if rt.BindDC(windows::Win32::Graphics::Gdi::HDC(hdc), &wr).is_err() {
                return;
            }
            rt.SetDpi(96.0 * self.scale, 96.0 * self.scale);
            rt.BeginDraw();
            rt.Clear(Some(&d2c(C(0.0, 0.0, 0.0, 0.0))));
        }
        let g = self.gfx.as_ref().unwrap();
        let rt = g.rt.clone().unwrap();
        let brush = g.brush.clone().unwrap();
        let p = Painter { g, rt: &rt, brush: &brush };
        let t = theme(self.dark);
        let mut hits = Vec::new();
        p.fill(R::new(0.0, 0.0, W, self.h), 0.0, t.tint);
        match &self.view {
            View::List => self.draw_list(&p, &t, &mut hits),
            View::Form(form) => {
                let rects = draw_form(&p, &t, form, self.h, self.hover, self.caret_on, &mut hits);
                self.field_rects = rects;
            }
            View::Search(s) => {
                self.search_rect = draw_search(&p, &t, s, self.h, self.hover, self.caret_on, &mut hits);
            }
            View::Options(o) => draw_options(&p, &t, o, &self.snap.opts, self.h, self.hover, self.caret_on, &mut hits),
            View::Tidy(td) => draw_tidy(&p, &t, td, self.h, self.hover, &mut hits),
        }
        self.hits = hits;
        unsafe {
            if rt.EndDraw(None, None).is_err() {
                if let Some(g) = self.gfx.as_mut() {
                    g.rt = None;
                g.logo = None;
                    g.logo = None;
                    g.brush = None;
                }
            }
        }
    }

    fn draw_list(&self, p: &Painter, t: &Theme, hits: &mut Vec<(R, Hit)>) {
        let s = &self.snap;
        let claude_rows = s.rows.iter().filter(|r| r.kind == Kind::Claude);
        let open = claude_rows.clone().filter(|r| r.running).count();
        // header
        p.logo(PAD + 17.0, 31.0, 26.0);
        let title_r = W - PAD - 184.0;
        p.text(crate::APP_NAME, &p.g.title, R::new(PAD + 38.0, 12.0, title_r, 32.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
        // Short enough to fit beside the buttons: Drive is named only when it's a problem
        // (the waiting banner below already says when Claude is waiting for it).
        let mut sub = format!("{open} of {} open", claude_rows.count());
        if s.waiting.is_none() && s.drive == Some(false) {
            sub.push_str("  \u{00B7}  Drive missing");
        }
        p.text(&sub, &p.g.small, R::new(PAD + 38.0, 31.0, W - PAD - 172.0, 49.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
        let bx = |i: f32| R::new(W - PAD - 32.0 - i * 34.0, 15.0, W - PAD - i * 34.0, 47.0);
        icon_button(p, t, hits, self.hover, Hit::Search, bx(4.0), "\u{E721}", t.text);
        icon_button(p, t, hits, self.hover, Hit::Add, bx(3.0), "\u{E710}", t.text);
        icon_button(p, t, hits, self.hover, Hit::Settings, bx(2.0), "\u{E713}", t.text);
        let (pin_glyph, pin_color) = if s.pinned { ("\u{E77A}", t.accent) } else { ("\u{E718}", t.text) };
        icon_button(p, t, hits, self.hover, Hit::Pin, bx(1.0), pin_glyph, pin_color);
        icon_button(p, t, hits, self.hover, Hit::Close, bx(0.0), "\u{E711}", t.text);

        let mut y = 64.0;
        if let Some(path) = &s.waiting {
            let r = R::new(PAD, y, W - PAD, y + 50.0);
            p.fill(r, 8.0, C(t.accent.0, t.accent.1, t.accent.2, 0.16));
            p.text("\u{E823}", &p.g.icon, R::new(r.l + 12.0, r.t, r.l + 32.0, r.b), t.accent, DWRITE_TEXT_ALIGNMENT_CENTER);
            p.text("Waiting for Google Drive", &p.g.bold, R::new(r.l + 42.0, r.t + 6.0, r.r - 100.0, r.t + 26.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(path, &p.g.tiny, R::new(r.l + 42.0, r.t + 26.0, r.r - 100.0, r.t + 42.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            let b = R::new(r.r - 92.0, r.t + 10.0, r.r - 10.0, r.b - 10.0);
            button(p, t, hits, self.hover, Hit::SkipWait, b, "Open now", false);
            y += 58.0;
        }
        y += self.draw_waiting(p, t, hits, y);
        if s.rows.is_empty() {
            p.text("No accounts yet. Add one with +", &p.g.body, R::new(PAD, y, W - PAD, y + 56.0), t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
            y += 60.0;
        }
        for (i, row) in s.rows.iter().enumerate() {
            self.draw_row(p, t, hits, i, row, y);
            y += row_height(row) + 4.0;
        }
        // footer
        y += 4.0;
        p.line(PAD + 4.0, y, W - PAD - 4.0, y, t.border, 1.0);
        y += 6.0;
        let mid = if s.best.is_some() { PAD + (W - 2.0 * PAD) * 0.6 } else { W / 2.0 };
        match &s.best {
            Some(best) => {
                button(p, t, hits, self.hover, Hit::OpenBest, R::new(PAD, y, mid - 4.0, y + 34.0), &format!("Open {best}"), true);
                button(p, t, hits, self.hover, Hit::OpenAll, R::new(mid + 4.0, y, W - PAD, y + 34.0), "Open all", false);
            }
            None => button(p, t, hits, self.hover, Hit::OpenAll, R::new(PAD, y, W - PAD, y + 34.0), "Open all", true),
        }
        y += 40.0;
        let third = (W - 2.0 * PAD - 8.0) / 3.0;
        button(p, t, hits, self.hover, Hit::SaveLayout, R::new(PAD, y, PAD + third, y + 34.0), "Save layout", false);
        button(p, t, hits, self.hover, Hit::RestoreLayout, R::new(PAD + third + 4.0, y, PAD + 2.0 * third + 4.0, y + 34.0), "Restore layout", false);
        button(p, t, hits, self.hover, Hit::Dashboard, R::new(W - PAD - third, y, W - PAD, y + 34.0), "Dashboard", false);
    }

    /// The Waiting section; returns the height it took (0 when nothing waits). Paints from
    /// `self.waiting` only: no file reads here.
    fn draw_waiting(&self, p: &Painter, t: &Theme, hits: &mut Vec<(R, Hit)>, top: f32) -> f32 {
        if self.waiting.is_empty() {
            return 0.0;
        }
        let plan = waiting::plan(&self.waiting);
        let now = crate::time::now();
        let mut y = top;
        let count = self.waiting.len();
        p.text(&format!("Waiting for you  \u{00B7}  {count}"), &p.g.bold, R::new(PAD + 4.0, y, W - PAD - 100.0, y + WAIT_HEAD), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
        let dr = R::new(W - PAD - 92.0, y + 2.0, W - PAD, y + WAIT_HEAD - 2.0);
        if self.hover == Some(Hit::WaitDismiss) {
            p.fill(dr, 6.0, t.card_hover);
        }
        if self.kb_used && self.focus == Some(waiting::Slot::Dismiss) {
            p.stroke(dr, 6.0, t.accent, 2.0);
        }
        p.text("Dismiss all", &p.g.small, dr, t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
        hits.push((dr, Hit::WaitDismiss));
        y += WAIT_HEAD;
        for e in &plan.entries {
            match e {
                waiting::Entry::Group(account) => {
                    p.text(&format!("Card chats  \u{00B7}  {account}"), &p.g.tiny, R::new(PAD + 22.0, y, W - PAD, y + WAIT_GROUP), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
                    y += WAIT_GROUP;
                }
                waiting::Entry::Chat { idx, child } => {
                    let Some(w) = self.waiting.get(*idx) else { continue };
                    let indent = if *child { 16.0 } else { 0.0 };
                    let r = R::new(PAD + indent, y, W - PAD, y + WAIT_ROW);
                    let over_x = self.hover == Some(Hit::WaitX(*idx));
                    let hot = self.hover == Some(Hit::Wait(*idx)) || over_x;
                    let focused = self.kb_used && self.focus == Some(waiting::Slot::Chat(w.key.clone()));
                    p.fill(r, 8.0, if (hot && w.openable()) || focused || over_x { t.card_hover } else { t.card });
                    if focused {
                        p.stroke(r, 8.0, t.accent, 2.0);
                    }
                    // The per-row x shows on hover and on the focused row; the age steps left to make room.
                    let show_x = wait_x_shown(hot, focused);
                    let xr = R::new(r.r - 26.0, r.t + 4.0, r.r - 6.0, r.t + 24.0);
                    let age_r = if show_x { xr.l - 4.0 } else { r.r - 10.0 };
                    let col = self.snap.palette.iter().find(|(n, _)| *n == w.account).map_or_else(
                        || if w.account == "Codex" { CODEX } else { AVATARS[name_hash(&w.account) % AVATARS.len()] },
                        |(_, c)| *c,
                    );
                    let dim = w.quiet != waiting::Quiet::No;
                    let mut dot = hex(col, 1.0);
                    if dim {
                        dot.3 = 0.45;
                    }
                    p.circle(r.l + 14.0, r.t + WAIT_ROW / 2.0, 5.0, dot);
                    let tx = r.l + 28.0;
                    let age = waiting::waited(w.at, now);
                    let aw = p.width(&age, &p.g.tiny) + 4.0;
                    p.text(&age, &p.g.tiny, R::new(age_r - aw, r.t + 5.0, age_r, r.t + 21.0), t.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
                    let tc = if dim { t.text2 } else { t.text };
                    p.text(&w.title, &p.g.small, R::new(tx, r.t + 4.0, age_r - 6.0 - aw, r.t + 22.0), tc, DWRITE_TEXT_ALIGNMENT_LEADING);
                    let mut sub = format!("{}  \u{00B7}  {}", w.account, w.why.label());
                    match w.quiet {
                        waiting::Quiet::Snoozed => sub.push_str("  \u{00B7}  snoozed"),
                        waiting::Quiet::Muted => sub.push_str("  \u{00B7}  muted"),
                        waiting::Quiet::No => {}
                    }
                    let subc = match (w.why, dim) {
                        (_, true) => t.text3,
                        (waiting::Why::Done, _) => t.text3,
                        _ => t.amber,
                    };
                    p.text(&sub, &p.g.tiny, R::new(tx, r.t + 21.0, r.r - 10.0, r.t + 36.0), subc, DWRITE_TEXT_ALIGNMENT_LEADING);
                    if show_x {
                        if over_x {
                            p.fill(xr, 5.0, t.card);
                        }
                        p.text("\u{00D7}", &p.g.small, xr, if over_x { t.text } else { t.text2 }, DWRITE_TEXT_ALIGNMENT_CENTER);
                    }
                    // The x is tested before the row it sits on, but only while it is drawn: a tap on the
                    // age text of a row that is not hovered must open the chat, not dismiss it.
                    if show_x {
                        hits.push((xr, Hit::WaitX(*idx)));
                    }
                    if w.openable() {
                        hits.push((r, Hit::Wait(*idx)));
                    }
                    y += WAIT_ROW + 3.0;
                }
            }
        }
        if plan.more > 0 {
            p.text(&format!("{} more", plan.more), &p.g.tiny, R::new(PAD + 4.0, y, W - PAD, y + WAIT_MORE), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
            y += WAIT_MORE;
        }
        y + 10.0 - top
    }

    fn draw_row(&self, p: &Painter, t: &Theme, hits: &mut Vec<(R, Hit)>, i: usize, row: &Row, y: f32) {
        let r = R::new(PAD, y, W - PAD, y + row_height(row));
        let row_hot = matches!(self.hover, Some(Hit::Row(j) | Hit::More(j) | Hit::Chrome(j)) if j == i);
        let focused = self.kb_used && self.focus == Some(waiting::Slot::Row(i));
        p.fill(r, 8.0, if row_hot || focused { t.card_hover } else { t.card });
        if focused {
            p.stroke(r, 8.0, t.accent, 2.0);
        }
        let cy = r.t + 28.0;
        // avatar with status dot
        let ax = r.l + 28.0;
        let (label, base) = match row.kind {
            // An extra Codex sign-in wears its tag color, like its taskbar badge.
            Kind::Codex => (">_".to_string(), self.snap.palette.iter().find(|(n, _)| *n == row.name && n != "Codex").map_or(CODEX, |(_, c)| *c)),
            Kind::Claude => (initials(&row.name), self.snap.palette.iter().find(|(n, _)| *n == row.name).map_or(AVATARS[name_hash(&row.name) % AVATARS.len()], |(_, c)| *c)),
        };
        let mut col = hex(base, 1.0);
        if !row.running {
            col.3 = 0.45;
        }
        p.circle(ax, cy, 17.0, col);
        p.text(&label, &p.g.avatar, R::new(ax - 17.0, cy - 17.0, ax + 17.0, cy + 17.0), hex(0xFFFFFF, 1.0), DWRITE_TEXT_ALIGNMENT_CENTER);
        let dot = if !row.running { t.text3 } else if row.signed_out { t.amber } else { t.green };
        p.circle(ax + 12.5, cy + 12.5, 6.5, t.ring);
        p.circle(ax + 12.5, cy + 12.5, 4.5, dot);
        // name, status
        let tx = r.l + 58.0;
        let right = r.r - 84.0;
        let name_w = p.width(&row.name, &p.g.bold).min(right - tx);
        p.text(&row.name, &p.g.bold, R::new(tx, cy - 19.0, right, cy + 1.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
        if row.best {
            let cw = p.width("Use next", &p.g.tiny) + 12.0;
            let cr = R::new(tx + name_w + 8.0, cy - 16.0, tx + name_w + 8.0 + cw, cy - 1.0);
            if cr.r < r.r - 8.0 {
                p.fill(cr, 7.5, C(t.green.0, t.green.1, t.green.2, 0.18));
                p.text("Use next", &p.g.tiny, cr, t.green, DWRITE_TEXT_ALIGNMENT_CENTER);
            }
        }
        let sc = if !row.running { t.text3 } else if row.signed_out { t.amber } else { t.text2 };
        p.text(&row.status, &p.g.small, R::new(tx, cy + 1.0, right, cy + 19.0), sc, DWRITE_TEXT_ALIGNMENT_LEADING);
        // usage bars and reset line
        let mut ly = cy + 22.0;
        if !row.bars.is_empty() {
            let mut x = tx;
            for (k, (lab, pct)) in arrange_bars(&row.bars, &self.snap.opts.usage_bars).into_iter().enumerate() {
                // The first bar is the one that matters (weekly by default): big and clear.
                // The rest are a quiet "5h 12%".
                x = if k == 0 { usage_bar(p, t, x, ly + 8.0, lab, *pct) } else { usage_mini(p, t, x + 12.0, ly + 8.0, lab, *pct) };
            }
            if row.trend.len() >= 3 {
                let (x0, x1) = (x + 12.0, (r.r - 12.0).min(x + 76.0));
                if x1 - x0 >= 24.0 {
                    sparkline(p, t, &row.trend, R::new(x0, ly + 2.0, x1, ly + 15.0));
                }
            }
            ly += 18.0;
        }
        if let Some((note, tone)) = &row.note {
            let c = match tone {
                Tone::Normal => t.text3,
                Tone::Warn => t.amber,
                Tone::Bad => t.red,
            };
            p.text("\u{E72C}", &p.g.icon_small, R::new(tx, ly + 1.0, tx + 12.0, ly + 17.0), c, DWRITE_TEXT_ALIGNMENT_LEADING);
            p.text(note, &p.g.tiny, R::new(tx + 16.0, ly + 1.0, r.r - 10.0, ly + 17.0), c, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
        // buttons: more, and Claude in Chrome
        let mb = R::new(r.r - 40.0, cy - 16.0, r.r - 8.0, cy + 16.0);
        let cb = R::new(r.r - 74.0, cy - 16.0, r.r - 42.0, cy + 16.0);
        if row_hot || focused {
            icon_button(p, t, hits, self.hover, Hit::More(i), mb, "\u{E712}", t.text);
        }
        if row.kind == Kind::Claude && (row.chrome || row_hot) {
            let c = if row.chrome { t.accent } else { t.text3 };
            if self.hover == Some(Hit::Chrome(i)) {
                p.fill(cb, 6.0, t.card_hover);
            }
            p.text("\u{E774}", &p.g.icon, cb, c, DWRITE_TEXT_ALIGNMENT_CENTER);
            hits.push((cb, Hit::Chrome(i)));
        }
        hits.push((r, Hit::Row(i)));
    }
}

fn draw_form(p: &Painter, t: &Theme, form: &Form, h: f32, hover: Option<Hit>, caret_on: bool, hits: &mut Vec<(R, Hit)>) -> [R; FIELDS] {
    icon_button(p, t, hits, hover, Hit::Back, R::new(PAD, 14.0, PAD + 32.0, 46.0), "\u{E72B}", t.text);
    let title = match &form.edit {
        None if form.codex => "Add a Codex sign-in".to_string(),
        None => "Add account".to_string(),
        Some(n) => format!("Edit {n}"),
    };
    p.text(&title, &p.g.title, R::new(PAD + 42.0, 14.0, W - PAD - 40.0, 46.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
    icon_button(p, t, hits, hover, Hit::Close, R::new(W - PAD - 32.0, 14.0, W - PAD, 46.0), "\u{E711}", t.text);
    let labels = ["Name", "Sign-in email", "Note", "Weekly reset (optional)"];
    let placeholders = [if form.codex { "e.g. Codex Work" } else { "e.g. Work 2" }, "name@example.com", "What it's for", "From Settings > Usage, e.g. Tue 2:00 AM"];
    let mut rects = [R::default(); FIELDS];
    let mut y = 60.0;
    for k in form.fields() {
        p.text(labels[k], &p.g.small, R::new(PAD + 2.0, y, W - PAD, y + 18.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
        let r = R::new(PAD, y + 20.0, W - PAD, y + 54.0);
        rects[k] = r;
        draw_box(p, t, &form.boxes[k], r, form.focus == k, caret_on, placeholders[k]);
        if !form.boxes[k].read_only {
            hits.push((r, Hit::Field(k)));
        }
        y += 64.0;
    }
    y += 6.0;
    let names = ["Open when Windows starts", "Keep running (reopen if it closes)", "Open it now"];
    for k in form.switches() {
        let r = R::new(PAD, y, W - PAD, y + 34.0);
        if hover == Some(Hit::Toggle(k)) {
            p.fill(r, 6.0, t.card);
        }
        if form.focus == F_TOGGLE + k {
            p.stroke(r, 6.0, t.accent, 2.0);
        }
        p.text(names[k], &p.g.body, R::new(r.l + 8.0, r.t, r.r - 60.0, r.b), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
        switch(p, t, R::new(r.r - 50.0, r.t + 7.0, r.r - 10.0, r.b - 7.0), form.toggles[k]);
        hits.push((r, Hit::Toggle(k)));
        y += 36.0;
    }
    let hint = if form.codex {
        "It gets its own Codex window, sign-in and Codex folder, next to your main Codex (which is not touched). Sign in to the other account in the new window."
    } else {
        "Each account gets its own window and sign-in. Sign in with Continue with email and the emailed code: Google sign-in always lands in the Personal window."
    };
    p.text(
        hint,
        &p.g.hint, R::new(PAD + 2.0, y + 6.0, W - PAD, y + 60.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING,
    );
    y += 62.0;
    if !form.error.is_empty() {
        p.text(&form.error, &p.g.small, R::new(PAD + 2.0, y, W - PAD, y + 20.0), t.red, DWRITE_TEXT_ALIGNMENT_LEADING);
    }
    let by = h - PAD - 36.0;
    let cancel = R::new(W - PAD - 212.0, by, W - PAD - 108.0, by + 34.0);
    let submit = R::new(W - PAD - 100.0, by, W - PAD, by + 34.0);
    button(p, t, hits, hover, Hit::Cancel, cancel, "Cancel", false);
    button(p, t, hits, hover, Hit::Submit, submit, if form.add_mode() { "Add" } else { "Save" }, true);
    if form.focus == F_CANCEL {
        p.stroke(cancel.inset(-2.0, -2.0), 7.0, t.accent, 2.0);
    }
    if form.focus == F_SUBMIT {
        p.stroke(submit.inset(-2.0, -2.0), 7.0, t.text, 2.0);
    }
    rects
}

fn draw_search(p: &Painter, t: &Theme, s: &Search, h: f32, hover: Option<Hit>, caret_on: bool, hits: &mut Vec<(R, Hit)>) -> R {
    icon_button(p, t, hits, hover, Hit::Back, R::new(PAD, 14.0, PAD + 32.0, 46.0), "\u{E72B}", t.text);
    let r = R::new(PAD + 40.0, 13.0, W - PAD, 47.0);
    draw_box(p, t, &s.query, r, true, caret_on, "Search chats in every account");
    hits.push((r, Hit::SearchBox));
    let mut y = 60.0;
    if s.results.is_empty() {
        let msg = if s.loading { "Reading chats..." } else if s.all.is_empty() { "No Code chats found" } else { "No chats match" };
        p.text(msg, &p.g.body, R::new(PAD, y + 10.0, W - PAD, y + 40.0), t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
    }
    for (k, &i) in s.results.iter().enumerate() {
        let c = &s.all[i];
        let rr = R::new(PAD, y, W - PAD, y + 44.0);
        let hot = hover == Some(Hit::Result(k)) || s.sel == k;
        p.fill(rr, 6.0, if hot { t.card_hover } else { t.card });
        p.text(&c.title, &p.g.body, R::new(rr.l + 12.0, rr.t + 3.0, rr.r - 10.0, rr.t + 23.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
        let meta = format!("{}  \u{00B7}  {}{}", c.account, crate::time::ago(c.last), if c.archived { "  \u{00B7}  archived" } else { "" });
        p.text(&meta, &p.g.small, R::new(rr.l + 12.0, rr.t + 23.0, rr.r - 10.0, rr.t + 40.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
        hits.push((rr, Hit::Result(k)));
        y += 48.0;
    }
    p.text("Enter opens the chat. Right-click to continue it in another account.", &p.g.tiny, R::new(PAD + 2.0, h - PAD - 20.0, W - PAD, h - PAD), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
    r
}

/// Weekly % over the last 7 days as a small line.
fn sparkline(p: &Painter, t: &Theme, v: &[u8], r: R) {
    let n = v.len();
    let at = |i: usize| (r.l + (r.r - r.l) * i as f32 / (n - 1) as f32, r.b - (r.b - r.t) * v[i] as f32 / 100.0);
    p.fill(R::new(r.l, r.b - 0.5, r.r, r.b + 0.5), 0.0, t.track);
    for i in 1..n {
        let ((x0, y0), (x1, y1)) = (at(i - 1), at(i));
        p.line(x0, y0, x1, y1, t.text3, 1.3);
    }
    let last = v[n - 1];
    let c = if last >= 90 { t.red } else if last >= 70 { t.amber } else { t.text2 };
    let (x, y) = at(n - 1);
    p.circle(x, y, 2.2, c);
}

fn draw_box(p: &Painter, t: &Theme, b: &TextBox, r: R, focused: bool, caret_on: bool, placeholder: &str) {
    p.fill(r, 5.0, if b.read_only { t.card } else { t.input });
    p.stroke(r, 5.0, t.border, 1.0);
    if focused {
        p.line(r.l + 3.0, r.b - 1.0, r.r - 3.0, r.b - 1.0, t.accent, 2.0);
    }
    let inner = r.inset(10.0, 0.0);
    let col = if b.read_only { t.text2 } else { t.text };
    if b.text.is_empty() {
        if !focused {
            p.text(placeholder, &p.g.body, inner, t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        }
    } else {
        let Some(layout) = layout_for(p.g, b) else { return };
        unsafe {
            p.rt.PushAxisAlignedClip(&inner.d2(), D2D1_ANTIALIAS_MODE_ALIASED);
            let (a, z) = b.sel();
            if focused && a != z {
                let x0 = caret_x(&layout, a);
                let x1 = caret_x(&layout, z);
                p.fill(R::new(inner.l + x0 - b.scroll, r.t + 7.0, inner.l + x1 - b.scroll, r.b - 7.0), 2.0, C(t.accent.0, t.accent.1, t.accent.2, 0.35));
            }
            p.color(col);
            let top = r.t + (r.b - r.t - 20.0) / 2.0;
            p.rt.DrawTextLayout(pt(inner.l - b.scroll, top), &layout, p.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            p.rt.PopAxisAlignedClip();
        }
    }
    if focused && caret_on && !b.read_only {
        let x = if b.text.is_empty() { 0.0 } else { layout_for(p.g, b).map_or(0.0, |l| caret_x(&l, b.caret)) };
        let cx = inner.l + x - b.scroll;
        p.line(cx, r.t + 8.0, cx, r.b - 8.0, t.text, 1.2);
    }
}

fn layout_for(g: &Gfx, b: &TextBox) -> Option<IDWriteTextLayout> {
    let s: String = b.text.iter().collect();
    let w = wtext(&s);
    unsafe {
        let _ = g.body.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
        let l = g.dwrite.CreateTextLayout(&w, &g.body, 4000.0, 20.0).ok()?;
        let _ = l.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        Some(l)
    }
}

/// x of the caret before char index `i` (UTF-16 positions match chars for BMP text).
fn caret_x(l: &IDWriteTextLayout, i: usize) -> f32 {
    let (mut x, mut y) = (0.0f32, 0.0f32);
    let mut m = DWRITE_HIT_TEST_METRICS::default();
    unsafe {
        let _ = l.HitTestTextPosition(i as u32, WBOOL(0), &mut x, &mut y, &mut m);
    }
    x
}

fn icon_button(p: &Painter, t: &Theme, hits: &mut Vec<(R, Hit)>, hover: Option<Hit>, id: Hit, r: R, glyph: &str, color: C) {
    if hover == Some(id) {
        p.fill(r, 6.0, if id == Hit::Close { C(t.red.0, t.red.1, t.red.2, 0.22) } else { t.card_hover });
    }
    p.text(glyph, &p.g.icon, r, color, DWRITE_TEXT_ALIGNMENT_CENTER);
    hits.push((r, id));
}

fn button(p: &Painter, t: &Theme, hits: &mut Vec<(R, Hit)>, hover: Option<Hit>, id: Hit, r: R, label: &str, accent: bool) {
    let hot = hover == Some(id);
    let inner = r.inset(10.0, 0.0);
    if accent {
        let mut c = t.accent;
        if hot {
            c = C((c.0 * 1.08).min(1.0), (c.1 * 1.08).min(1.0), (c.2 * 1.08).min(1.0), 1.0);
        }
        p.fill(r, 6.0, c);
        p.text(label, &p.g.bold, inner, t.on_accent, DWRITE_TEXT_ALIGNMENT_CENTER);
    } else {
        p.fill(r, 6.0, if hot { t.card_hover } else { t.card });
        p.stroke(r, 6.0, t.border, 1.0);
        p.text(label, &p.g.body, inner, t.text, DWRITE_TEXT_ALIGNMENT_CENTER);
    }
    hits.push((r, id));
}

fn switch(p: &Painter, t: &Theme, r: R, on: bool) {
    let rad = (r.b - r.t) / 2.0;
    if on {
        p.fill(r, rad, t.accent);
        p.circle(r.r - rad, r.t + rad, rad - 4.0, t.on_accent);
    } else {
        p.stroke(r, rad, t.text2, 1.0);
        p.circle(r.l + rad, r.t + rad, rad - 5.0, t.text2);
    }
}

/// A labelled bar, e.g. 5h [bar] 54%. Returns the x where it ends.
fn usage_bar(p: &Painter, t: &Theme, x: f32, y: f32, label: &str, pct: Option<u8>) -> f32 {
    let lw = p.width(label, &p.g.tiny);
    p.text(label, &p.g.tiny, R::new(x, y - 8.0, x + lw + 2.0, y + 8.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
    let bx = x + lw + 6.0;
    let bw = 96.0;
    p.fill(R::new(bx, y - 3.0, bx + bw, y + 3.0), 3.0, t.track);
    let (txt, c) = match pct {
        Some(v) => {
            let c = if v >= 90 { t.red } else if v >= 70 { t.amber } else { t.green };
            if v > 0 {
                p.fill(R::new(bx, y - 3.0, bx + bw * v as f32 / 100.0, y + 3.0), 3.0, c);
            }
            (format!("{v}%"), if v >= 70 { c } else { t.text })
        }
        None => ("reset".to_string(), t.text3),
    };
    let tw = p.width(&txt, &p.g.small);
    p.text(&txt, &p.g.small, R::new(bx + bw + 6.0, y - 9.0, bx + bw + 8.0 + tw, y + 9.0), c, DWRITE_TEXT_ALIGNMENT_LEADING);
    bx + bw + 8.0 + tw
}

/// A secondary usage figure as quiet text, e.g. "5h 12%" (red once it is nearly used up).
fn usage_mini(p: &Painter, t: &Theme, x: f32, y: f32, label: &str, pct: Option<u8>) -> f32 {
    let txt = match pct {
        Some(v) => format!("{label} {v}%"),
        None => format!("{label} reset"),
    };
    let c = if pct.is_some_and(|v| v >= 90) { t.red } else { t.text3 };
    let tw = p.width(&txt, &p.g.tiny);
    p.text(&txt, &p.g.tiny, R::new(x, y - 8.0, x + tw + 2.0, y + 8.0), c, DWRITE_TEXT_ALIGNMENT_LEADING);
    x + tw + 2.0
}

/// The account's avatar color (0xRRGGBB), also used for its window name tag.
/// Every account's color, in list order: the one its name hashes to, or the
/// next free one, so no two accounts share a color (up to six).
pub fn avatar_palette<'a>(names: impl Iterator<Item = &'a str>) -> Vec<(String, u32)> {
    let mut used = [false; AVATARS.len()];
    names
        .map(|n| {
            let mut i = name_hash(n) % AVATARS.len();
            for _ in 0..AVATARS.len() {
                if !used[i] {
                    break;
                }
                i = (i + 1) % AVATARS.len();
            }
            used[i] = true;
            (n.to_string(), AVATARS[i])
        })
        .collect()
}

fn name_hash(s: &str) -> usize {
    s.bytes().fold(7usize, |h, b| h.wrapping_mul(31).wrapping_add(b.to_ascii_lowercase() as usize))
}

fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let mut s: String = words.iter().take(2).filter_map(|w| w.chars().next()).collect();
    if s.is_empty() {
        s.push('?');
    }
    s.to_uppercase()
}

// ---- input --------------------------------------------------------------------

fn to_dip(f: &Fly, lp: LPARAM) -> (f32, f32) {
    let x = (lp & 0xffff) as i16 as f32 / f.scale;
    let y = ((lp >> 16) & 0xffff) as i16 as f32 / f.scale;
    (x, y)
}

fn hit_at(f: &Fly, lp: LPARAM) -> (f32, f32, Option<Hit>) {
    let (x, y) = to_dip(f, lp);
    (x, y, f.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h))
}

fn key_down(k: u16) -> bool {
    unsafe { GetKeyState(k as i32) < 0 }
}

/// Things to do after the state borrow is released (they may run modal loops).
enum After {
    Nothing,
    Open(String),
    OpenCodex(String),
    OpenAll,
    SaveLayout,
    RestoreLayout,
    More(String, i32, i32),
    CodexMore(String, i32, i32),
    Chrome(String),
    SkipWait,
    Pin,
    Form(Option<String>),
    List,
    Hide,
    Submit,
    Search(String),
    OpenChat(String, String, String),
    /// Right-clicked search result: (account, id, cli, title).
    ChatMenu(String, String, String, String),
    Options,
    Opt(OptHit),
    /// A shortcut's new keys: (KeyRow key, combo; "" clears it).
    SetKey(String, String),
    /// Ask this account's Claude to archive these chats.
    Tidy(String, Vec<crate::claude::TidyChat>),
    Dashboard,
    /// A Waiting row: (account, chat id, (dismissal key, its event time), title).
    OpenWaiting(String, String, (String, u64), String),
    DismissWaiting,
    /// One waiting row cleared (account, chat id, (key, event time)).
    DismissWaitingOne(String, String, (String, u64)),
}

fn client_to_screen(f: &Fly, r: R) -> (i32, i32) {
    let mut p = POINT { x: (r.l * f.scale) as i32, y: (r.b * f.scale) as i32 };
    unsafe { ClientToScreen(f.hwnd, &mut p) };
    (p.x, p.y)
}

/// Keys on the list view. Focus runs through "Dismiss all", the waiting rows and the account rows
/// (`waiting::slots`). Tab and Down go forward, Shift+Tab and Up back, both wrapping. Enter or
/// Space acts like a click on the focused control; Delete dismisses just the focused waiting row.
fn list_key(f: &mut Fly, vk: u16, shift: bool) -> After {
    let order = waiting::slots(&f.waiting, &waiting::plan(&f.waiting), f.snap.rows.len());
    match vk {
        VK_ESCAPE => After::Hide,
        VK_TAB | VK_DOWN | VK_UP => {
            let back = vk == VK_UP || (vk == VK_TAB && shift);
            f.focus = waiting::step(&order, f.focus.as_ref(), back);
            After::Nothing
        }
        VK_RETURN | VK_SPACE => match f.focus.clone() {
            Some(waiting::Slot::Dismiss) => click(f, Hit::WaitDismiss),
            Some(waiting::Slot::Row(i)) => click(f, Hit::Row(i)),
            Some(slot @ waiting::Slot::Chat(_)) => match waiting::index_of(&f.waiting, &slot) {
                Some(i) => click(f, Hit::Wait(i)),
                None => After::Nothing,
            },
            None => After::Nothing,
        },
        VK_DELETE => match f.focus.clone() {
            Some(slot @ waiting::Slot::Chat(_)) => {
                let Some(i) = waiting::index_of(&f.waiting, &slot) else { return After::Nothing };
                let key = f.waiting[i].key.clone();
                f.focus = waiting::focus_after_dismiss(&order, &key, f.snap.rows.len());
                click(f, Hit::WaitX(i))
            }
            _ => After::Nothing,
        },
        _ => After::Nothing,
    }
}

/// After the list reloads: a focus whose row is gone (dismissed all, opened elsewhere) moves to the first account row.
fn keep_focus_valid(f: &mut Fly) {
    if let Some(cur) = &f.focus {
        let order = waiting::slots(&f.waiting, &waiting::plan(&f.waiting), f.snap.rows.len());
        if !order.contains(cur) {
            f.focus = order.into_iter().find(|s| matches!(s, waiting::Slot::Row(_)));
        }
    }
}

fn click(f: &mut Fly, hit: Hit) -> After {
    let rect = f.hits.iter().find(|(_, h)| *h == hit).map(|(r, _)| *r).unwrap_or_default();
    let row = |i: usize| f.snap.rows.get(i).map(|r| (r.kind, r.name.clone()));
    match hit {
        Hit::Row(i) => match row(i) {
            Some((Kind::Codex, n)) => After::OpenCodex(n),
            Some((Kind::Claude, n)) => After::Open(n),
            None => After::Nothing,
        },
        Hit::More(i) => {
            let (x, y) = client_to_screen(f, rect);
            match row(i) {
                Some((Kind::Codex, n)) => After::CodexMore(n, x, y),
                Some((Kind::Claude, n)) => After::More(n, x, y),
                None => After::Nothing,
            }
        }
        Hit::Chrome(i) => row(i).map_or(After::Nothing, |(_, n)| After::Chrome(n)),
        Hit::OpenAll => After::OpenAll,
        Hit::SaveLayout => After::SaveLayout,
        Hit::Dashboard => After::Dashboard,
        Hit::Wait(i) => f.waiting.get(i).filter(|w| w.openable()).map_or(After::Nothing, |w| After::OpenWaiting(w.account.clone(), w.chat.clone(), (w.key.clone(), w.at), w.title.clone())),
        Hit::WaitDismiss => After::DismissWaiting,
        Hit::WaitX(i) => f.waiting.get(i).map_or(After::Nothing, |w| After::DismissWaitingOne(w.account.clone(), w.chat.clone(), (w.key.clone(), w.at))),
        Hit::RestoreLayout => After::RestoreLayout,
        Hit::OpenBest => f.snap.best.clone().map_or(After::Nothing, After::Open),
        Hit::Add => After::Form(None),
        Hit::Settings => After::Options,
        Hit::Opt(OptHit::Number(i)) => {
            if let View::Options(o) = &mut f.view {
                o.capturing = None;
                if o.focus != Some(i) {
                    NUMBER.with(|c| c.set(o.commit()));
                    o.focus = Some(i);
                    let n = o.boxes[i].text.len();
                    o.boxes[i].anchor = 0; // select it all: typing replaces the number
                    o.boxes[i].caret = n;
                }
            }
            f.caret_on = true;
            After::Nothing
        }
        Hit::Opt(o) => {
            if let View::Options(opts) = &mut f.view {
                NUMBER.with(|c| c.set(opts.commit()));
            }
            After::Opt(o)
        }
        Hit::Pin => After::Pin,
        Hit::Close => After::Hide,
        Hit::SkipWait => After::SkipWait,
        Hit::Back | Hit::Cancel => {
            if let View::Options(o) = &mut f.view {
                NUMBER.with(|c| c.set(o.commit()));
            }
            After::List
        }
        Hit::Submit => After::Submit,
        Hit::Field(k) => {
            if let View::Form(form) = &mut f.view {
                form.focus = k;
            }
            After::Nothing
        }
        Hit::Toggle(k) => {
            if let View::Form(form) = &mut f.view {
                form.toggles[k] = !form.toggles[k];
                form.focus = F_TOGGLE + k;
            }
            After::Nothing
        }
        Hit::Search => After::Search(String::new()),
        Hit::SearchBox => After::Nothing,
        Hit::Result(k) => match &f.view {
            View::Search(s) => open_result(s, k),
            _ => After::Nothing,
        },
        Hit::TidyRow(i) => {
            if let View::Tidy(td) = &mut f.view {
                if let Some((_, on)) = td.items.get_mut(i) {
                    *on = !*on;
                }
            }
            After::Nothing
        }
        Hit::TidyAll => {
            if let View::Tidy(td) = &mut f.view {
                let all = td.items.iter().all(|(_, on)| *on);
                td.items.iter_mut().for_each(|(_, on)| *on = !all);
            }
            After::Nothing
        }
        Hit::TidyGo => match &f.view {
            View::Tidy(td) if !td.ticked().is_empty() => After::Tidy(td.account.clone(), td.ticked()),
            _ => After::Nothing,
        },
    }
}

/// Sample Tidy up list for `--render`.
fn sample_tidy() -> Tidy {
    let now = crate::time::now();
    let chat = |n: u64, title: &str, detail: &str| crate::claude::TidyChat { id: format!("local_{n}"), title: title.into(), last: now - n * 86_400, detail: detail.into() };
    let items = vec![
        (chat(21, "Crash report analysis", "3 issues found, fixes listed"), true),
        (chat(14, "MCP and skills recommendations", "startup message turned off; backup saved"), false),
        (chat(9, "Weekday news digest for the team", "digest written for Sept 23"), true),
        (chat(6, "Day ahead preplan", "4 tasks checked, 3 routine-owned"), true),
        (chat(5, "Weekly status notes draft", "summary written, two open items"), true),
        (chat(4, "Slow page load", "cause found, fix listed"), true),
        (chat(4, "Wire the server game loop end to end", "21 checks ok"), true),
        (chat(3, "Quarterly budget review", ""), true),
        (chat(3, "Old coordinator handoff", "handed off"), true),
    ];
    Tidy { account: "Team".into(), items, held: crate::claude::TidyHeld { not_done: 42, recent: 28, starred: 4, with_pr: 8, waiting: 1 }, loading: false, top: 0 }
}

/// Tidy up an account's chats: the review list (read on a worker thread; WM_TIDY brings it).
pub fn open_tidy(account: &str) {
    show_view(View::Tidy(Box::new(Tidy { account: account.to_string(), items: Vec::new(), held: Default::default(), loading: true, top: 0 })));
    let h = hwnd();
    if !h.is_null() {
        tray::read_tidy(account, h as usize, WM_TIDY);
    }
}

const WM_TIDY: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 9;

fn tidy_arrived(hwnd: HWND) {
    let Some((account, chats, held)) = tray::take_tidy() else { return };
    let changed = with(|f| match &mut f.view {
        View::Tidy(td) if td.account == account => {
            td.items = chats.into_iter().map(|c| (c, true)).collect();
            td.held = held;
            td.loading = false;
            td.top = 0;
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    if changed {
        invalidate(hwnd);
    }
}

fn draw_tidy(p: &Painter, t: &Theme, td: &Tidy, h: f32, hover: Option<Hit>, hits: &mut Vec<(R, Hit)>) {
    icon_button(p, t, hits, hover, Hit::Back, R::new(PAD, 14.0, PAD + 32.0, 46.0), "\u{E72B}", t.text);
    p.text(&format!("Tidy up \u{00B7} {}", td.account), &p.g.title, R::new(PAD + 42.0, 14.0, W - PAD - 40.0, 46.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
    icon_button(p, t, hits, hover, Hit::Close, R::new(W - PAD - 32.0, 14.0, W - PAD, 46.0), "\u{E711}", t.text);
    let intro = format!(
        "Chats Claude marked finished, idle {}+ days. Untick any to keep. Claude archives the rest in a new chat and keeps back anything still running or with unsaved work.",
        crate::claude::TIDY_IDLE_DAYS
    );
    p.text(&intro, &p.g.hint, R::new(PAD + 2.0, 54.0, W - PAD, 106.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
    let mut y = 112.0;
    if td.items.is_empty() {
        let msg = if td.loading { format!("Reading {}'s chats...", td.account) } else { format!("Nothing to tidy in {}.", td.account) };
        p.text(&msg, &p.g.body, R::new(PAD, y + 10.0, W - PAD, y + 40.0), t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
    }
    for (i, (c, on)) in td.items.iter().enumerate().skip(td.top).take(TIDY_ROWS) {
        let rr = R::new(PAD, y, W - PAD, y + 44.0);
        let hot = hover == Some(Hit::TidyRow(i));
        p.fill(rr, 6.0, if hot { t.card_hover } else { t.card });
        // The tick box.
        let b = R::new(rr.l + 10.0, rr.t + 12.0, rr.l + 30.0, rr.t + 32.0);
        if *on {
            p.fill(b, 4.0, t.accent);
            p.text("\u{E73E}", &p.g.icon_small, b, t.on_accent, DWRITE_TEXT_ALIGNMENT_CENTER);
        } else {
            p.stroke(b, 4.0, t.text3, 1.2);
        }
        let col = if *on { t.text } else { t.text2 };
        p.text(&c.title, &p.g.body, R::new(rr.l + 40.0, rr.t + 3.0, rr.r - 10.0, rr.t + 23.0), col, DWRITE_TEXT_ALIGNMENT_LEADING);
        let meta = if c.detail.is_empty() { crate::time::ago(c.last) } else { format!("{}  \u{00B7}  {}", crate::time::ago(c.last), c.detail) };
        p.text(&meta, &p.g.small, R::new(rr.l + 40.0, rr.t + 23.0, rr.r - 10.0, rr.t + 40.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
        hits.push((rr, Hit::TidyRow(i)));
        y += 48.0;
    }
    // Footer: what wasn't offered, then the buttons.
    let fy = h - PAD - 36.0;
    let mut notes = Vec::new();
    if td.items.len() > TIDY_ROWS {
        notes.push(format!("{}-{} of {}, scroll for more", td.top + 1, (td.top + TIDY_ROWS).min(td.items.len()), td.items.len()));
    }
    let hd = &td.held;
    let held: Vec<String> = [(hd.not_done, "unfinished"), (hd.recent, "recent"), (hd.with_pr, "with a PR"), (hd.starred, "starred"), (hd.waiting, "waiting on you")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    if !held.is_empty() && !td.loading {
        notes.push(format!("Not offered: {}.", held.join(", ")));
    }
    p.text(&notes.join("
"), &p.g.hint, R::new(PAD + 2.0, fy - 64.0, W - PAD, fy - 4.0), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
    let n = td.ticked().len();
    let all = !td.items.is_empty() && td.items.iter().all(|(_, on)| *on);
    if !td.items.is_empty() {
        button(p, t, hits, hover, Hit::TidyAll, R::new(PAD, fy, PAD + 110.0, fy + 34.0), if all { "Untick all" } else { "Tick all" }, false);
    }
    let picked = td.items.iter().filter(|(_, on)| *on).count();
    let label = match n {
        0 => "Archive in Claude".to_string(),
        _ if picked > n => format!("Archive {n} of {picked} in Claude"),
        _ => format!("Archive {n} in Claude"),
    };
    if n > 0 {
        button(p, t, hits, hover, Hit::TidyGo, R::new(PAD + 118.0, fy, W - PAD, fy + 34.0), &label, true);
    } else {
        let r = R::new(PAD + 118.0, fy, W - PAD, fy + 34.0);
        p.fill(r, 6.0, t.card);
        p.text(&label, &p.g.body, r.inset(10.0, 0.0), t.text3, DWRITE_TEXT_ALIGNMENT_CENTER);
    }
}

fn open_result(s: &Search, k: usize) -> After {
    s.results.get(k).map_or(After::Nothing, |&i| {
        let c = &s.all[i];
        After::OpenChat(c.account.clone(), c.id.clone(), c.title.clone())
    })
}

/// Search Code-tab chats across every account. Shows the last index at once
/// and reads the chat files again on a worker thread (WM_CHATS brings them).
pub fn open_search(q: &str) {
    let cached = tray::cached_chats();
    let loading = cached.is_none();
    let mut s = Search { query: TextBox::with(q), all: cached.unwrap_or_default(), results: Vec::new(), sel: 0, shown_for: String::new(), loading };
    s.refilter();
    show_view(View::Search(Box::new(s)));
    let h = hwnd();
    if !h.is_null() {
        tray::read_chats(h as usize, WM_CHATS);
    }
}

/// The worker finished reading chats: use them if search is still open.
const WM_CHATS: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 8;

fn chats_arrived(hwnd: HWND) {
    let Some(all) = tray::cached_chats() else { return };
    let changed = with(|f| match &mut f.view {
        View::Search(s) => {
            s.all = all;
            s.loading = false;
            s.refilter();
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    if changed {
        invalidate(hwnd);
    }
}

/// After opening something: a flyout gets out of the way, a pinned window stays.
fn done() {
    if !pinned() {
        hide();
    }
}

fn run_after(a: After) {
    if let Some(n) = NUMBER.with(|c| c.take()) {
        save_number(Some(n));
        refresh();
    }
    match a {
        After::Nothing => {}
        After::Open(n) => {
            tray::open_account(&n);
            done();
        }
        After::OpenCodex(n) => {
            tray::open_codex_named(&n);
            done();
        }
        After::OpenAll => {
            tray::open_all();
            done();
        }
        After::SaveLayout => tray::save_layout(),
        After::RestoreLayout => tray::restore_layout(),
        After::More(n, x, y) => tray::account_menu(hwnd(), &n, x, y),
        After::CodexMore(n, x, y) => tray::codex_menu(hwnd(), x, y, &n),
        After::Chrome(n) => {
            tray::chrome_here(&n);
            refresh();
        }
        After::SkipWait => {
            tray::skip_wait();
            refresh();
        }
        After::Pin => toggle_pin(),
        After::Form(n) => open_form(n.as_deref()),
        After::List => show_view(View::List),
        After::Hide => hide(),
        After::Submit => submit(),
        After::Search(q) => open_search(&q),
        After::Options => show_view(View::Options(Box::new(Options::new()))),
        After::Opt(o) => {
            apply_opt(o);
            refresh();
        }
        After::SetKey(key, combo) => {
            tray::set_hotkey(&key, &combo);
            refresh();
        }
        After::ChatMenu(account, id, cli, title) => {
            let mut p = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut p) };
            match tray::chat_menu(hwnd(), &account, &cli, p.x, p.y) {
                Some(tray::ChatChoice::Open) => run_after(After::OpenChat(account, id, title)),
                Some(tray::ChatChoice::ContinueIn(target)) => {
                    tray::continue_in(&target, &cli, &title);
                    if pinned() {
                        show_view(View::List);
                    } else {
                        hide();
                    }
                }
                None => {}
            }
        }
        After::Dashboard => {
            if !pinned() {
                hide();
            }
            dash::open();
        }
        After::Tidy(account, chats) => {
            tray::tidy_archive(&account, &chats);
            if pinned() {
                show_view(View::List);
            } else {
                hide();
            }
        }
        After::OpenWaiting(account, chat, entry, title) => {
            // The same path a card click takes, then the chat leaves the list and its card closes.
            tray::card_action(crate::card::Action::Chat(account.clone(), chat.clone(), title));
            crate::card::close_chat(&account, &chat);
            waiting::dismiss(&[entry]);
            if pinned() {
                refresh();
            } else {
                hide();
            }
        }
        After::DismissWaiting => {
            let all = with(|f| f.waiting.clone()).unwrap_or_default();
            for w in &all {
                crate::card::close_chat(&w.account, &w.chat);
            }
            waiting::dismiss(&all.iter().map(|w| (w.key.clone(), w.at)).collect::<Vec<_>>());
            refresh();
        }
        After::DismissWaitingOne(account, chat, entry) => {
            // Same effect as Dismiss all for one chat: its card closes, nothing is opened.
            crate::card::close_chat(&account, &chat);
            waiting::dismiss(&[entry]);
            refresh();
        }
        After::OpenChat(account, id, title) => {
            tray::open_chat(&account, &id, &title);
            if pinned() {
                show_view(View::List);
            } else {
                hide();
            }
        }
    }
}

fn submit() {
    let Some((edit, vals, toggles, codex)) = with(|f| match &f.view {
        View::Form(form) => Some((form.edit.clone(), form.boxes.iter().map(|b| b.value()).collect::<Vec<_>>(), form.toggles, form.codex)),
        View::List | View::Search(_) | View::Options(_) | View::Tidy(_) => None,
    })
    .flatten() else {
        return;
    };
    let (name, email, note, reset) = (&vals[0], vals[1].trim(), &vals[2], vals[3].trim());
    let result = if codex {
        tray::add_codex_account(name, note, toggles[0], toggles[2])
    } else if !email.is_empty() && (!email.contains('@') || email.contains(' ')) {
        Err("That email doesn't look right. Leave it empty or fix it.".to_string())
    } else if !reset.is_empty() && crate::time::next_weekly(reset).is_none() {
        Err("Weekly reset needs a day and time, like Tue 2:00 AM.".to_string())
    } else {
        match &edit {
            None => tray::add_account(name, email, note, reset, toggles[0], toggles[1], toggles[2]),
            Some(n) => tray::edit_account(n, email, note, reset, toggles[0], toggles[1]),
        }
    };
    match result {
        Ok(()) if edit.is_none() && toggles[2] => done(), // the new window is opening
        Ok(()) => show_view(View::List),
        Err(e) => {
            with(|f| {
                if let View::Form(form) = &mut f.view {
                    form.error = e;
                }
                unsafe { InvalidateRect(f.hwnd, null(), 0) };
            });
        }
    }
}

/// Keyboard in the form. Returns an action to run after the borrow.
fn form_key(f: &mut Fly, vk: u16) -> After {
    let View::Form(form) = &mut f.view else { return After::Nothing };
    let shift = key_down(VK_SHIFT);
    let ctrl = key_down(VK_CONTROL);
    let order = form.focusable();
    let is_toggle = (F_TOGGLE..F_CANCEL).contains(&form.focus);
    match vk {
        VK_TAB => {
            let pos = order.iter().position(|&x| x == form.focus).unwrap_or(0);
            let n = order.len();
            form.focus = order[if shift { (pos + n - 1) % n } else { (pos + 1) % n }];
            return After::Nothing;
        }
        VK_ESCAPE => return After::List,
        VK_RETURN => return if form.focus == F_CANCEL { After::List } else { After::Submit },
        VK_SPACE if is_toggle => {
            let k = form.focus - F_TOGGLE;
            form.toggles[k] = !form.toggles[k];
            return After::Nothing;
        }
        VK_SPACE if form.focus == F_CANCEL => return After::List,
        VK_SPACE if form.focus == F_SUBMIT => return After::Submit,
        _ => {}
    }
    if form.focus >= FIELDS {
        return After::Nothing;
    }
    let hwnd = f.hwnd;
    edit_box(&mut form.boxes[form.focus], vk, ctrl, shift, hwnd);
    After::Nothing
}

/// Keyboard in the chat search.
fn search_key(f: &mut Fly, vk: u16) -> After {
    let hwnd = f.hwnd;
    let View::Search(s) = &mut f.view else { return After::Nothing };
    let n = s.results.len();
    match vk {
        VK_ESCAPE => return After::List,
        VK_DOWN => {
            if n > 0 {
                s.sel = (s.sel + 1) % n;
            }
            return After::Nothing;
        }
        VK_UP => {
            if n > 0 {
                s.sel = (s.sel + n - 1) % n;
            }
            return After::Nothing;
        }
        VK_RETURN => return open_result(s, s.sel),
        _ => {}
    }
    edit_box(&mut s.query, vk, key_down(VK_CONTROL), key_down(VK_SHIFT), hwnd);
    s.refilter();
    After::Nothing
}

/// Caret movement, deletion and clipboard keys for a text box.
fn edit_box(b: &mut TextBox, vk: u16, ctrl: bool, shift: bool, hwnd: HWND) {
    let n = b.text.len();
    match vk {
        VK_LEFT => {
            let to = if ctrl { b.word_left(b.caret) } else if b.caret != b.anchor && !shift { b.sel().0 } else { b.caret.saturating_sub(1) };
            b.caret = to;
            if !shift {
                b.anchor = to;
            }
        }
        VK_RIGHT => {
            let to = if ctrl { b.word_right(b.caret) } else if b.caret != b.anchor && !shift { b.sel().1 } else { (b.caret + 1).min(n) };
            b.caret = to;
            if !shift {
                b.anchor = to;
            }
        }
        VK_HOME | VK_UP => {
            b.caret = 0;
            if !shift {
                b.anchor = 0;
            }
        }
        VK_END | VK_DOWN => {
            b.caret = n;
            if !shift {
                b.anchor = n;
            }
        }
        VK_DELETE if !b.read_only => {
            if !b.delete_sel() && b.caret < n {
                let end = if ctrl { b.word_right(b.caret) } else { b.caret + 1 };
                b.text.drain(b.caret..end);
            }
        }
        VK_BACK if !b.read_only => {
            if !b.delete_sel() && b.caret > 0 {
                let start = if ctrl { b.word_left(b.caret) } else { b.caret - 1 };
                b.text.drain(start..b.caret);
                b.caret = start;
                b.anchor = start;
            }
        }
        0x41 if ctrl => {
            b.anchor = 0;
            b.caret = n;
        }
        0x43 if ctrl => {
            let s = b.selected();
            if !s.is_empty() {
                set_clipboard(hwnd, &s);
            }
        }
        0x58 if ctrl && !b.read_only => {
            let s = b.selected();
            if !s.is_empty() && set_clipboard(hwnd, &s) {
                b.delete_sel();
            }
        }
        0x56 if ctrl => {
            if let Some(s) = get_clipboard(hwnd) {
                b.insert(s.lines().next().unwrap_or("").trim());
            }
        }
        _ => {}
    }
}

/// Keep the caret of the focused box inside its visible area.
fn keep_caret_visible(f: &mut Fly) {
    let Some(g) = f.gfx.as_ref() else { return };
    match &mut f.view {
        View::Form(form) if form.focus < FIELDS => scroll_box(g, &mut form.boxes[form.focus], f.field_rects[form.focus]),
        View::Search(s) => scroll_box(g, &mut s.query, f.search_rect),
        _ => {}
    }
}

fn scroll_box(g: &Gfx, b: &mut TextBox, r: R) {
    if r.r - r.l < 40.0 {
        return; // not drawn yet: nothing to measure against
    }
    let inner_w = (r.r - r.l - 20.0).max(10.0);
    let x = if b.text.is_empty() { 0.0 } else { layout_for(g, b).map_or(0.0, |l| caret_x(&l, b.caret)) };
    if x - b.scroll > inner_w - 2.0 {
        b.scroll = x - inner_w + 2.0;
    } else if x < b.scroll {
        b.scroll = x;
    }
    if b.scroll < 0.0 {
        b.scroll = 0.0;
    }
}

/// Caret index under x in a form box (Some(k)) or the search box (None).
fn caret_from_point(f: &Fly, k: Option<usize>, x: f32) -> Option<usize> {
    let g = f.gfx.as_ref()?;
    let (b, r) = match (&f.view, k) {
        (View::Form(form), Some(k)) => (form.boxes.get(k)?, *f.field_rects.get(k)?),
        (View::Search(s), None) => (&s.query, f.search_rect),
        _ => return None,
    };
    if b.text.is_empty() {
        return Some(0);
    }
    let l = layout_for(g, b)?;
    let lx = x - (r.l + 10.0) + b.scroll;
    let mut trailing = WBOOL(0);
    let mut inside = WBOOL(0);
    let mut m = DWRITE_HIT_TEST_METRICS::default();
    unsafe { l.HitTestPoint(lx, 10.0, &mut trailing, &mut inside, &mut m).ok()? };
    Some((m.textPosition as usize + trailing.0 as usize).min(b.text.len()))
}

fn invalidate(hwnd: HWND) {
    unsafe { InvalidateRect(hwnd, null(), 0) };
}

/// The per-row x of the waiting list exists (drawn and clickable) only on the hovered or focused row.
fn wait_x_shown(hot: bool, focused: bool) -> bool {
    hot || focused
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wp != 0 => 0, // the whole window is client area
        WM_NCHITTEST => {
            // Drag the window by the empty part of the header (letting go pins it there).
            let drag = with(|f| {
                if !matches!(f.view, View::List) {
                    return false;
                }
                let mut p = POINT { x: (lp & 0xffff) as i16 as i32, y: ((lp >> 16) & 0xffff) as i16 as i32 };
                ScreenToClient(hwnd, &mut p);
                let (x, y) = (p.x as f32 / f.scale, p.y as f32 / f.scale);
                y < HEADER && !f.hits.iter().any(|(r, _)| r.contains(x, y))
            })
            .unwrap_or(false);
            if drag { HTCAPTION_ } else { HTCLIENT_ }
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_SYSKEYDOWN | WM_SYSKEYUP if with(|f| matches!(&f.view, View::Options(o) if o.capturing.is_some())).unwrap_or(false) => {
            // Alt combinations arrive as system keys: record them instead of opening the window menu.
            if msg == WM_SYSKEYDOWN {
                let after = with(|f| options_key(f, wp as u16)).unwrap_or(After::Nothing);
                invalidate(hwnd);
                run_after(after);
            }
            0
        }
        WM_ACTIVATE => {
            if (wp & 0xffff) == 0 {
                // Lost focus: the list closes like a Windows flyout; a pinned window
                // or a half-filled form stays.
                let close = with(|f| matches!(f.view, View::List | View::Search(_)) && !f.snap.pinned).unwrap_or(true);
                if close {
                    hide();
                }
            }
            0
        }
        WM_EXITSIZEMOVE_ => {
            save_position();
            if !pinned() {
                toggle_pin(); // you moved it: it stays where you put it
            }
            0
        }
        WM_MOUSEMOVE => {
            let mut repaint = false;
            let mut drag: Option<(Option<usize>, f32)> = None;
            with(|f| {
                if !f.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut tme);
                    f.tracking = true;
                }
                let (x, y, hit) = hit_at(f, lp);
                let max = settings_max_scroll(settings_height(&f.snap.opts), f.h);
                if let View::Options(o) = &mut f.view {
                    if wp & 1 == 0 {
                        // No button held (capture was lost mid-drag): the drag is over.
                        o.drag = None;
                    }
                    if let (Some((y0, s0, moved)), true) = (o.drag, max > 0.0) {
                        if moved || (y - y0).abs() > 4.0 {
                            o.scroll = (s0 - (y - y0)).clamp(0.0, max);
                            o.drag = Some((y0, s0, true));
                            repaint = true;
                        }
                    }
                }
                if f.selecting {
                    match &f.view {
                        View::Form(form) if form.focus < form.boxes.len() => drag = Some((Some(form.focus), x)),
                        View::Form(_) => {}
                        View::Search(_) => drag = Some((None, x)),
                        View::List | View::Options(_) | View::Tidy(_) => {}
                    }
                }
                if hit != f.hover {
                    f.hover = hit;
                    repaint = true;
                }
            });
            if let Some((k, x)) = drag {
                with(|f| {
                    if let Some(pos) = caret_from_point(f, k, x) {
                        match (&mut f.view, k) {
                            (View::Form(form), Some(k)) => form.boxes[k].caret = pos,
                            (View::Search(s), None) => s.query.caret = pos,
                            _ => {}
                        }
                        keep_caret_visible(f);
                    }
                });
                repaint = true;
            }
            if repaint {
                invalidate(hwnd);
            }
            0
        }
        WM_MOUSELEAVE => {
            with(|f| {
                f.tracking = false;
                f.hover = None;
            });
            invalidate(hwnd);
            0
        }
        WM_SETCURSOR => {
            let over_field = with(|f| matches!(f.hover, Some(Hit::Field(_) | Hit::SearchBox))).unwrap_or(false);
            SetCursor(LoadCursorW(null_mut(), if over_field { IDC_IBEAM } else { IDC_ARROW }));
            1
        }
        WM_LBUTTONDOWN => {
            SetCapture(hwnd);
            with(|f| {
                let (x, y, hit) = hit_at(f, lp);
                f.pressed = hit;
                f.kb_used = false;
                let max = settings_max_scroll(settings_height(&f.snap.opts), f.h);
                if let View::Options(o) = &mut f.view {
                    o.drag = if drag_may_start(hit, y, max) { Some((y, o.scroll, false)) } else { None };
                }
                let target = match hit {
                    Some(Hit::Field(k)) => Some(Some(k)),
                    Some(Hit::SearchBox) => Some(None),
                    _ => None,
                };
                if let Some(k) = target {
                    if let Some(pos) = caret_from_point(f, k, x) {
                        let b = match (&mut f.view, k) {
                            (View::Form(form), Some(k)) => {
                                form.focus = k;
                                Some(&mut form.boxes[k])
                            }
                            (View::Search(s), None) => Some(&mut s.query),
                            _ => None,
                        };
                        if let Some(b) = b {
                            b.caret = pos;
                            if !key_down(VK_SHIFT) {
                                b.anchor = pos;
                            }
                        }
                    }
                    f.selecting = true;
                    f.caret_on = true;
                }
            });
            invalidate(hwnd);
            0
        }
        WM_RBUTTONUP => {
            let after = with(|f| match (hit_at(f, lp).2, &f.view) {
                (Some(Hit::Result(k)), View::Search(s)) => s.results.get(k).map_or(After::Nothing, |&i| {
                    let c = &s.all[i];
                    After::ChatMenu(c.account.clone(), c.id.clone(), c.cli.clone(), c.title.clone())
                }),
                _ => After::Nothing,
            })
            .unwrap_or(After::Nothing);
            run_after(after);
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let after = with(|f| {
                f.selecting = false;
                let (_, _, hit) = hit_at(f, lp);
                let mut pressed = f.pressed.take();
                if let View::Options(o) = &mut f.view {
                    // A drag that scrolled is not a click on whatever it ended over.
                    if matches!(o.drag.take(), Some((_, _, true))) {
                        pressed = None;
                    }
                }
                match (hit, pressed) {
                    (Some(h), Some(p)) if h == p => click(f, h),
                    _ => After::Nothing,
                }
            })
            .unwrap_or(After::Nothing);
            invalidate(hwnd);
            run_after(after);
            0
        }
        WM_KEYDOWN => {
            let vk = wp as u16;
            // Holding Delete must clear one row per press, not one per auto-repeat (bit 30 = was down).
            if vk == VK_DELETE && (lp >> 30) & 1 == 1 && with(|f| matches!(f.view, View::List)).unwrap_or(false) {
                return 0;
            }
            let after = with(|f| {
                f.kb_used = true;
                f.caret_on = true;
                match &f.view {
                    View::Form(_) => {
                        let a = form_key(f, vk);
                        keep_caret_visible(f);
                        a
                    }
                    View::Search(_) => {
                        let a = search_key(f, vk);
                        keep_caret_visible(f);
                        a
                    }
                    View::Options(_) => options_key(f, vk),
                    View::Tidy(_) => match vk {
                        VK_ESCAPE => After::List,
                        VK_RETURN => click(f, Hit::TidyGo),
                        _ => After::Nothing,
                    },
                    View::List => list_key(f, vk, key_down(VK_SHIFT)),
                }
            })
            .unwrap_or(After::Nothing);
            invalidate(hwnd);
            run_after(after);
            0
        }
        WM_CHATS => {
            chats_arrived(hwnd);
            0
        }
        WM_TIDY => {
            tidy_arrived(hwnd);
            0
        }
        WM_MOUSEWHEEL_ => {
            // Scroll the Tidy up list (three rows a notch) or the Settings rows (48 DIPs a notch).
            let notches = ((wp >> 16) & 0xffff) as i16 as i32 / 120;
            let moved = with(|f| match &mut f.view {
                View::Tidy(td) => {
                    let max = td.items.len().saturating_sub(TIDY_ROWS) as i32;
                    let top = (td.top as i32 - notches * 3).clamp(0, max) as usize;
                    let changed = top != td.top;
                    td.top = top;
                    changed
                }
                View::Options(_) => {
                    let max = settings_max_scroll(settings_height(&f.snap.opts), f.h);
                    let View::Options(o) = &mut f.view else { return false };
                    let to = (o.scroll - notches as f32 * 48.0).clamp(0.0, max);
                    let changed = to != o.scroll;
                    o.scroll = to;
                    changed
                }
                _ => false,
            })
            .unwrap_or(false);
            if moved {
                with(|f| f.hover = None);
                invalidate(hwnd);
            }
            0
        }
        WM_CHAR => {
            let c = wp as u32;
            let after = with(|f| {
                let ch = char::from_u32(c).filter(|_| c >= 0x20 && c != 0x7f);
                match &mut f.view {
                    View::Form(form) => {
                        if let (true, Some(ch)) = (form.focus < FIELDS, ch) {
                            form.boxes[form.focus].insert(&ch.to_string());
                        }
                    }
                    View::Search(s) => {
                        if let Some(ch) = ch {
                            s.query.insert(&ch.to_string());
                            s.refilter();
                        }
                    }
                    View::Options(o) => {
                        if let (Some(i), Some(ch)) = (o.focus, ch.filter(|c| c.is_ascii_digit())) {
                            if o.boxes[i].value().len() < 3 || o.boxes[i].sel().0 != o.boxes[i].sel().1 {
                                o.boxes[i].insert(&ch.to_string());
                            }
                        }
                    }
                    View::Tidy(_) => {}
                    // Typing in the list starts a chat search.
                    View::List => {
                        if let Some(ch) = ch.filter(|c| !c.is_whitespace()) {
                            return After::Search(ch.to_string());
                        }
                    }
                }
                keep_caret_visible(f);
                After::Nothing
            })
            .unwrap_or(After::Nothing);
            invalidate(hwnd);
            run_after(after);
            0
        }
        WM_TIMER if wp == TIMER_CARET => {
            with(|f| f.caret_on = !f.caret_on);
            invalidate(hwnd);
            0
        }
        WM_DPICHANGED => {
            with(|f| place(f));
            invalidate(hwnd);
            0
        }
        WM_SETTINGCHANGE => {
            theme_changed();
            0
        }
        WM_CLOSE => {
            hide();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- Settings screen ----------------------------------------------------------

const OPT_HEAD: f32 = 28.0;
const OPT_ROW: f32 = 38.0;
const KEY_ROW: f32 = 46.0;
/// Where a tab's rows start: under the title and the tab strip.
const OPT_TOP: f32 = 102.0;
/// The tools row (Check setup, Edit file, Help, Exit app), on the General tab.
const OPT_TOOLS: f32 = 56.0;
const TABS: [&str; 4] = ["General", "Usage", "Alerts", "Shortcuts"];

/// One row of a Settings tab. The last field of a row is its (i) explainer.
enum ORow {
    Head(&'static str, Option<&'static str>),
    Switch(&'static str, Sw, bool, Option<&'static str>),
    Chips(&'static str, Group, Vec<String>, Option<usize>, Option<&'static str>),
    /// (label, box, its on/off group, value; 0 = off, explainer)
    Number(&'static str, usize, Group, u8, Option<&'static str>),
    /// The Drive folder under "Wait for Google Drive first".
    Drive,
    Key(usize),
    /// Choices too many for one line: two chips per line, right aligned.
    Grid(&'static str, Group, Vec<String>, Option<usize>),
}

impl ORow {
    fn height(&self) -> f32 {
        match self {
            ORow::Head(..) => OPT_HEAD,
            ORow::Drive => 30.0,
            ORow::Key(_) => KEY_ROW,
            ORow::Grid(_, _, names, _) => OPT_ROW + 30.0 * (names.len().div_ceil(2).saturating_sub(1)) as f32,
            _ => OPT_ROW,
        }
    }
}

/// What a Settings tab shows. Options that don't apply right now stay hidden.
fn tab_rows(tab: u8, v: &Opts) -> Vec<ORow> {
    let names = |c: &[(&'static str, &'static str)]| c.iter().map(|c| c.0.to_string()).collect::<Vec<String>>();
    let mut r = Vec::new();
    match tab {
        0 => {
            r.push(ORow::Head("STARTUP", None));
            r.push(ORow::Switch("Start with Windows", Sw::Start, v.start_with_windows, None));
            r.push(ORow::Switch("Wait for Google Drive first", Sw::Drive, v.wait_drive, None));
            if v.wait_drive {
                r.push(ORow::Drive);
            }
            r.push(ORow::Head("WINDOWS", None));
            r.push(ORow::Switch("Name tag on each window", Sw::Tags, v.tags, None));
            r.push(ORow::Switch("Own taskbar button per account", Sw::Taskbar, v.taskbar, None));
            r.push(ORow::Switch("Restore saved layout on open", Sw::LayoutAuto, v.layout_auto, None));
        }
        1 => {
            r.push(ORow::Head("SHOW", None));
            r.push(ORow::Switch("Codex usage", Sw::Codex, v.codex, None));
            let bar_sel = BAR_CHOICES.iter().position(|c| c.1 == v.usage_bars).or(Some(0));
            r.push(ORow::Chips("Usage bars", Group::Bars, names(&BAR_CHOICES), bar_sel, None));
            r.push(ORow::Head("TELL ME", None));
            r.push(ORow::Number("Usage alert at", 0, Group::Alert, v.alert_percent, Some("Notifies you when an account passes this share of its limit.")));
            r.push(ORow::Number("Use next under", 1, Group::UseNext, v.use_next, Some("Tags the soonest-resetting account still under this much.")));
            let cur = crate::time::parse_clock(&v.daily_summary);
            let summary_sel = if v.daily_summary.trim().is_empty() {
                Some(0)
            } else {
                SUMMARY_CHOICES.iter().position(|c| !c.1.is_empty() && crate::time::parse_clock(c.1) == cur)
            };
            r.push(ORow::Chips("Daily summary", Group::Summary, names(&SUMMARY_CHOICES), summary_sel, Some("One notification a day: weekly use and reset per account.")));
            r.push(ORow::Head("KEEP UPDATED", None));
            let nudge_sel = NUDGE_CHOICES.iter().position(|c| c.1 == v.usage_nudge).or(Some(0));
            r.push(ORow::Chips(
                "Auto-refresh",
                Group::Nudge,
                names(&NUDGE_CHOICES),
                nudge_sel,
                Some("Claude stops checking usage when its tray menu hasn't been opened for a day. This right-clicks its tray icon for you, only while you're away from the keyboard. Off: right-click it yourself to resume."),
            ));
        }
        2 => {
            r.push(ORow::Head("HOW", None));
            let sound_sel = SOUND_CHOICES.iter().position(|c| c.1 == v.alert_sound).or(Some(0));
            r.push(ORow::Chips("Sound", Group::Sound, names(&SOUND_CHOICES), sound_sel, None));
            let windows_style = v.alert_style == "windows" || v.alert_card == "windows";
            r.push(ORow::Chips("Style", Group::Style, names(&STYLE_CHOICES), Some(if windows_style { 1 } else { 0 }), Some("Windows alerts vanish in seconds; the card waits for you.")));
            if !windows_style {
                let stay_sel = STAY_CHOICES.iter().position(|c| c.1 == v.alert_card).or(Some(2));
                r.push(ORow::Chips("Card stays up", Group::Stay, names(&STAY_CHOICES), stay_sel, None));
                let mons = crate::layout::card_monitors();
                if mons.len() > 1 {
                    let sel = crate::layout::choose_monitor(&mons, &v.card_monitor, v.card_monitor_index);
                    r.push(ORow::Chips("Card monitor", Group::CardMonitor, crate::layout::monitor_labels(&mons), sel, Some("Which screen alert cards appear on. Pick one that is out of the way of games and Windows notifications.")));
                }
                let corner = crate::layout::CORNERS.iter().position(|c| c.2 == crate::layout::Corner::parse(&v.card_corner));
                r.push(ORow::Grid("Card corner", Group::CardCorner, crate::layout::CORNERS.iter().map(|c| c.0.to_string()).collect(), corner));
                r.push(ORow::Switch("Close it when I open that chat", Sw::CardSeen, v.card_seen, Some("Once you open the chat yourself, its card goes away. Claude chats.")));
            }
            r.push(ORow::Head("WHEN", None));
            r.push(ORow::Switch("A chat finishes", Sw::Notify, v.notify, None));
            r.push(ORow::Switch("A chat needs me", Sw::NotifyWaiting, v.notify_waiting, None));
            if v.notify_waiting {
                r.push(ORow::Switch("A reply ends in a question", Sw::Questions, v.questions, None));
                r.push(ORow::Switch("Codex alerts", Sw::CodexAlerts, v.codex_alerts, Some("Replaces Codex's own notifications (its permission prompts stay). Turning this off puts Codex's settings back.")));
            }
        }
        _ => {
            let heads: [(&'static str, Option<&'static str>); 3] = [
                ("THIS APP", Some("They work anywhere in Windows. Use Alt, Win or two modifier keys with a letter or digit, or an F-key alone. Backspace while recording clears one.")),
                ("ACCOUNTS", Some("Brings that account's window forward, or opens it if it is closed.")),
                ("MORE", None),
            ];
            let mut section = u8::MAX;
            for (i, k) in v.hotkeys.iter().enumerate() {
                if k.section != section {
                    section = k.section;
                    let (name, info) = heads.get(section as usize).copied().unwrap_or(("", None));
                    r.push(ORow::Head(name, info));
                }
                r.push(ORow::Key(i));
            }
        }
    }
    r
}

/// A Settings tab's height: the title and tabs, its rows, and the tools row on General.
/// Settings keeps one height for every tab (the tallest), so switching tabs never
/// resizes the window: a resize always shows a frame or so of the old drawing.
fn settings_height(v: &Opts) -> f32 {
    (0..TABS.len() as u8).map(|t| options_height(t, v)).fold(0.0, f32::max)
}

fn options_height(tab: u8, v: &Opts) -> f32 {
    OPT_TOP + tab_rows(tab, v).iter().map(ORow::height).sum::<f32>() + if tab == 0 { OPT_TOOLS } else { 4.0 } + PAD
}

fn draw_options(p: &Painter, t: &Theme, o: &Options, v: &Opts, h: f32, hover: Option<Hit>, caret_on: bool, hits: &mut Vec<(R, Hit)>) {
    icon_button(p, t, hits, hover, Hit::Back, R::new(PAD, 14.0, PAD + 32.0, 46.0), "\u{E72B}", t.text);
    p.text("Settings", &p.g.title, R::new(PAD + 42.0, 14.0, W - PAD - 40.0, 46.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
    icon_button(p, t, hits, hover, Hit::Close, R::new(W - PAD - 32.0, 14.0, W - PAD, 46.0), "\u{E711}", t.text);

    // The tab strip.
    let gap = 4.0;
    let tw = (W - 2.0 * PAD - gap * (TABS.len() as f32 - 1.0)) / TABS.len() as f32;
    let broken_keys = v.hotkeys.iter().any(|k| k.error.is_some());
    for (i, name) in TABS.iter().enumerate() {
        let r = R::new(PAD + i as f32 * (tw + gap), 56.0, PAD + i as f32 * (tw + gap) + tw, 88.0);
        let id = Hit::Opt(OptHit::Tab(i as u8));
        let on = o.tab == i as u8;
        if on {
            p.fill(r, 6.0, t.accent);
        } else {
            p.fill(r, 6.0, if hover == Some(id) { t.card_hover } else { t.card });
            p.stroke(r, 6.0, if i == 3 && broken_keys { t.red } else { t.border }, 1.0);
        }
        p.text(name, &p.g.small, r, if on { t.on_accent } else { t.text }, DWRITE_TEXT_ALIGNMENT_CENTER);
        hits.push((r, id));
    }

    // The rows scroll when the window is shorter than the tallest tab; the title and tabs stay.
    let full = settings_height(v);
    let scroll = o.scroll.clamp(0.0, settings_max_scroll(full, h));
    let view = R::new(0.0, OPT_VIEW_TOP, W, h);
    let hits_from = hits.len();
    if scroll > 0.0 || full > h {
        unsafe { p.rt.PushAxisAlignedClip(&view.d2(), D2D1_ANTIALIAS_MODE_ALIASED) };
    }
    let mut y = OPT_TOP - scroll;
    // The explainer whose (i) is under the mouse: drawn last, over the rows.
    let mut tip: Option<(R, &'static str)> = None;
    // A small (i) after a label; its hit goes first so it wins over the row's.
    let mut info = |hits: &mut Vec<(R, Hit)>, n: usize, x: f32, top: f32, bottom: f32, text: Option<&'static str>| {
        let Some(text) = text else { return };
        let mid = (top + bottom) / 2.0;
        let r = R::new(x + 4.0, mid - 9.0, x + 22.0, mid + 9.0);
        let id = Hit::Opt(OptHit::Info(n as u8));
        p.text("\u{E946}", &p.g.icon_small, r, if hover == Some(id) { t.accent } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_CENTER);
        hits.push((r, id));
        if hover == Some(id) {
            tip = Some((r, text));
        }
    };
    for (n, row) in tab_rows(o.tab, v).iter().enumerate() {
        match row {
            ORow::Head(name, text) => {
                p.text(name, &p.g.tiny, R::new(PAD + 6.0, y + 8.0, W - PAD, y + OPT_HEAD), t.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
                info(hits, n, PAD + 6.0 + p.width(name, &p.g.tiny), y + 8.0, y + OPT_HEAD, *text);
            }
            ORow::Switch(label, sw, on, text) => {
                let r = R::new(PAD, y, W - PAD, y + OPT_ROW - 4.0);
                let id = Hit::Opt(OptHit::Switch(*sw));
                if hover == Some(id) {
                    p.fill(r, 6.0, t.card);
                }
                p.text(label, &p.g.body, R::new(r.l + 8.0, r.t, r.r - 60.0, r.b), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
                info(hits, n, r.l + 8.0 + p.width(label, &p.g.body), r.t, r.b, *text);
                switch(p, t, R::new(r.r - 50.0, r.t + 7.0, r.r - 10.0, r.b - 7.0), *on);
                hits.push((r, id));
            }
            ORow::Chips(label, group, names, selected, text) => {
                p.text(label, &p.g.body, R::new(PAD + 8.0, y, W - PAD, y + OPT_ROW - 4.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
                info(hits, n, PAD + 8.0 + p.width(label, &p.g.body), y, y + OPT_ROW - 4.0, *text);
                let gap = 4.0;
                // Each choice as wide as its words, so the row's label keeps its room.
                let widths: Vec<f32> = names.iter().map(|n| (p.width(n, &p.g.small) + 18.0).max(46.0)).collect();
                let mut x = W - PAD - 6.0 - widths.iter().sum::<f32>() - gap * (names.len() as f32 - 1.0);
                for (i, name) in names.iter().enumerate() {
                    let cw = widths[i];
                    let r = R::new(x, y + 5.0, x + cw, y + OPT_ROW - 9.0);
                    let id = Hit::Opt(OptHit::Chip(*group, i));
                    if *selected == Some(i) {
                        p.fill(r, 6.0, t.accent);
                        p.text(name, &p.g.small, r, t.on_accent, DWRITE_TEXT_ALIGNMENT_CENTER);
                    } else {
                        p.fill(r, 6.0, if hover == Some(id) { t.card_hover } else { t.card });
                        p.stroke(r, 6.0, t.border, 1.0);
                        p.text(name, &p.g.small, r, t.text, DWRITE_TEXT_ALIGNMENT_CENTER);
                    }
                    hits.push((r, id));
                    x += cw + gap;
                }
            }
            ORow::Grid(label, group, names, selected) => {
                p.text(label, &p.g.body, R::new(PAD + 8.0, y, W - PAD, y + OPT_ROW - 4.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
                let gap = 4.0;
                let cw = names.iter().map(|n| p.width(n, &p.g.small) + 18.0).fold(46.0f32, f32::max);
                let x0 = W - PAD - 6.0 - 2.0 * cw - gap;
                for (i, name) in names.iter().enumerate() {
                    let x = x0 + (i % 2) as f32 * (cw + gap);
                    let top = y + 5.0 + (i / 2) as f32 * 30.0;
                    let r = R::new(x, top, x + cw, top + OPT_ROW - 14.0);
                    let id = Hit::Opt(OptHit::Chip(*group, i));
                    if *selected == Some(i) {
                        p.fill(r, 6.0, t.accent);
                        p.text(name, &p.g.small, r, t.on_accent, DWRITE_TEXT_ALIGNMENT_CENTER);
                    } else {
                        p.fill(r, 6.0, if hover == Some(id) { t.card_hover } else { t.card });
                        p.stroke(r, 6.0, t.border, 1.0);
                        p.text(name, &p.g.small, r, t.text, DWRITE_TEXT_ALIGNMENT_CENTER);
                    }
                    hits.push((r, id));
                }
            }
            ORow::Number(label, k, group, value, text) => {
                // A label, the number (greyed while off), and an on/off switch like every other row.
                let on = *value > 0;
                let row = R::new(PAD, y, W - PAD, y + OPT_ROW - 4.0);
                let sw = R::new(row.r - 50.0, row.t + 7.0, row.r - 10.0, row.b - 7.0);
                let id = Hit::Opt(OptHit::Chip(*group, 0));
                if hover == Some(id) {
                    p.fill(R::new(sw.l - 8.0, row.t, row.r, row.b), 6.0, t.card);
                }
                p.text(label, &p.g.body, R::new(row.l + 8.0, row.t, row.r - 160.0, row.b), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
                info(hits, n, row.l + 8.0 + p.width(label, &p.g.body), row.t, row.b, *text);
                let bx = R::new(sw.l - 98.0, y + 4.0, sw.l - 38.0, y + OPT_ROW - 8.0);
                let focused = o.focus == Some(*k);
                if on || focused {
                    draw_box(p, t, &o.boxes[*k], bx, focused, caret_on, "");
                } else {
                    let mut dim = TextBox::with(&o.boxes[*k].value());
                    dim.read_only = true;
                    draw_box(p, t, &dim, bx, false, false, "");
                }
                p.text("%", &p.g.body, R::new(bx.r + 6.0, row.t, sw.l - 4.0, row.b), if on { t.text2 } else { t.text3 }, DWRITE_TEXT_ALIGNMENT_LEADING);
                hits.push((bx, Hit::Opt(OptHit::Number(*k))));
                switch(p, t, sw, on);
                hits.push((R::new(sw.l - 8.0, row.t, row.r, row.b), id));
            }
            ORow::Drive => {
                let path = if v.drive_path.trim().is_empty() { "No folder chosen".to_string() } else { v.drive_path.clone() };
                p.text(&path, &p.g.tiny, R::new(PAD + 8.0, y - 4.0, W - PAD - 90.0, y + 20.0), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
                button(p, t, hits, hover, Hit::Opt(OptHit::DrivePick), R::new(W - PAD - 84.0, y - 4.0, W - PAD - 10.0, y + 20.0), "Change", false);
            }
            ORow::Key(i) => {
                let Some(k) = v.hotkeys.get(*i) else { continue };
                let recording = o.capturing.as_deref() == Some(k.key.as_str());
                let label_b = if k.error.is_some() { y + 26.0 } else { y + KEY_ROW - 6.0 };
                p.text(&k.label, &p.g.body, R::new(PAD + 8.0, y, W - PAD - 214.0, label_b), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
                if let Some(e) = &k.error {
                    p.text(e, &p.g.tiny, R::new(PAD + 8.0, y + 24.0, W - PAD - 214.0, y + KEY_ROW - 4.0), t.red, DWRITE_TEXT_ALIGNMENT_LEADING);
                }
                let r = R::new(W - PAD - 206.0, y + 7.0, W - PAD - 40.0, y + KEY_ROW - 11.0);
                let id = Hit::Opt(OptHit::Key(*i));
                let text = if recording {
                    "Press keys (Esc)".to_string()
                } else if k.combo.is_empty() {
                    "Not set".to_string()
                } else {
                    k.combo.clone()
                };
                p.fill(r, 6.0, if hover == Some(id) { t.card_hover } else { t.card });
                let edge = if recording { t.accent } else if k.error.is_some() { t.red } else { t.border };
                p.stroke(r, 6.0, edge, if recording { 2.0 } else { 1.0 });
                let color = if recording { t.accent } else if k.combo.is_empty() { t.text3 } else { t.text };
                p.text(&text, &p.g.small, r, color, DWRITE_TEXT_ALIGNMENT_CENTER);
                hits.push((r, id));
                if !k.combo.is_empty() && !recording {
                    icon_button(p, t, hits, hover, Hit::Opt(OptHit::KeyClear(*i)), R::new(W - PAD - 34.0, y + 7.0, W - PAD - 4.0, y + KEY_ROW - 11.0), "\u{E711}", t.text2);
                }
            }
        }
        y += row.height();
    }

    if o.tab == 0 {
        // Tools: at the foot of the full-height content.
        let by = full - PAD - 34.0 - scroll;
        p.line(PAD + 4.0, by - 10.0, W - PAD - 4.0, by - 10.0, t.border, 1.0);
        let labels = ["Check setup", "Edit file", "Help", "Exit app"];
        let bw = (W - 2.0 * PAD - 3.0 * 6.0) / 4.0;
        for (i, l) in labels.iter().enumerate() {
            let x = PAD + i as f32 * (bw + 6.0);
            // Small text: four buttons share one row.
            let r = R::new(x, by, x + bw, by + 32.0);
            let id = Hit::Opt(OptHit::Action(i as u8));
            p.fill(r, 6.0, if hover == Some(id) { t.card_hover } else { t.card });
            p.stroke(r, 6.0, t.border, 1.0);
            p.text(l, &p.g.small, r.inset(4.0, 0.0), if i == 3 { t.red } else { t.text }, DWRITE_TEXT_ALIGNMENT_CENTER);
            hits.push((r, id));
        }
    }

    if scroll > 0.0 || full > h {
        unsafe { p.rt.PopAxisAlignedClip() };
        // Only the part of a control inside the visible rows can be hit.
        let mut i = hits_from;
        while i < hits.len() {
            let r = &mut hits[i].0;
            r.t = r.t.max(view.t);
            r.b = r.b.min(view.b);
            if r.t >= r.b {
                hits.remove(i);
            } else {
                i += 1;
            }
        }
        // A thin bar on the right edge shows there is more.
        let track = view.b - view.t - 8.0;
        let thumb = (track * (view.b - view.t) / (full - OPT_VIEW_TOP)).clamp(24.0, track);
        let top = view.t + 4.0 + (track - thumb) * scroll / settings_max_scroll(full, h).max(1.0);
        p.fill(R::new(W - 5.0, top, W - 2.0, top + thumb), 1.5, t.text3);
    }

    // The explainer under (or, near the bottom, over) its (i).
    if let Some((anchor, text)) = tip {
        let w = W - 2.0 * PAD - 16.0;
        let th = p.text_height(text, &p.g.hint, w - 20.0) + 14.0;
        let top = if anchor.b + 6.0 + th <= h - 6.0 { anchor.b + 6.0 } else { anchor.t - 6.0 - th };
        let r = R::new(PAD + 8.0, top, PAD + 8.0 + w, top + th);
        p.fill(r, 8.0, t.ring);
        p.stroke(r, 8.0, t.border, 1.0);
        p.text(text, &p.g.hint, r.inset(10.0, 7.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
    }
}

/// Make a change from the Settings screen.
fn apply_opt(o: OptHit) {
    let v = with(|f| f.snap.opts.clone()).unwrap_or_default();
    match o {
        OptHit::Switch(Sw::Start) => tray::set_startup(!v.start_with_windows),
        OptHit::Switch(Sw::CodexAlerts) => tray::set_codex_alerts(!v.codex_alerts),
        OptHit::Switch(s) => tray::update_settings(|c| match s {
            Sw::Drive => c.wait_for_drive.enabled = !c.wait_for_drive.enabled,
            Sw::Tags => c.show_tags = !c.show_tags,
            Sw::Taskbar => c.separate_taskbar = !c.separate_taskbar,
            Sw::LayoutAuto => c.restore_on_open = !c.restore_on_open,
            Sw::Codex => c.codex.enabled = !c.codex.enabled,
            Sw::Notify => c.toggle_alert(true),
            Sw::NotifyWaiting => c.toggle_alert(false),
            Sw::Questions => c.alert_questions = !c.alert_questions,
            Sw::CardSeen => c.close_card_on_open = !c.close_card_on_open,
            Sw::CodexAlerts => {}
            Sw::Start => {}
        }),
        // "Off" next to a number box.
        OptHit::Chip(Group::Alert, _) | OptHit::Chip(Group::UseNext, _) => {
            let k = if matches!(o, OptHit::Chip(Group::Alert, _)) { 0 } else { 1 };
            // The live settings, not the screen's copy (which can be a step behind right after typing).
            let (alert, next) = tray::number_settings();
            let on = if k == 0 { alert > 0 } else { next > 0 };
            let n = with(|f| match &f.view {
                View::Options(op) => op.number(k),
                _ => NUMBER_DEFAULTS[k],
            })
            .unwrap_or(NUMBER_DEFAULTS[k]);
            let new = if on { 0 } else { n };
            tray::update_settings(|c| if k == 0 { c.alert_percent = new } else { c.use_next_below = new });
        }
        OptHit::Number(_) => {}
        OptHit::Chip(Group::Stay, i) => {
            tray::update_settings(|c| {
                c.alert_card = STAY_CHOICES[i].1.to_string();
                c.alert_style = "card".into(); // choosing a time means using the card
            });
            card_preview();
        }
        OptHit::Chip(Group::Style, i) => {
            tray::update_settings(|c| {
                c.alert_style = STYLE_CHOICES[i].1.to_string();
                if c.alert_card == "windows" {
                    c.alert_card = "until".into();
                }
            });
            if STYLE_CHOICES[i].1 == "card" {
                card_preview();
            }
        }
        OptHit::Chip(Group::CardMonitor, i) => {
            let mons = crate::layout::card_monitors();
            if let Some(m) = mons.get(i) {
                let name = m.name.clone();
                tray::update_settings(|c| {
                    c.card_monitor = name;
                    c.card_monitor_index = Some(i);
                });
                card_preview();
            }
        }
        OptHit::Chip(Group::CardCorner, i) => {
            tray::update_settings(|c| c.card_corner = crate::layout::CORNERS[i].1.to_string());
            card_preview();
        }
        OptHit::Chip(Group::Sound, i) => {
            tray::update_settings(|c| c.alert_sound = SOUND_CHOICES[i].1.to_string());
            if SOUND_CHOICES[i].1 == "soft" {
                crate::sound::chime(true); // let you hear it
            }
        }
        OptHit::Chip(Group::Bars, i) => tray::update_settings(|c| c.usage_bars = BAR_CHOICES[i].1.to_string()),
        OptHit::Chip(Group::Nudge, i) => tray::update_settings(|c| c.usage_nudge = NUDGE_CHOICES[i].1.to_string()),
        OptHit::Chip(Group::Summary, i) => tray::update_settings(|c| c.daily_summary = SUMMARY_CHOICES[i].1.to_string()),
        OptHit::Tab(i) => {
            tray::update_settings(|c| c.settings_tab = i);
            show_view(View::Options(Box::new(Options::new())));
        }
        OptHit::Info(_) => {}
        OptHit::Key(i) => {
            let key = v.hotkeys.get(i).map(|k| k.key.clone());
            with(|f| {
                if let View::Options(o) = &mut f.view {
                    o.capturing = if o.capturing == key { None } else { key };
                }
            });
        }
        OptHit::KeyClear(i) => {
            if let Some(k) = v.hotkeys.get(i) {
                tray::set_hotkey(&k.key, "");
            }
        }
        OptHit::DrivePick => {
            if let Some(path) = pick_folder(hwnd(), &v.drive_path) {
                tray::update_settings(|c| {
                    c.wait_for_drive.path = path;
                    c.wait_for_drive.enabled = true;
                });
            }
        }
        OptHit::Action(i) => tray::settings_action(hwnd(), i),
    }
}

/// Keys on the Settings screen: Esc goes back, or records a new shortcut.
fn options_key(f: &mut Fly, vk: u16) -> After {
    let target = match &f.view {
        View::Options(o) => o.capturing.clone(),
        _ => None,
    };
    let capturing = target.is_some();
    let hwnd = f.hwnd;
    if let View::Options(o) = &mut f.view {
        if let (false, Some(i)) = (capturing, o.focus) {
            match vk {
                VK_RETURN | VK_TAB => NUMBER.with(|c| c.set(o.commit())),
                VK_ESCAPE => {
                    // Undo the typing.
                    o.focus = None;
                    **o = Options::new();
                }
                _ => edit_box(&mut o.boxes[i], vk, key_down(VK_CONTROL), key_down(VK_SHIFT), hwnd),
            }
            return After::Nothing;
        }
    }
    let Some(target) = target else {
        return match vk {
            VK_ESCAPE => After::List,
            _ => After::Nothing,
        };
    };
    let stop = |f: &mut Fly| {
        if let View::Options(o) = &mut f.view {
            o.capturing = None;
        }
    };
    match vk {
        VK_ESCAPE => {
            stop(f);
            After::Nothing
        }
        VK_BACK | VK_DELETE => {
            stop(f);
            After::SetKey(target, String::new())
        }
        // Shift, Ctrl, Alt and Win on their own (either side): wait for the real key.
        0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5 => After::Nothing,
        _ => {
            let fkey = (0x70..=0x87).contains(&vk);
            let key = match vk {
                0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
                0x70..=0x87 => format!("F{}", vk - 0x6F),
                0x20 => "Space".to_string(),
                _ => return After::Nothing, // only letters, digits, F-keys and Space
            };
            let mut parts: Vec<String> = Vec::new();
            for (k, name) in [(0x11u16, "Ctrl"), (0x12, "Alt"), (0x10, "Shift")] {
                if key_down(k) {
                    parts.push(name.to_string());
                }
            }
            if key_down(0x5B) || key_down(0x5C) {
                parts.push("Win".to_string());
            }
            // A shortcut must not steal ordinary typing or Ctrl+C-style keys: F-keys may stand
            // alone; anything else needs Alt or Win, or two modifiers (Ctrl+Shift+K).
            let strong = parts.iter().any(|p| p == "Alt" || p == "Win") || parts.len() >= 2;
            if !fkey && !strong {
                return After::Nothing;
            }
            parts.push(key);
            stop(f);
            After::SetKey(target, parts.join("+"))
        }
    }
}

/// Windows' own folder picker.
fn pick_folder(owner: HWND, start: &str) -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = d.GetOptions().ok()?;
        d.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = d.SetTitle(w!("Folder to wait for before opening Claude"));
        if !start.trim().is_empty() {
            let ws = wide(start.trim());
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(windows::core::PCWSTR(ws.as_ptr()), None) {
                let _ = d.SetFolder(&item);
            }
        }
        d.Show(windows::Win32::Foundation::HWND(owner as _)).ok()?;
        let item = d.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

// ---- alert cards, drawn like the flyout ------------------------------------------

pub const CARD_W: f32 = 360.0;

thread_local! {
    /// Drawing resources for alert cards (kept while any card is up).
    static CARD_GFX: RefCell<Option<Gfx>> = const { RefCell::new(None) };
}

/// Where a card was clicked.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CardHit {
    Primary,
    Dismiss,
    Close,
    Body,
    Snooze,
    Mute,
}

/// What a card shows.
pub struct CardView<'a> {
    pub title: &'a str,
    pub text: &'a str,
    /// Initials and color of the account's badge; None shows the app's mark.
    pub badge: Option<(&'a str, u32)>,
    /// Label of the action button ("Open chat"); None: no buttons, click anywhere.
    pub primary: Option<&'a str>,
    /// Seconds it stays up (0: until closed) and milliseconds left, for the time line.
    pub stay: u32,
    pub left: i64,
    pub hover: Option<CardHit>,
    /// How many alerts for this chat it stands for: from 2, "3 updates" by the buttons.
    pub count: u32,
    /// A chat alert: a second row with "Snooze 1 h" and "Mute until I open it".
    pub snooze: bool,
    /// With `snooze`: also "Mute until I open it" (only chats whose opening Hive can see).
    pub mute: bool,
}

const CARD_TEXT_L: f32 = 62.0;
const CARD_BTN_H: f32 = 30.0;
/// The second button row is as tall as the first.
const CARD_SNOOZE_H: f32 = CARD_BTN_H;
/// Side padding inside a card button, around its measured text.
const CARD_BTN_PAD: f32 = 28.0;

/// Left and right edges of the second button row (Snooze, Mute): each as wide as its text plus
/// the padding row 1 uses, starting at `left`, 8 px apart, and never past `limit`. When the
/// row is too wide the buttons shrink together to fit.
fn snooze_row(text_widths: &[f32], left: f32, limit: f32) -> Vec<(f32, f32)> {
    let mut w: Vec<f32> = text_widths.iter().map(|t| t + CARD_BTN_PAD).collect();
    let gaps = 8.0 * w.len().saturating_sub(1) as f32;
    let total: f32 = w.iter().sum::<f32>() + gaps;
    let room = (limit - left).max(0.0);
    if total > room && total > gaps {
        let k = ((room - gaps) / (total - gaps)).max(0.0);
        for x in &mut w {
            *x *= k;
        }
    }
    let mut x = left;
    w.into_iter()
        .map(|w| {
            let r = (x, x + w);
            x += w + 8.0;
            r
        })
        .collect()
}

fn card_text_height(g: &Gfx, text: &str) -> f32 {
    let w = wtext(text);
    unsafe {
        let Ok(l) = g.dwrite.CreateTextLayout(&w, &g.hint, CARD_W - CARD_TEXT_L - 16.0, 2000.0) else { return 18.0 };
        let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS::default();
        if l.GetMetrics(&mut m).is_ok() {
            m.height.min(16.0 * 8.0)
        } else {
            18.0
        }
    }
}

fn with_card_gfx<R>(f: impl FnOnce(&mut Gfx) -> R) -> Option<R> {
    CARD_GFX.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = make_gfx().ok();
        }
        c.as_mut().map(f)
    })
}

/// A card's height in DIPs for this text.
pub fn card_height(text: &str, buttons: bool, snooze: bool) -> f32 {
    let body = with_card_gfx(|g| card_text_height(g, text)).unwrap_or(18.0);
    let h = 14.0 + 22.0 + 4.0 + body + if buttons { 12.0 + CARD_BTN_H } else { 0.0 } + if buttons && snooze { 8.0 + CARD_SNOOZE_H } else { 0.0 } + 16.0;
    h.max(76.0)
}

/// Free the card drawing resources (no cards left).
pub fn card_gfx_release() {
    CARD_GFX.with(|c| *c.borrow_mut() = None);
}

/// Paint one card into its window. Returns the clickable areas in DIPs.
pub unsafe fn paint_card(hwnd: HWND, v: &CardView) -> Vec<([f32; 4], CardHit)> {
    let mut hits = Vec::new();
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let params = BP_PAINTPARAMS { cbSize: std::mem::size_of::<BP_PAINTPARAMS>() as u32, dwFlags: BPPF_ERASE, prcExclude: null(), pBlendFunction: null() };
    let mut mem = null_mut();
    let pb = BeginBufferedPaint(hdc, &rc, BPBF_TOPDOWNDIB, &params, &mut mem);
    if pb as usize != 0 {
        let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96);
        let scale = dpi as f32 / 96.0;
        draw_card(mem, &rc, scale, v, &mut hits);
        EndBufferedPaint(pb, 1);
    }
    EndPaint(hwnd, &ps);
    hits
}

/// Draw a card's contents into a device context (`rc` is its pixel rectangle).
pub unsafe fn draw_card(mem: windows_sys::Win32::Graphics::Gdi::HDC, rc: &RECT, scale: f32, v: &CardView, hits: &mut Vec<([f32; 4], CardHit)>) {
        let rc = *rc;
        let h = rc.bottom as f32 / scale;
        with_card_gfx(|g| {
            if g.rt.is_none() {
                let props = D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                    dpiX: 0.0,
                    dpiY: 0.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                };
                let Ok(rt) = g.factory.CreateDCRenderTarget(&props) else { return };
                let Ok(brush) = rt.CreateSolidColorBrush(&d2c(C(1.0, 1.0, 1.0, 1.0)), None) else { return };
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                g.logo = logo_bitmap(&rt);
                g.rt = Some(rt);
                g.brush = Some(brush);
            }
            let rt = g.rt.clone().unwrap();
            let wr = windows::Win32::Foundation::RECT { left: rc.left, top: rc.top, right: rc.right, bottom: rc.bottom };
            if rt.BindDC(windows::Win32::Graphics::Gdi::HDC(mem), &wr).is_err() {
                return;
            }
            rt.SetDpi(96.0 * scale, 96.0 * scale);
            rt.BeginDraw();
            rt.Clear(Some(&d2c(C(0.0, 0.0, 0.0, 0.0))));
            let brush = g.brush.clone().unwrap();
            let p = Painter { g, rt: &rt, brush: &brush };
            let t = theme(is_dark());
            // Thin, so the frosted blur behind shows through.
            p.fill(R::new(0.0, 0.0, CARD_W, h), 0.0, if is_dark() { hex(0x161412, 0.22) } else { hex(0xF6F4F2, 0.30) });
            // The whole card is clickable (it does the main action).
            hits.push(([0.0, 0.0, CARD_W, h], CardHit::Body));
            // Badge: the account's color and initials, like its row and window tag.
            match v.badge {
                Some((txt, color)) => {
                    p.circle(14.0 + 18.0, 14.0 + 18.0, 18.0, hex(color, 1.0));
                    p.text(txt, &p.g.avatar, R::new(14.0, 14.0, 50.0, 50.0), C(1.0, 1.0, 1.0, 1.0), DWRITE_TEXT_ALIGNMENT_CENTER);
                }
                None => p.logo(14.0 + 18.0, 14.0 + 18.0, 30.0),
            }
            p.text(v.title, &p.g.bold, R::new(CARD_TEXT_L, 12.0, CARD_W - 44.0, 34.0), t.text, DWRITE_TEXT_ALIGNMENT_LEADING);
            let body_h = card_text_height(g, v.text);
            p.text(v.text, &p.g.hint, R::new(CARD_TEXT_L, 38.0, CARD_W - 16.0, 38.0 + body_h), t.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
            // Close.
            let cr = R::new(CARD_W - 40.0, 8.0, CARD_W - 10.0, 38.0);
            if v.hover == Some(CardHit::Close) {
                p.fill(cr, 6.0, C(t.red.0, t.red.1, t.red.2, 0.22));
            }
            p.text("\u{E711}", &p.g.icon_small, cr, t.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
            hits.push(([cr.l, cr.t, cr.r, cr.b], CardHit::Close));
            // Buttons: the action, and Dismiss.
            if let Some(label) = v.primary {
                let sz = if v.snooze { 8.0 + CARD_SNOOZE_H } else { 0.0 };
                let by = h - 16.0 - CARD_BTN_H - sz;
                let pw = (p.width(label, &p.g.bold) + 28.0).clamp(96.0, 200.0);
                let pr = R::new(CARD_TEXT_L, by, CARD_TEXT_L + pw, by + CARD_BTN_H);
                let mut c = t.accent;
                if v.hover == Some(CardHit::Primary) {
                    c = C((c.0 * 1.08).min(1.0), (c.1 * 1.08).min(1.0), (c.2 * 1.08).min(1.0), 1.0);
                }
                p.fill(pr, 6.0, c);
                p.text(label, &p.g.bold, pr.inset(8.0, 0.0), t.on_accent, DWRITE_TEXT_ALIGNMENT_CENTER);
                hits.push(([pr.l, pr.t, pr.r, pr.b], CardHit::Primary));
                let dr = R::new(pr.r + 8.0, by, pr.r + 8.0 + 90.0, by + CARD_BTN_H);
                p.fill(dr, 6.0, if v.hover == Some(CardHit::Dismiss) { t.card_hover } else { t.card });
                p.stroke(dr, 6.0, t.border, 1.0);
                p.text("Dismiss", &p.g.body, dr.inset(8.0, 0.0), t.text, DWRITE_TEXT_ALIGNMENT_CENTER);
                hits.push(([dr.l, dr.t, dr.r, dr.b], CardHit::Dismiss));
                if v.count > 1 {
                    let nr = R::new(dr.r + 8.0, by, CARD_W - 14.0, by + CARD_BTN_H);
                    p.text(&format!("{} updates", v.count), &p.g.hint, nr, t.text2, DWRITE_TEXT_ALIGNMENT_TRAILING);
                }
                if v.snooze {
                    let sy = by + CARD_BTN_H + 8.0;
                    let all = [("Snooze 1 h", CardHit::Snooze), ("Mute until I open it", CardHit::Mute)];
                    let all = &all[..if v.mute { 2 } else { 1 }];
                    let tw: Vec<f32> = all.iter().map(|(l, _)| p.width(l, &p.g.body)).collect();
                    for (&(label, hit), (l, rr)) in all.iter().zip(snooze_row(&tw, CARD_TEXT_L, CARD_W - 14.0)) {
                        let r = R::new(l, sy, rr, sy + CARD_SNOOZE_H);
                        p.fill(r, 6.0, if v.hover == Some(hit) { t.card_hover } else { t.card });
                        p.stroke(r, 6.0, t.border, 1.0);
                        p.text(label, &p.g.body, r, t.text, DWRITE_TEXT_ALIGNMENT_CENTER);
                        hits.push(([r.l, r.t, r.r, r.b], hit));
                    }
                }
            }
            // Time left, as a thin line along the bottom.
            if v.stay > 0 {
                let frac = (v.left.max(0) as f32 / (v.stay as f32 * 1000.0)).clamp(0.0, 1.0);
                p.fill(R::new(0.0, h - 3.0, CARD_W * frac, h), 0.0, t.accent);
            }
            if rt.EndDraw(None, None).is_err() {
                g.rt = None;
                g.logo = None;
                g.brush = None;
            }
        });
}

/// The flyout's frosted glass on another of this app's windows (alert cards).
/// Frosted glass for an alert card. Cards never take focus, and Windows draws its
/// backdrop solid on a window it thinks is inactive, so a card's frame always says it
/// is active (card.rs answers WM_NCACTIVATE with TRUE, and sends it once at creation).
pub fn glass(hwnd: HWND) {
    unsafe {
        BufferedPaintInit();
        apply_glass(hwnd, is_dark());
    }
}

/// A short-lived sample card, so you can see what alerts will look like.
fn card_preview() {
    crate::card::show(
        "This is an alert card",
        "When a chat finishes or needs you, a card like this waits here. Open acts on it; Dismiss or the X closes it.",
        Some(("P".into(), 0xC9922E)),
        crate::card::Action::Preview,
        10,
    );
}

#[cfg(test)]
mod card_row_tests {
    use super::*;

    #[test]
    fn snooze_row_matches_row_one() {
        let r = snooze_row(&[62.0, 118.0], CARD_TEXT_L, CARD_W - 14.0);
        assert_eq!(r.len(), 2);
        // Left edge lines up with row 1; widths are text plus the same padding.
        assert_eq!(r[0].0, CARD_TEXT_L);
        assert_eq!(r[0].1 - r[0].0, 62.0 + CARD_BTN_PAD);
        assert_eq!(r[1].1 - r[1].0, 118.0 + CARD_BTN_PAD);
        assert_eq!(r[1].0 - r[0].1, 8.0);
        assert!(r[1].1 <= CARD_W - 14.0);
        // Same height as the buttons above, so the card height math has one button height.
        assert_eq!(CARD_SNOOZE_H, CARD_BTN_H);
    }

    #[test]
    fn snooze_row_shrinks_instead_of_passing_the_margin() {
        let r = snooze_row(&[200.0, 300.0], CARD_TEXT_L, CARD_W - 14.0);
        assert_eq!(r[0].0, CARD_TEXT_L);
        assert!(r[1].1 <= CARD_W - 14.0 + 1e-3);
        assert!((r[1].0 - r[0].1 - 8.0).abs() < 1e-3);
        // One button alone.
        let one = snooze_row(&[62.0], CARD_TEXT_L, CARD_W - 14.0);
        assert_eq!(one, vec![(CARD_TEXT_L, CARD_TEXT_L + 90.0)]);
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    #[test]
    fn tall_work_area_changes_nothing() {
        // 2560x1440 at 150%: a 1440 px work area (about 1400 after the taskbar) holds the 570 DIP tab (855 px).
        for full in [520.0f32, 570.0, 640.0] {
            assert_eq!(settings_view_h(full, 1.5, 1400.0), full);
            assert_eq!(settings_max_scroll(full, settings_view_h(full, 1.5, 1400.0)), 0.0);
        }
    }

    #[test]
    fn short_work_area_scrolls_at_full_scale() {
        let full = 640.0;
        for (scale, work) in [(1.0f32, 600.0f32), (1.5, 720.0), (2.0, 900.0), (1.0, 400.0)] {
            let view = settings_view_h(full, scale, work);
            assert!(view * scale <= work - 24.0 + 0.5, "fits {scale} {work}");
            assert!(view < full);
            assert!(view >= OPT_TOP + 120.0 || view * scale > 0.0);
            // Every pixel of the content is reachable: the last scroll position shows the end.
            let max = settings_max_scroll(full, view);
            assert!((view + max - full).abs() < 1e-3);
        }
    }

    #[test]
    fn drag_never_starts_in_a_text_box_or_above_the_rows() {
        assert!(drag_may_start(None, 200.0, 100.0));
        assert!(drag_may_start(Some(Hit::Opt(OptHit::Tab(1))), 200.0, 100.0) == true);
        assert!(!drag_may_start(Some(Hit::Opt(OptHit::Number(0))), 200.0, 100.0));
        assert!(!drag_may_start(Some(Hit::Field(0)), 200.0, 100.0));
        assert!(!drag_may_start(None, 60.0, 100.0));
        assert!(!drag_may_start(None, 200.0, 0.0));
    }

    #[test]
    fn scrolled_rows_keep_their_offsets() {
        // A row at content y sits at y - scroll on screen, so a click at screen y maps back to content y + scroll.
        let (full, view) = (640.0f32, 400.0f32);
        let max = settings_max_scroll(full, view);
        for scroll in [0.0, 48.0, 137.5, max] {
            let content_y = OPT_TOP + 5.0 * OPT_ROW;
            let screen = content_y - scroll;
            assert!((screen + scroll - content_y).abs() < 1e-4);
        }
        // The Tools row (General) is at the foot of the content, reachable at max scroll.
        let by = full - PAD - 34.0 - max;
        assert!(by + 32.0 <= view);
    }
}

#[cfg(test)]
mod waiting_key_tests {
    use super::waiting::{self, Slot};
    use super::*;

    fn fly() -> Fly {
        let snap = crate::demo::snapshot(Opts::default(), false, None);
        Fly {
            hwnd: null_mut(),
            gfx: None,
            scale: 1.0,
            h: 600.0,
            dark: true,
            view: View::List,
            snap,
            waiting: waiting::demo(),
            hits: Vec::new(),
            field_rects: [R::default(); FIELDS],
            search_rect: R::default(),
            hover: None,
            pressed: None,
            focus: None,
            kb_used: false,
            caret_on: false,
            selecting: false,
            hidden_at: None,
            tracking: false,
            mode_pinned: None,
        }
    }

    fn chat(f: &Fly, title: &str) -> Slot {
        Slot::Chat(f.waiting.iter().find(|w| w.title == title).unwrap().key.clone())
    }

    fn tab_to(f: &mut Fly, target: &Slot) {
        for _ in 0..40 {
            if f.focus.as_ref() == Some(target) {
                return;
            }
            list_key(f, VK_TAB, false);
        }
        panic!("never reached {target:?}");
    }

    #[test]
    fn tab_and_arrows_walk_dismiss_all_waiting_rows_then_accounts() {
        let mut f = fly();
        let rows = f.snap.rows.len();
        let expect = [
            Slot::Dismiss,
            chat(&f, "Release planning coordinator"),
            chat(&f, "card 14: Fix the login bug"),
            chat(&f, "card 15: Rebuild the search index"),
            chat(&f, "Trip planning"),
            chat(&f, "card 7: Refactor the importer"),
            chat(&f, "card 8: Add unit tests"),
        ];
        let mut seen = Vec::new();
        for _ in 0..expect.len() {
            assert!(matches!(list_key(&mut f, VK_TAB, false), After::Nothing));
            seen.push(f.focus.clone().unwrap());
        }
        assert_eq!(seen, expect);
        for i in 0..rows {
            list_key(&mut f, VK_DOWN, false);
            assert_eq!(f.focus, Some(Slot::Row(i)));
        }
        // Past the last account row it wraps to "Dismiss all"; Shift+Tab and Up walk back.
        list_key(&mut f, VK_TAB, false);
        assert_eq!(f.focus, Some(Slot::Dismiss));
        list_key(&mut f, VK_TAB, true);
        assert_eq!(f.focus, Some(Slot::Row(rows - 1)));
        list_key(&mut f, VK_UP, false);
        assert_eq!(f.focus, Some(Slot::Row(rows - 2)));
        // Escape still hides the flyout.
        assert!(matches!(list_key(&mut f, VK_ESCAPE, false), After::Hide));
    }

    #[test]
    fn enter_opens_the_focused_chat_like_a_click() {
        let mut f = fly();
        let slot = chat(&f, "Trip planning");
        tab_to(&mut f, &slot);
        let w = f.waiting.iter().find(|w| w.title == "Trip planning").unwrap().clone();
        let key_path = list_key(&mut f, VK_RETURN, false);
        let at = f.waiting.iter().position(|x| x.key == w.key).unwrap();
        let click_path = click(&mut f, Hit::Wait(at));
        for after in [key_path, click_path] {
            match after {
                After::OpenWaiting(account, chat, entry, title) => {
                    assert_eq!((account, chat, entry, title), (w.account.clone(), w.chat.clone(), (w.key.clone(), w.at), w.title.clone()));
                }
                _ => panic!("Enter on a waiting row must open it"),
            }
        }
        // Space does the same; "Dismiss all" and account rows keep their own meaning.
        assert!(matches!(list_key(&mut f, VK_SPACE, false), After::OpenWaiting(..)));
        f.focus = Some(Slot::Dismiss);
        assert!(matches!(list_key(&mut f, VK_RETURN, false), After::DismissWaiting));
        f.focus = Some(Slot::Row(1));
        assert!(matches!(list_key(&mut f, VK_RETURN, false), After::Open(_) | After::OpenCodex(_)));
        // A chat with no id cannot be opened: Enter does nothing.
        f.waiting[0].chat.clear();
        f.focus = Some(Slot::Chat(f.waiting[0].key.clone()));
        assert!(matches!(list_key(&mut f, VK_RETURN, false), After::Nothing));
    }

    #[test]
    fn wait_x_is_only_clickable_when_shown() {
        assert!(!wait_x_shown(false, false));
        assert!(wait_x_shown(true, false));
        assert!(wait_x_shown(false, true));
    }

    #[test]
    fn delete_dismisses_only_the_focused_row_and_moves_focus_on() {
        let mut f = fly();
        let slot = chat(&f, "card 14: Fix the login bug");
        tab_to(&mut f, &slot);
        let w = f.waiting.iter().find(|w| w.title == "card 14: Fix the login bug").unwrap().clone();
        match list_key(&mut f, VK_DELETE, false) {
            After::DismissWaitingOne(account, chat, entry) => assert_eq!((account, chat, entry), (w.account.clone(), w.chat.clone(), (w.key.clone(), w.at))),
            _ => panic!("Delete on a waiting row must dismiss that row"),
        }
        // Focus is already on the row that follows it, so Delete again keeps clearing downwards.
        assert_eq!(f.focus, Some(chat(&f, "card 15: Rebuild the search index")));
        // The little x does the same for the row it sits on.
        let i = f.waiting.iter().position(|x| x.key == w.key).unwrap();
        assert!(matches!(click(&mut f, Hit::WaitX(i)), After::DismissWaitingOne(_, _, (k, _)) if k == w.key));
        // Delete means nothing on "Dismiss all", an account row, or with nothing focused.
        for slot in [Some(Slot::Dismiss), Some(Slot::Row(0)), None] {
            f.focus = slot;
            assert!(matches!(list_key(&mut f, VK_DELETE, false), After::Nothing));
        }
    }

    #[test]
    fn focus_survives_a_reload_that_reorders_the_list_and_falls_back_when_the_row_is_gone() {
        let mut f = fly();
        let slot = chat(&f, "Trip planning");
        f.focus = Some(slot.clone());
        f.waiting.reverse();
        keep_focus_valid(&mut f);
        assert_eq!(f.focus, Some(slot), "the focus follows the chat, not its position");
        f.waiting.retain(|w| w.title != "Trip planning");
        keep_focus_valid(&mut f);
        assert_eq!(f.focus, Some(Slot::Row(0)));
        // Dismiss all removes its own slot: focus drops to the first account row.
        f.focus = Some(Slot::Dismiss);
        f.waiting.clear();
        keep_focus_valid(&mut f);
        assert_eq!(f.focus, Some(Slot::Row(0)));
    }
}
