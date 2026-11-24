//! Settings file (hive.json next to the exe).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Account {
    pub name: String,
    pub purpose: String,
    pub email: String,
    /// The normal Claude profile (no --user-data-dir). Only one account should have this.
    pub default_profile: bool,
    /// Opened by "--startup" (Windows sign-in), in list order.
    pub startup: bool,
    /// Reopen it if it closes while the tray app is running.
    pub keep_running: bool,
    /// Weekly limit reset from Claude's Settings > Usage, e.g. "Tue 2:00 AM".
    /// "" = work it out from the usage history.
    pub weekly_reset: String,
    /// Keys this version doesn't know, kept so a save never drops them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            name: String::new(),
            purpose: String::new(),
            email: String::new(),
            default_profile: false,
            startup: true,
            keep_running: false,
            weekly_reset: String::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct DriveWait {
    pub enabled: bool,
    /// Folder that must be readable before startup accounts open.
    pub path: String,
    /// 0 = wait until it appears (the tray menu has "Open now").
    pub timeout_minutes: u32,
}

impl Default for DriveWait {
    fn default() -> Self {
        Self { enabled: true, path: guess_drive_path(), timeout_minutes: 0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub accounts: Vec<Account>,
    /// Account that Claude in Chrome is handed back to after windows open. "" = leave it.
    pub chrome_account: String,
    pub wait_for_drive: DriveWait,
    pub launch_delay_seconds: u32,
    /// Where extra profiles live. "" = %USERPROFILE%\ClaudeProfiles
    pub profiles_root: String,
    /// Claude.exe to use. "" = find the Store app automatically.
    pub claude_exe: String,
    /// Shortcut that opens the flyout from anywhere, e.g. "Ctrl+Alt+Shift+C". "" = none.
    pub hotkey: String,
    /// More shortcuts, off unless set: "account:<name>" (bring that account
    /// forward, or open it), "use_next", "open_all" -> e.g. "Ctrl+Alt+1".
    pub hotkeys: BTreeMap<String, String>,
    /// The Settings tab last shown (0 General, 1 Usage, 2 Alerts, 3 Shortcuts).
    pub settings_tab: u8,
    /// Codex usage row (reads Codex's own session logs).
    pub codex: CodexCfg,
    /// Notify when an account's usage crosses this percent (0 = off).
    pub alert_percent: u8,
    /// "Use next" picks the account whose week resets soonest among those still
    /// below this share used (of both the week and the 5-hour limit).
    pub use_next_below: u8,
    /// Which usage bars each row shows and which comes first (and is drawn big):
    /// "week_5h" (default), "week", "5h_week" or "5h".
    pub usage_bars: String,
    /// Sound with this app's notifications: "soft" (a quiet chime), "windows" (the usual sound) or "none".
    pub alert_sound: String,
    /// How long alerts stay up: "windows" (Windows' own brief notification), "30", "120"
    /// (seconds on this app's alert card) or "until" (the card stays until closed).
    pub alert_card: String,
    /// "card" (this app's own alert card) or "windows" (Windows' brief notification).
    pub alert_style: String,
    /// Which monitor alert cards sit on: its device name (for example DISPLAY2). "" = the primary monitor.
    pub card_monitor: String,
    /// That monitor's left-to-right position, the fallback when its device name changed.
    pub card_monitor_index: Option<usize>,
    /// Which corner alert cards stack from: "top_left", "top_right", "bottom_left" or "bottom_right".
    pub card_corner: String,
    /// Daily usage summary notification, e.g. "9:00 AM". "" = off.
    pub daily_summary: String,
    /// Keep Claude's usage check running (nudge.rs): "" off, "away" whenever you're
    /// away from the keyboard, or a time like "3:00 AM" (that hour, if you're away).
    pub usage_nudge: String,
    /// Flyout pinned: stays open as a normal window with a taskbar button.
    pub pinned: bool,
    pub pinned_at: Option<[i32; 2]>,
    /// Alert when a Code chat finishes or needs you (needs the hooks from --hook-entry).
    pub notify_chats: bool,
    /// Only alert when a chat needs you (permission prompt, question), not on every finished turn.
    pub notify_only_waiting: bool,
    /// Alert when a chat finishes a turn. None: follow the two older keys above.
    pub alert_finished: Option<bool>,
    /// Alert when a chat needs you. None: follow the older keys above.
    pub alert_needs_me: Option<bool>,
    /// A reply that ends with a question (or an "Asks" list) counts as needing you.
    pub alert_questions: bool,
    /// Close a chat's alert card once you open that chat yourself (Claude chats).
    pub close_card_on_open: bool,
    /// A small name tag on each open Claude window, so you can tell them apart.
    pub show_tags: bool,
    /// One taskbar button (with its own colored icon) per account, instead of all under Claude's.
    pub separate_taskbar: bool,
    /// Move a window back to its saved spot when this app opens it.
    pub restore_on_open: bool,
    /// Where each account's window sat when "Save layout" was last used.
    pub layout: Layout,
    pub org_names: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            chrome_account: String::new(),
            wait_for_drive: DriveWait::default(),
            launch_delay_seconds: 6,
            profiles_root: String::new(),
            claude_exe: String::new(),
            hotkey: "Ctrl+Alt+Shift+C".into(),
            hotkeys: BTreeMap::new(),
            settings_tab: 0,
            codex: CodexCfg::default(),
            alert_percent: 90,
            use_next_below: 95,
            usage_bars: "week_5h".into(),
            alert_sound: "soft".into(),
            alert_card: "until".into(),
            alert_style: "card".into(),
            card_monitor: String::new(),
            card_monitor_index: None,
            card_corner: "bottom_right".into(),
            daily_summary: "9:00 AM".into(),
            usage_nudge: String::new(),
            pinned: false,
            pinned_at: None,
            notify_chats: true,
            notify_only_waiting: false,
            alert_finished: None,
            alert_needs_me: None,
            alert_questions: true,
            close_card_on_open: true,
            show_tags: true,
            separate_taskbar: true,
            restore_on_open: true,
            layout: Layout::default(),
            org_names: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

/// Saved window positions, one slot per account.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Layout {
    /// When it was saved, for the flyout's tooltip. "" = never saved.
    pub saved_at: String,
    pub slots: BTreeMap<String, Slot>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Slot {
    /// Display the window was on, e.g. "\\.\DISPLAY2".
    pub monitor: String,
    /// That display's rectangle when saved (left, top, right, bottom), to find it again if its name changed.
    pub monitor_rect: [i32; 4],
    pub maximized: bool,
    /// Window rectangle relative to the display's top-left corner.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CodexCfg {
    pub enabled: bool,
    /// Codex home folder. "" = %USERPROFILE%\.codex
    pub home: String,
    /// App to open from the Codex row.
    pub app_id: String,
    /// You turned Codex alerts on: keep this app in Codex's notify setting (re-added if a Codex update drops it).
    pub alerts: bool,
    /// More Codex sign-ins, each in its own window and Codex folder. The main one
    /// above (row "Codex") stays the app's normal window and ~/.codex.
    pub accounts: Vec<CodexAccount>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CodexAccount {
    pub name: String,
    pub purpose: String,
    /// Opened by "--startup" (Windows sign-in), after the Claude accounts.
    pub startup: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for CodexAccount {
    fn default() -> Self {
        Self { name: String::new(), purpose: String::new(), startup: false, extra: BTreeMap::new() }
    }
}

impl Default for CodexCfg {
    fn default() -> Self {
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join(".codex");
        Self { enabled: home.join("sessions").is_dir(), home: String::new(), app_id: "OpenAI.Codex_2p2nqsd0c76g0!App".into(), alerts: false, accounts: Vec::new() }
    }
}

impl Config {
    /// Alert when a chat finishes a turn.
    pub fn alerts_finished(&self) -> bool {
        self.alert_finished.unwrap_or(self.notify_chats && !self.notify_only_waiting)
    }

    /// Alert when a chat needs you (a question or a permission prompt).
    pub fn alerts_needs_me(&self) -> bool {
        self.alert_needs_me.unwrap_or(self.notify_chats)
    }

    /// Flip one of the two chat alerts; both are then stored explicitly.
    pub fn toggle_alert(&mut self, finished: bool) {
        let (mut f, mut n) = (self.alerts_finished(), self.alerts_needs_me());
        if finished {
            f = !f;
        } else {
            n = !n;
        }
        self.alert_finished = Some(f);
        self.alert_needs_me = Some(n);
    }

    pub fn find(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.name.eq_ignore_ascii_case(name))
    }

    /// One of the extra Codex sign-ins.
    pub fn find_codex(&self, name: &str) -> Option<&CodexAccount> {
        self.codex.accounts.iter().find(|a| a.name.eq_ignore_ascii_case(name))
    }

    /// Every name that gets a tag color: Claude accounts, then (with extra Codex
    /// sign-ins) the main "Codex" and each extra one.
    pub fn tag_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.accounts.iter().map(|a| a.name.as_str()).collect();
        if !self.codex.accounts.is_empty() {
            v.push("Codex");
            v.extend(self.codex.accounts.iter().map(|a| a.name.as_str()));
        }
        v
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.name.eq_ignore_ascii_case(name))
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("{}: {e}", path.display()))?;
        // Hand edits: percentages past 100 mean "always".
        cfg.alert_percent = cfg.alert_percent.min(100);
        cfg.use_next_below = cfg.use_next_below.min(100);
        Ok(cfg)
    }

    /// Load the settings file, creating it from the old PowerShell launcher's
    /// claude-profiles.json the first time.
    pub fn load_or_migrate(path: &Path) -> Result<Config, String> {
        if path.exists() {
            return Self::load(path);
        }
        let cfg = migrate(&path.with_file_name("claude-profiles.json"));
        cfg.save(path)?;
        crate::log(&format!("created {} with {} accounts", path.display(), cfg.accounts.len()));
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Own temp file per process: the tray and a CLI run may save at the same moment.
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }
}

/// The first Google Drive "My Drive" folder found (Drive mounts it as a drive letter).
fn guess_drive_path() -> String {
    "DEFGHIJKLMNOPQRSTUVWXYZ"
        .chars()
        .map(|c| format!("{c}:\\My Drive"))
        .find(|p| Path::new(p).is_dir())
        .unwrap_or_else(|| "G:\\My Drive".into())
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct OldConfig {
    profiles: Vec<OldProfile>,
    org_names: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OldProfile {
    name: String,
    purpose: String,
    email: String,
}

fn migrate(old_path: &Path) -> Config {
    let old: OldConfig = std::fs::read_to_string(old_path)
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default();

    let mut cfg = Config { org_names: old.org_names, ..Config::default() };
    let mut names: Vec<(String, String, String)> =
        old.profiles.into_iter().map(|p| (p.name, p.purpose, p.email)).collect();
    if names.is_empty() {
        names.push(("Personal".into(), String::new(), String::new()));
    }
    // The old "All" also opened every folder under ClaudeProfiles.
    let root: PathBuf = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("ClaudeProfiles");
    if let Ok(dirs) = std::fs::read_dir(&root) {
        for d in dirs.flatten() {
            let n = d.file_name().to_string_lossy().to_string();
            if d.path().is_dir() && !n.contains(".before-") && !n.starts_with('.')
                && !names.iter().any(|(x, _, _)| x.eq_ignore_ascii_case(&n))
            {
                names.push((n, String::new(), String::new()));
            }
        }
    }
    for (name, purpose, email) in names {
        let is_team = name.eq_ignore_ascii_case("Team");
        cfg.accounts.push(Account {
            default_profile: name.eq_ignore_ascii_case("Personal"),
            keep_running: is_team,
            name,
            purpose,
            email,
            ..Account::default()
        });
    }
    // Old order: Personal first, Team last (Claude in Chrome ends on Team).
    cfg.accounts.sort_by_key(|a| (!a.default_profile, a.name.eq_ignore_ascii_case("Team")));
    if cfg.find("Team").is_some() {
        cfg.chrome_account = "Team".into();
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_survive_a_save() {
        let text = r#"{"accounts":[{"name":"Work","color":"blue"}],"my_note":"keep me","launch_delay_seconds":3}"#;
        let cfg: Config = serde_json::from_str(text).unwrap();
        assert_eq!(cfg.launch_delay_seconds, 3);
        assert!(cfg.accounts[0].startup, "missing startup defaults to true");
        let back = serde_json::to_string(&cfg).unwrap();
        assert!(back.contains("\"my_note\":\"keep me\""), "{back}");
        assert!(back.contains("\"color\":\"blue\""), "{back}");
    }

    #[test]
    fn card_position_defaults_and_round_trips() {
        // An old file without the new keys: primary monitor, bottom right.
        let old: Config = serde_json::from_str(r#"{"accounts":[]}"#).unwrap();
        assert_eq!(old.card_monitor, "");
        assert_eq!(old.card_monitor_index, None);
        assert_eq!(old.card_corner, "bottom_right");
        let mut cfg = Config::default();
        cfg.card_monitor = "DISPLAY3".into();
        cfg.card_monitor_index = Some(2);
        cfg.card_corner = "bottom_left".into();
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.card_monitor, "DISPLAY3");
        assert_eq!(back.card_monitor_index, Some(2));
        assert_eq!(back.card_corner, "bottom_left");
    }

    #[test]
    fn old_launcher_file_migrates() {
        let dir = std::env::temp_dir().join(format!("ca-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("claude-profiles.json");
        std::fs::write(&old, "\u{feff}{\"profiles\":[{\"name\":\"Team\",\"purpose\":\"p\",\"email\":\"e@x\"},{\"name\":\"Personal\"}],\"orgNames\":{\"abc\":\"Org\"}}").unwrap();
        let cfg = migrate(&old);
        let names: Vec<_> = cfg.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names.first(), Some(&"Personal"));
        assert_eq!(names.last(), Some(&"Team"));
        assert!(cfg.find("personal").unwrap().default_profile);
        assert!(cfg.find("Team").unwrap().keep_running);
        assert_eq!(cfg.chrome_account, "Team");
        assert_eq!(cfg.org_names.get("abc").map(String::as_str), Some("Org"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bom_and_broken_files() {
        let dir = std::env::temp_dir().join(format!("ca-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("hive.json");
        std::fs::write(&p, "\u{feff}{\"accounts\":[]}").unwrap();
        assert!(Config::load(&p).is_ok());
        std::fs::write(&p, "{\"accounts\":[},").unwrap();
        assert!(Config::load(&p).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
