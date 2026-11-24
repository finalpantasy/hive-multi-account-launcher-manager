//! Everything that touches Claude Desktop: finding it, opening a profile,
//! telling whether a profile is open, which org it is signed in to, and
//! which profile Claude in Chrome talks to.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_SHARING_VIOLATION, GENERIC_WRITE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::Storage::Packaging::Appx::{
    GetPackagePathByFullName, GetPackagesByPackageFamily,
};

use crate::config::{Account, Config};
use crate::time;
use crate::win::{reg_get, reg_set, wide, wide_path};

pub const PACKAGE_FAMILY: &str = "Claude_pzs8sxrjxfjjc";
const HOST: &str = "com.anthropic.claude_browser_extension";
const BROWSERS: [&str; 2] = ["Google\\Chrome", "Microsoft\\Edge"];

/// Claude Code's transcripts, shared by every account (~/.claude/projects).
pub fn projects_dir() -> PathBuf {
    env_path("USERPROFILE").join(".claude").join("projects")
}

fn env_path(k: &str) -> PathBuf {
    std::env::var_os(k).map(PathBuf::from).unwrap_or_default()
}

pub fn profiles_root(cfg: &Config) -> PathBuf {
    let r = cfg.profiles_root.trim();
    if r.is_empty() { env_path("USERPROFILE").join("ClaudeProfiles") } else { PathBuf::from(r) }
}

/// The Store app keeps the normal profile in its LocalCache; %APPDATA%\Claude
/// is only what the app itself sees.
fn default_dirs() -> Vec<PathBuf> {
    vec![
        env_path("LOCALAPPDATA").join("Packages").join(PACKAGE_FAMILY).join("LocalCache").join("Roaming").join("Claude"),
        env_path("APPDATA").join("Claude"),
    ]
}

pub fn data_dir(cfg: &Config, a: &Account) -> PathBuf {
    if a.default_profile {
        default_dirs().into_iter().find(|d| d.exists()).unwrap_or_else(|| env_path("APPDATA").join("Claude"))
    } else {
        profiles_root(cfg).join(&a.name)
    }
}

/// A running Claude holds <profile>\lockfile open for writing without sharing.
fn lock_held(dir: &Path) -> bool {
    let p = dir.join("lockfile");
    if !p.exists() {
        return false;
    }
    let name = wide_path(&p);
    unsafe {
        let h = CreateFileW(
            name.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return GetLastError() == ERROR_SHARING_VIOLATION;
        }
        CloseHandle(h);
        false
    }
}

pub fn is_running(cfg: &Config, a: &Account) -> bool {
    if a.default_profile {
        default_dirs().iter().any(|d| lock_held(d))
    } else {
        lock_held(&data_dir(cfg, a))
    }
}

fn log_file(cfg: &Config, a: &Account) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if a.default_profile {
        candidates.push(env_path("LOCALAPPDATA").join("Claude").join("Logs").join("main.log"));
        candidates.extend(default_dirs().into_iter().map(|d| d.join("logs").join("main.log")));
    } else {
        candidates.push(data_dir(cfg, a).join("logs").join("main.log"));
    }
    candidates
        .into_iter()
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

pub enum Org {
    Unknown,
    SignedOut,
    Id(String),
}

/// Last org the window used, read from the end of its main.log.
pub fn org(cfg: &Config, a: &Account) -> Org {
    const TAIL: u64 = 256 * 1024;
    let Some(path) = log_file(cfg, a) else { return Org::Unknown };
    let Ok(mut f) = std::fs::File::open(&path) else { return Org::Unknown };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).is_err() {
        return Org::Unknown;
    }
    let mut buf = Vec::new();
    let _ = f.by_ref().take(TAIL).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let org_at = text.rfind("lookup orgId=");
    let null_at = text.rfind("accountId=null, orgId=null");
    match (org_at, null_at) {
        (Some(o), n) if n.map_or(true, |n| n < o) => {
            let id: String = text[o + "lookup orgId=".len()..].chars().take(36).collect();
            if id.len() == 36 { Org::Id(id) } else { Org::Unknown }
        }
        (_, Some(_)) => Org::SignedOut,
        _ => Org::Unknown,
    }
}

pub fn org_label(cfg: &Config, id: &str) -> String {
    cfg.org_names.get(id).cloned().unwrap_or_else(|| format!("org {}", id.chars().take(8).collect::<String>()))
}

/// (running, short status text) for menus and --status.
pub fn status(cfg: &Config, a: &Account) -> (bool, String) {
    if !is_running(cfg, a) {
        return (false, "closed".into());
    }
    let text = match org(cfg, a) {
        Org::Id(id) => org_label(cfg, &id),
        Org::SignedOut => "not signed in".into(),
        Org::Unknown => "open".into(),
    };
    (true, text)
}

/// Folders that hold this account's profile files (the default profile can live in two places).
pub fn profile_dirs(cfg: &Config, a: &Account) -> Vec<PathBuf> {
    if a.default_profile {
        default_dirs().into_iter().filter(|d| d.exists()).collect()
    } else {
        vec![data_dir(cfg, a)]
    }
}

pub struct Usage {
    /// Percent of the 5-hour limit used; None once that window has reset.
    pub five_hour: Option<u8>,
    pub week: u8,
    /// Next weekly reset and how sure we are about it (seconds either way).
    pub week_reset: Option<(u64, u64)>,
    /// Rough end of the current 5-hour window.
    pub five_reset: Option<u64>,
    /// Weekly % over the last 7 days (oldest first), for the trend line.
    pub trend: Vec<u8>,
    /// When Claude last checked (the newest sample).
    pub as_of: u64,
}

struct Sample {
    t: u64,
    fh: u8,
    sd: u8,
}

/// This org's samples from plan-usage-history.json, oldest first (seconds).
fn samples(cfg: &Config, a: &Account, org: Option<&str>) -> Vec<Sample> {
    let Some(path) = profile_dirs(cfg, a).into_iter().map(|d| d.join("plan-usage-history.json")).find(|p| p.exists()) else {
        return Vec::new();
    };
    let Some(v) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else {
        return Vec::new();
    };
    let mut out: Vec<Sample> = v
        .get("samples")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|s| org.map_or(true, |o| s.get("org").and_then(|x| x.as_str()) == Some(o)))
                .filter_map(|s| {
                    let u = s.get("u")?;
                    let pct = |k: &str| u.get(k).and_then(|x| x.as_u64()).map(|x| x.min(100) as u8);
                    Some(Sample { t: s.get("t")?.as_u64()? / 1000, fh: pct("fh").unwrap_or(0), sd: pct("sd")? })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|s| s.t);
    out
}

/// The weekly limit resets at the same moment every week. Each big drop in the
/// weekly % brackets that moment between two samples; lining the brackets up
/// week over week narrows it down. Returns (estimate, uncertainty) for the next reset.
fn weekly_reset(s: &[Sample]) -> Option<(u64, u64)> {
    let drops: Vec<(u64, u64)> = s
        .windows(2)
        .filter(|w| w[0].sd >= w[1].sd + 15 && w[1].sd <= w[0].sd / 2 && w[1].t - w[0].t < time::WEEK)
        .map(|w| (w[0].t, w[1].t))
        .collect();
    let &(mut lo, mut hi) = drops.last()?;
    for &(a, b) in drops.iter().rev().skip(1) {
        let k = (lo.saturating_sub(a) + time::WEEK / 2) / time::WEEK;
        let (a, b) = (a + k * time::WEEK, b + k * time::WEEK);
        let (nlo, nhi) = (lo.max(a), hi.min(b));
        if nlo <= nhi {
            lo = nlo;
            hi = nhi;
        }
    }
    let mut at = lo + (hi - lo) / 2;
    while at <= time::now() {
        at += time::WEEK;
    }
    Some((at, (hi - lo) / 2))
}

/// The 5-hour window starts with the first use after it lapsed: find where the
/// current run of rising samples began.
fn five_hour_reset(s: &[Sample]) -> Option<u64> {
    let last = s.last()?;
    if last.fh == 0 || time::now().saturating_sub(last.t) > 5 * time::HOUR {
        return None;
    }
    let mut k = s.len() - 1;
    while k > 0 && s[k - 1].fh > 0 && s[k - 1].fh <= s[k].fh && s[k].t - s[k - 1].t < 5 * time::HOUR {
        k -= 1;
    }
    let at = s[k].t + 5 * time::HOUR;
    (at > time::now()).then_some(at)
}

/// Latest usage for the org the window is signed in to, plus reset times and pace.
pub fn usage(cfg: &Config, a: &Account, org: Option<&str>) -> Option<Usage> {
    let s = samples(cfg, a, org);
    let last = s.last()?;
    let minutes_old = time::now().saturating_sub(last.t) / 60;
    if minutes_old > 7 * 24 * 60 {
        return None;
    }
    let manual = time::next_weekly(&a.weekly_reset).map(|t| (t, 0));
    let week_reset = manual.or_else(|| weekly_reset(&s));
    let since = time::now().saturating_sub(time::WEEK);
    let recent: Vec<u8> = s.iter().filter(|x| x.t >= since).map(|x| x.sd).collect();
    let step = recent.len().div_ceil(48).max(1);
    let trend = recent.iter().step_by(step).copied().chain(recent.last().copied()).collect();
    let five_reset = five_hour_reset(&s);
    // The window's own end (found from where its run of samples began) can come before
    // the last sample is five hours old: from then on its last number no longer counts.
    let five_ended = last.fh > 0 && five_reset.is_none();
    Some(Usage {
        trend,
        five_hour: if minutes_old > 5 * 60 || five_ended { None } else { Some(last.fh) },
        week: last.sd,
        week_reset,
        five_reset,
        as_of: last.t,
    })
}

// ---- keeping Claude's own usage check running ------------------------------
//
// Claude Desktop checks plan usage every 15 minutes, but pauses that check when its
// tray menu hasn't been opened in the last 24 hours (a server-side flag), and reads
// when the menu was last opened only at startup (config.json key
// planUsageLastTrayOpenAt, in ms). So before this app starts a profile, it records
// "just opened": that profile's usage then stays fresh for its first 24 hours.
// Only while the profile is closed: a running Claude rewrites config.json from memory.

const TRAY_OPEN_KEY: &str = "\"planUsageLastTrayOpenAt\"";

/// Before starting a closed profile: mark its tray menu as just opened.
pub fn keep_usage_fresh(cfg: &Config, a: &Account) {
    if is_running(cfg, a) {
        return;
    }
    let path = data_dir(cfg, a).join("config.json");
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    match mark_tray_opened(&path, ms) {
        Ok(()) => crate::log(&format!("usage check kept on for {}", a.name)),
        Err(e) => crate::log(&format!("usage check for {}: {e}", a.name)),
    }
}

/// When Claude last counted its tray as opened (planUsageLastTrayOpenAt, ms), read-only.
pub fn tray_opened_ms(cfg: &Config, a: &Account) -> Option<u64> {
    let text = std::fs::read_to_string(data_dir(cfg, a).join("config.json")).ok()?;
    let i = text.find(TRAY_OPEN_KEY)? + TRAY_OPEN_KEY.len();
    let rest = text[i..].trim_start().strip_prefix(':')?.trim_start();
    rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}

/// The org's usage samples, oldest first: (seconds, 5-hour %, weekly %).
pub fn usage_samples(cfg: &Config, a: &Account, org: Option<&str>) -> Vec<(u64, u8, u8)> {
    samples(cfg, a, org).into_iter().map(|s| (s.t, s.fh, s.sd)).collect()
}

/// Time of the account's newest usage sample (any org), seconds.
pub fn latest_sample(cfg: &Config, a: &Account) -> Option<u64> {
    samples(cfg, a, None).last().map(|s| s.t)
}

/// Set planUsageLastTrayOpenAt in a Claude config.json, changing nothing else in
/// the file (a one-time backup is kept). Never creates the file.
fn mark_tray_opened(path: &Path, ms: u64) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|_| "no config.json yet, skipped".to_string())?;
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(b) => ("\u{feff}", b),
        None => ("", text.as_str()),
    };
    if !serde_json::from_str::<serde_json::Value>(body).is_ok_and(|v| v.is_object()) {
        return Err("config.json doesn't parse, skipped".into());
    }
    let new = match body.find(TRAY_OPEN_KEY) {
        Some(i) => {
            let after_key = i + TRAY_OPEN_KEY.len();
            let colon = after_key + body[after_key..].find(':').ok_or("odd config.json, skipped")?;
            let rest = &body[colon + 1..];
            let start = colon + 1 + (rest.len() - rest.trim_start().len());
            let digits = body[start..].chars().take_while(|c| c.is_ascii_digit()).count();
            if digits == 0 {
                return Err("planUsageLastTrayOpenAt isn't a number, skipped".into());
            }
            format!("{}{ms}{}", &body[..start], &body[start + digits..])
        }
        None => {
            // A new first key, indented like the file's own keys.
            let open = body.find('{').ok_or("odd config.json, skipped")?;
            let after = &body[open + 1..];
            let line = after.trim_start_matches(['\r', '\n']);
            let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let nl = if body.contains("\r\n") { "\r\n" } else { "\n" };
            let sep = if after.trim().starts_with('}') { "" } else { "," };
            format!("{}{nl}{indent}{TRAY_OPEN_KEY}: {ms}{sep}{}", &body[..=open], after)
        }
    };
    let check: serde_json::Value = serde_json::from_str(&new).map_err(|_| "edit wouldn't parse, skipped".to_string())?;
    if check.get("planUsageLastTrayOpenAt").and_then(|v| v.as_u64()) != Some(ms) {
        return Err("edit didn't take, skipped".into());
    }
    let backup = path.with_extension("json.before-usage-check");
    if !backup.exists() {
        std::fs::copy(path, &backup).map_err(|e| format!("couldn't back up config.json: {e}"))?;
    }
    let tmp = path.with_extension("json.usage-check.tmp");
    std::fs::write(&tmp, format!("{bom}{new}")).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

// ---- finding and starting Claude ----------------------------------------

fn alias_path() -> PathBuf {
    env_path("LOCALAPPDATA").join("Microsoft").join("WindowsApps").join("claude-desktop.exe")
}

/// Claude.exe inside the newest installed Store package.
fn package_exe() -> Option<PathBuf> {
    package_exe_of(PACKAGE_FAMILY, "Claude.exe")
}

/// `app\<exe>` inside the newest installed version of a Store package family.
pub fn package_exe_of(family: &str, exe_name: &str) -> Option<PathBuf> {
    let family = wide(family);
    let mut count = 0u32;
    let mut len = 0u32;
    unsafe {
        let rc = GetPackagesByPackageFamily(family.as_ptr(), &mut count, null_mut(), &mut len, null_mut());
        if rc != ERROR_INSUFFICIENT_BUFFER || count == 0 {
            return None;
        }
        let mut names = vec![null_mut::<u16>(); count as usize];
        let mut buf = vec![0u16; len as usize];
        if GetPackagesByPackageFamily(family.as_ptr(), &mut count, names.as_mut_ptr(), &mut len, buf.as_mut_ptr()) != 0 {
            return None;
        }
        let mut best: Option<(Vec<u64>, PathBuf)> = None;
        for &name in names.iter().take(count as usize) {
            let mut plen = 0u32;
            GetPackagePathByFullName(name, &mut plen, null_mut());
            if plen == 0 {
                continue;
            }
            let mut pbuf = vec![0u16; plen as usize];
            if GetPackagePathByFullName(name, &mut plen, pbuf.as_mut_ptr()) != 0 {
                continue;
            }
            let exe = PathBuf::from(crate::win::from_wide(&pbuf)).join("app").join(exe_name);
            let full = {
                let mut n = 0;
                while *name.add(n) != 0 {
                    n += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(name, n))
            };
            // Claude_2.37337.1.0_x64__pzs8sxrjxfjjc, OpenAI.Codex_26.1007.9446.0_x64__...
            let version: Vec<u64> = full.split('_').nth(1).unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect();
            if best.as_ref().map_or(true, |(v, _)| version > *v) {
                best = Some((version, exe));
            }
        }
        best.map(|(_, exe)| exe)
    }
}

pub fn claude_exe(cfg: &Config) -> Option<PathBuf> {
    let custom = cfg.claude_exe.trim();
    if !custom.is_empty() && Path::new(custom).exists() {
        return Some(PathBuf::from(custom));
    }
    package_exe().or_else(|| {
        let alias = alias_path();
        std::fs::symlink_metadata(&alias).is_ok().then_some(alias)
    })
}

/// Drop CLAUDE*/ANTHROPIC* variables so a value from a running Claude session
/// can never leak into a new window.
fn clean_env(cmd: &mut Command) {
    for (k, _) in std::env::vars_os() {
        let upper = k.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("CLAUDE") || upper.starts_with("ANTHROPIC") {
            cmd.env_remove(&k);
        }
    }
}

/// Open the account's window. If it is already open, Claude just shows it.
pub fn launch(cfg: &Config, a: &Account) -> Result<(), String> {
    launch_with(cfg, a, &[])
}

/// Open the account's window and hand it extra arguments (e.g. a claude:// link).
pub fn launch_with(cfg: &Config, a: &Account, extra: &[String]) -> Result<(), String> {
    keep_usage_fresh(cfg, a);
    let missing = || "Couldn't find Claude Desktop. Set claude_exe in the settings file.".to_string();
    let mut cmd;
    if a.default_profile {
        let alias = alias_path();
        cmd = if std::fs::symlink_metadata(&alias).is_ok() {
            Command::new(alias)
        } else {
            Command::new(claude_exe(cfg).ok_or_else(missing)?)
        };
        clean_env(&mut cmd);
    } else {
        let exe = claude_exe(cfg).ok_or_else(missing)?;
        let dir = data_dir(cfg, a);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        cmd = Command::new(exe);
        clean_env(&mut cmd);
        cmd.arg(format!("--user-data-dir={}", dir.display()));
        cmd.env("CLAUDE_USER_DATA_DIR", &dir);
    }
    cmd.args(extra);
    cmd.current_dir(env_path("USERPROFILE"));
    // Claude must not inherit our console or output pipe: a caller capturing
    // our output would otherwise wait until the Claude window closes.
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub last: u64,
    pub archived: bool,
    /// The Claude Code conversation behind it (a uuid), shared by every account.
    pub cli: String,
}

/// A JSON string or number value that follows `"key":` in `text`.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let i = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    Some(text[i..].trim_start())
}

/// Chat record files in this account's sidebar folder, with their size and time.
/// Only the folder the app really uses: the default profile has a stale copy too.
fn chat_files(cfg: &Config, a: &Account) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let root = data_dir(cfg, a).join("claude-code-sessions");
    std::fs::read_dir(&root).into_iter().flatten().flatten() // <account>
        .flat_map(|acct| std::fs::read_dir(acct.path()).into_iter().flatten().flatten()) // <org>
        .flat_map(|org| std::fs::read_dir(org.path()).into_iter().flatten().flatten())
        .filter(|e| e.file_name().to_string_lossy().starts_with("local_") && e.path().extension().is_some_and(|x| x == "json"))
        // A directory entry already carries size and time: no extra file open.
        .filter_map(|e| e.metadata().ok().map(|m| (e.path(), m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH))))
        .collect()
}

/// One chat record. Each record is large, but the fields needed are almost
/// always in its first couple of KB.
fn read_chat(p: &Path) -> Option<Chat> {
    let f = std::fs::File::open(p).ok()?;
    let mut f = f.take(32 * 1024);
    // The first 4 KB in full (one read may return less).
    let mut buf = Vec::with_capacity(4096);
    let _ = f.by_ref().take(4096).read_to_end(&mut buf);
    let n = buf.len();
    // Each needed field must start with room to spare before the end of what was read,
    // or its value may be cut off (and a cut-off read would be cached as no chat).
    let ends_by = |b: &[u8], k: &str| b.windows(k.len()).position(|w| w == k.as_bytes()).map(|i| i + k.len() + 512);
    let complete = ["\"title\":", "\"lastActivityAt\":"].iter().all(|k| ends_by(&buf, k).is_some_and(|e| e <= n));
    if n == 4096 && !complete {
        let _ = f.read_to_end(&mut buf); // a long record: look further (up to 32 KB)
    }
    let text = String::from_utf8_lossy(&buf);
    let title = field(&text, "title")
        .and_then(|v| serde_json::Deserializer::from_str(v).into_iter::<String>().next())
        .and_then(|r| r.ok())
        .unwrap_or_default();
    if title.trim().is_empty() {
        return None;
    }
    let last = field(&text, "lastActivityAt")
        .map(|v| v.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .and_then(|d| d.parse::<u64>().ok())
        .unwrap_or(0)
        / 1000;
    let archived = field(&text, "isArchived").is_some_and(|v| v.starts_with("true"));
    let cli = field(&text, "cliSessionId")
        .and_then(|v| serde_json::Deserializer::from_str(v).into_iter::<String>().next())
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let id = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    Some(Chat { id, title, last, archived, cli })
}

/// Chat records already read, per account: a file is read again only when its size or time changes.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct ChatCache {
    accounts: std::collections::HashMap<String, std::collections::HashMap<PathBuf, (u64, std::time::SystemTime, Option<Chat>)>>,
    /// Some record was read again (or one went away) since the last save.
    #[serde(skip)]
    pub changed: bool,
    /// 2: records cached as "no chat" by version 1 may have been a cut-off read.
    #[serde(default)]
    version: u32,
}

const CHAT_CACHE_VERSION: u32 = 2;

impl ChatCache {
    /// The cache saved by an earlier run, so a restart reads only the records that
    /// changed instead of every one (a cold disk made that take 20 s).
    pub fn load(path: &Path) -> ChatCache {
        let mut c: ChatCache = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if c.version < CHAT_CACHE_VERSION {
            // Read the "no chat" records again (a few); keep the rest.
            for files in c.accounts.values_mut() {
                files.retain(|_, (_, _, chat)| chat.is_some());
            }
            c.version = CHAT_CACHE_VERSION;
            c.changed = true;
        }
        c
    }

    pub fn save(&mut self, path: &Path) {
        if !self.changed {
            return;
        }
        let Ok(text) = serde_json::to_vec(self) else { return };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_ok() {
            self.changed = false;
        }
    }
}

/// Code-tab chats in this account's sidebar (newest first).
pub fn chats(cfg: &Config, a: &Account) -> Vec<Chat> {
    chats_cached(cfg, a, &mut ChatCache::default())
}

pub fn chats_cached(cfg: &Config, a: &Account, cache: &mut ChatCache) -> Vec<Chat> {
    chats_cached_with(cfg, a, cache, &mut || {})
}

/// `chats_cached`, calling `each_file` before every chat file (a slow warm-up uses it
/// to notice that a search is waiting).
pub fn chats_cached_with(cfg: &Config, a: &Account, cache: &mut ChatCache, each_file: &mut dyn FnMut()) -> Vec<Chat> {
    let old = cache.accounts.remove(&a.name).unwrap_or_default();
    let mut fresh = std::collections::HashMap::new();
    let mut out = Vec::new();
    for (p, len, time) in chat_files(cfg, a) {
        each_file();
        let chat = match old.get(&p) {
            Some((l, t, c)) if *l == len && *t == time => c.clone(),
            _ => {
                cache.changed = true;
                read_chat(&p)
            }
        };
        out.extend(chat.clone());
        fresh.insert(p, (len, time, chat));
    }
    if fresh.len() != old.len() {
        cache.changed = true; // records went away
    }
    cache.accounts.insert(a.name.clone(), fresh);
    out.sort_by(|a, b| b.last.cmp(&a.last));
    out
}

/// Title of one chat, reading only its own record.
pub fn chat_title(cfg: &Config, a: &Account, session: &str) -> Option<String> {
    let name = format!("{session}.json");
    let root = data_dir(cfg, a).join("claude-code-sessions");
    std::fs::read_dir(root).ok()?.flatten()
        .flat_map(|acct| std::fs::read_dir(acct.path()).into_iter().flatten().flatten())
        .map(|org| org.path().join(&name))
        .find(|p| p.is_file())
        .and_then(|p| read_chat(&p))
        .map(|c| c.title)
}

/// When Claude last showed each of the account's open chats (record's lastFocusedAt,
/// ms), read from the start of each record only.
pub fn focus_times(cfg: &Config, a: &Account) -> Vec<(String, u64)> {
    chat_files(cfg, a)
        .into_iter()
        .filter_map(|(p, _, _)| {
            let mut head = vec![0u8; 8192];
            let n = std::fs::File::open(&p).ok()?.read(&mut head).ok()?;
            let text = String::from_utf8_lossy(&head[..n]);
            let ms = field(&text, "lastFocusedAt")?.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
            Some((p.file_stem()?.to_string_lossy().to_string(), ms))
        })
        .collect()
}

/// When one chat last had the focus in its account's window (ms), from its record.
/// Reads that one record only, so it is cheap to ask often.
pub fn focused_at(cfg: &Config, a: &Account, id: &str) -> Option<u64> {
    if !id.starts_with("local_") || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    let file = format!("{id}.json");
    let root = data_dir(cfg, a).join("claude-code-sessions");
    let path = std::fs::read_dir(&root).ok()?.flatten()
        .flat_map(|acct| std::fs::read_dir(acct.path()).into_iter().flatten().flatten())
        .map(|org| org.path().join(&file))
        .find(|p| p.is_file())?;
    let mut head = vec![0u8; 8192];
    let n = std::fs::File::open(&path).ok()?.read(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);
    field(&text, "lastFocusedAt")?.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}

/// Did a chat link sent at `sent_ms` land? Some(true): that chat got focus since;
/// Some(false): another chat did instead; None: no chat got focus (it was most
/// likely already showing, which Claude doesn't record again).
pub fn link_landed(times: &[(String, u64)], id: &str, sent_ms: u64) -> Option<bool> {
    let since = sent_ms.saturating_sub(500);
    if times.iter().any(|(i, t)| i == id && *t >= since) {
        return Some(true);
    }
    times.iter().any(|(_, t)| *t >= since).then_some(false)
}

pub fn drive_ready(path: &str) -> bool {
    let p = Path::new(path);
    p.is_dir() && std::fs::read_dir(p).map(|mut it| it.next().is_some()).unwrap_or(false)
}

// ---- Tidy up: finished chats to archive ------------------------------------
//
// Only offered, never archived here: the list goes into a new chat in that
// account, and Claude's own archive does the work there. That archive refuses a
// chat that is running, has work that would be lost, is pinned or is on screen
// (and the same for its side sessions). This app never edits chat files.

/// A finished chat Tidy up offers to archive.
#[derive(Clone, Debug, PartialEq)]
pub struct TidyChat {
    pub id: String,
    pub title: String,
    /// Unix seconds of its last activity.
    pub last: u64,
    /// Claude's own note on where it ended ("Shipped and verified", ...).
    pub detail: String,
}

/// Why the account's other open chats aren't offered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TidyHeld {
    /// Claude hasn't marked it completed (blocked, review ready, no status, ...).
    pub not_done: usize,
    /// Completed, but active in the last few days.
    pub recent: usize,
    pub starred: usize,
    pub with_pr: usize,
    /// An alert card for it is up, or it was waiting on you.
    pub waiting: usize,
}

/// Days without activity before a finished chat is offered.
pub const TIDY_IDLE_DAYS: u64 = 3;

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ChatRecord {
    session_id: String,
    title: Option<String>,
    last_activity_at: Option<f64>,
    is_archived: Option<bool>,
    is_starred: Option<bool>,
    prs: Option<Vec<serde_json::Value>>,
    post_turn_summary: Option<TurnSummary>,
    interrupted_awaiting_person: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct TurnSummary {
    status_category: Option<String>,
    status_detail: Option<String>,
}

enum Verdict {
    Offer(TidyChat),
    Archived,
    NotDone,
    Recent,
    Starred,
    WithPr,
    Waiting,
}

/// The strict rules, in order: open, completed by Claude's own account of it, not
/// starred, no linked pull request, nothing waiting on you, idle for `TIDY_IDLE_DAYS`.
fn judge(r: &ChatRecord, now: u64, has_card: bool) -> Verdict {
    if r.is_archived == Some(true) {
        return Verdict::Archived;
    }
    if r.is_starred == Some(true) {
        return Verdict::Starred;
    }
    if r.prs.as_ref().is_some_and(|p| !p.is_empty()) {
        return Verdict::WithPr;
    }
    if has_card || r.interrupted_awaiting_person.as_ref().is_some_and(|v| !v.is_null()) {
        return Verdict::Waiting;
    }
    let status = r.post_turn_summary.as_ref().and_then(|s| s.status_category.as_deref()).unwrap_or("");
    if status != "completed" {
        return Verdict::NotDone;
    }
    let last = r.last_activity_at.map_or(0, |ms| (ms / 1000.0) as u64);
    if last == 0 || now.saturating_sub(last) < TIDY_IDLE_DAYS * time::DAY {
        return Verdict::Recent;
    }
    Verdict::Offer(TidyChat {
        id: r.session_id.clone(),
        title: r.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Untitled chat".into()),
        last,
        detail: r.post_turn_summary.as_ref().and_then(|s| s.status_detail.clone()).unwrap_or_default(),
    })
}

/// The account's finished chats to offer (oldest first) and why the rest stay.
/// `has_card` tells which chat ids have an alert card up. Reads every open chat's
/// record in full: call it off the UI thread.
pub fn tidy_candidates(cfg: &Config, a: &Account, now: u64, has_card: &dyn Fn(&str) -> bool) -> (Vec<TidyChat>, TidyHeld) {
    let mut offer = Vec::new();
    let mut held = TidyHeld::default();
    let current = match org(cfg, a) {
        Org::Id(id) => Some(id),
        Org::SignedOut => return (offer, held),
        Org::Unknown => None,
    };
    for p in in_current_org(chat_files(cfg, a), current.as_deref()) {
        // The cheap read first: most records are archived already.
        if read_chat(&p).is_some_and(|c| c.archived) {
            continue;
        }
        let Ok(text) = std::fs::read(&p) else { continue };
        let Ok(mut r) = serde_json::from_slice::<ChatRecord>(&text) else { continue };
        if r.session_id.is_empty() {
            r.session_id = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        }
        match judge(&r, now, has_card(&r.session_id)) {
            Verdict::Offer(c) => offer.push(c),
            Verdict::Archived => {}
            Verdict::NotDone => held.not_done += 1,
            Verdict::Recent => held.recent += 1,
            Verdict::Starred => held.starred += 1,
            Verdict::WithPr => held.with_pr += 1,
            Verdict::Waiting => held.waiting += 1,
        }
    }
    offer.sort_by_key(|c| c.last);
    (offer, held)
}

/// Only the chats of the organization the app is signed in to: a profile keeps the
/// records of earlier sign-ins in their own org folders, and the app cannot see or
/// archive those. With the org unknown, the folder with the newest record stands in.
fn in_current_org(files: Vec<(PathBuf, u64, std::time::SystemTime)>, org: Option<&str>) -> Vec<PathBuf> {
    let folder = |p: &Path| p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string());
    let current = match org {
        Some(o) => Some(o.to_string()),
        None => files.iter().max_by_key(|(_, _, t)| *t).and_then(|(p, _, _)| folder(p)),
    };
    let Some(current) = current else { return Vec::new() };
    files.into_iter().map(|(p, _, _)| p).filter(|p| folder(p).as_deref() == Some(current.as_str())).collect()
}

/// The prompt for the new chat that does the archiving. Lists each chat by id so
/// nothing else can be picked up.
pub fn tidy_prompt(account: &str, chats: &[TidyChat]) -> String {
    let mut s = format!(
        "Tidy up from Hive ({account}). Archive exactly these {} finished chats with archive_session, one call each, reason \"Tidy up: finished, idle {TIDY_IDLE_DAYS}+ days\". Archive nothing else, and never delete. If the app keeps one back (still running, work that would be lost, pinned or on screen), leave it and say why. Then list what was archived and what was kept.\n",
        chats.len()
    );
    for c in chats {
        let title: String = c.title.chars().filter(|ch| !ch.is_control()).take(60).collect();
        s.push_str(&format!("- {} ({title})\n", c.id));
    }
    s
}

/// A new Code chat in this account with `prompt` filled in, not sent.
pub fn new_chat_link(prompt: &str) -> String {
    let mut q = String::new();
    for b in prompt.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => q.push(*b as char),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    format!("claude://code/new?q={q}")
}

#[cfg(test)]
mod tidy_tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn rec(json: &str) -> ChatRecord {
        serde_json::from_str(json).unwrap()
    }

    fn offered(json: &str, card: bool) -> bool {
        matches!(judge(&rec(json), NOW, card), Verdict::Offer(_))
    }

    #[test]
    fn only_finished_idle_unstarred_chats_without_prs_are_offered() {
        let old = (NOW - 4 * time::DAY) * 1000;
        let done = format!(r#"{{"sessionId":"local_a","title":"T","lastActivityAt":{old},"isArchived":false,"postTurnSummary":{{"status_category":"completed","status_detail":"Shipped"}}}}"#);
        assert!(offered(&done, false));
        assert!(!offered(&done, true), "an alert card is up for it");
        assert!(!offered(&done.replace("completed", "blocked"), false));
        assert!(!offered(&done.replace("completed", "review_ready"), false));
        assert!(!offered(&done.replace(r#""isArchived":false"#, r#""isArchived":false,"isStarred":true"#), false));
        assert!(!offered(&done.replace(r#""isArchived":false"#, r#""isArchived":false,"prs":[{"prNumber":1}]"#), false));
        assert!(!offered(&done.replace(r#""isArchived":false"#, r#""isArchived":false,"interruptedAwaitingPerson":{"x":1}"#), false));
        assert!(!offered(&done.replace(&old.to_string(), &((NOW - 2 * time::DAY) * 1000).to_string()), false), "active 2 days ago");
        assert!(!offered(&done.replace(r#""isArchived":false"#, r#""isArchived":true"#), false));
        assert!(!offered(r#"{"sessionId":"local_b","title":"No status"}"#, false));
        // Nulls where lists or objects usually are still read.
        assert!(offered(&done.replace(r#""isArchived":false"#, r#""isArchived":false,"prs":null,"interruptedAwaitingPerson":null"#), false));
    }

    #[test]
    fn only_the_signed_in_orgs_chats_are_read() {
        let t = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s);
        let f = |org: &str, id: &str, s: u64| (PathBuf::from(format!("acct/{org}/{id}.json")), 0, t(s));
        let files = || vec![f("old", "local_x", 300), f("now", "local_a", 200), f("now", "local_b", 100)];
        let names = |v: Vec<PathBuf>| v.iter().map(|p| p.file_stem().unwrap().to_string_lossy().to_string()).collect::<Vec<_>>();
        assert_eq!(names(in_current_org(files(), Some("now"))), ["local_a", "local_b"]);
        assert_eq!(names(in_current_org(files(), None)), ["local_x"], "unknown org: the folder with the newest record");
        assert!(in_current_org(Vec::new(), None).is_empty());
    }

    #[test]
    fn a_chat_link_landing_is_read_from_focus_times() {
        let t = |v: &[(&str, u64)]| v.iter().map(|(i, t)| (i.to_string(), *t)).collect::<Vec<_>>();
        let sent = 1_000_000;
        assert_eq!(link_landed(&t(&[("a", sent + 900), ("b", sent + 400)]), "a", sent), Some(true));
        assert_eq!(link_landed(&t(&[("a", sent - 60_000), ("b", sent + 400)]), "a", sent), Some(false), "another chat took focus");
        assert_eq!(link_landed(&t(&[("a", sent - 60_000), ("b", sent - 5_000)]), "a", sent), None, "nothing moved: already showing");
    }

    #[test]
    fn the_prompt_names_each_chat_and_the_link_is_encoded() {
        let c = TidyChat { id: "local_1".into(), title: "Fix \"quotes\" & more\nnext".into(), last: 0, detail: String::new() };
        let p = tidy_prompt("Team", &[c]);
        assert!(p.contains("- local_1 (Fix \"quotes\" & morenext)\n"));
        let link = new_chat_link(&p);
        assert!(link.starts_with("claude://code/new?q=Tidy%20up%20from%20Hive%20%28Team%29"));
        assert!(!link.contains(' ') && !link.contains('&') && !link.contains('\n'));
    }
}

// ---- Claude in Chrome ------------------------------------------------------

fn host_key(browser: &str) -> String {
    format!("Software\\{browser}\\NativeMessagingHosts\\{HOST}")
}

fn chrome_manifest(cfg: &Config, a: &Account) -> Option<PathBuf> {
    let dirs = if a.default_profile { default_dirs() } else { vec![data_dir(cfg, a)] };
    dirs.into_iter()
        .map(|d| d.join("ChromeNativeHost").join(format!("{HOST}.json")))
        .find(|p| p.exists())
}

/// Claude 2.42 and later install one shared Chrome connector (Claude-NativeHost) and
/// link Claude in Chrome by the account signed in to the extension. Then there is
/// nothing to switch per account, and a profile's old ChromeNativeHost copy is stale:
/// pointing Chrome at it would break Claude in Chrome.
pub fn chrome_shared() -> bool {
    let in_shared = |p: &str| {
        Path::new(p).parent().and_then(|d| d.file_name()).is_some_and(|n| n.eq_ignore_ascii_case(SHARED_HOST_DIR))
    };
    BROWSERS.iter().filter_map(|b| reg_get(&host_key(b), "")).any(|p| in_shared(&p))
        || default_dirs().iter().filter_map(|d| d.parent()).any(|r| r.join(SHARED_HOST_DIR).join(format!("{HOST}.json")).exists())
}

const SHARED_HOST_DIR: &str = "Claude-NativeHost";
pub const CHROME_SHARED_NOTE: &str = "This Claude version links Claude in Chrome to the account you sign in to in the extension, so there's nothing to switch here.";

/// Does Claude in Chrome currently point at this account?
pub fn chrome_is(cfg: &Config, a: &Account) -> bool {
    if chrome_shared() {
        return false;
    }
    let current = BROWSERS.iter().find_map(|b| reg_get(&host_key(b), ""));
    match (current, chrome_manifest(cfg, a)) {
        (Some(c), Some(m)) => c.eq_ignore_ascii_case(&m.to_string_lossy()),
        _ => false,
    }
}

pub fn set_chrome(cfg: &Config, a: &Account) -> Result<String, String> {
    if chrome_shared() {
        return Err(CHROME_SHARED_NOTE.into());
    }
    let manifest = chrome_manifest(cfg, a)
        .ok_or_else(|| format!("Open {} once first, then try again.", a.name))?;
    let value = manifest.to_string_lossy();
    let done = BROWSERS.iter().filter(|b| reg_set(&host_key(b), "", &value, false)).count();
    if done == 0 {
        Err("Claude in Chrome isn't set up in Chrome or Edge yet.".into())
    } else {
        Ok(format!("Claude in Chrome now connects to {}. Reload the extension if it was open.", a.name))
    }
}

#[cfg(test)]
mod chat_read_tests {
    use super::*;

    #[test]
    fn a_title_cut_off_at_the_first_read_is_still_found() {
        let p = std::env::temp_dir().join(format!("ca-chat-cut-{}.json", std::process::id()));
        // lastActivityAt first, then the title starting just before byte 4096.
        let head = "{\"lastActivityAt\":1792086398855,\"pad\":\"";
        let pad = "x".repeat(4096 - head.len() - 30);
        let text = format!("{head}{pad}\",\"title\":\"A chat whose title crosses the first read\",\"isArchived\":false}}");
        std::fs::write(&p, &text).unwrap();
        let c = read_chat(&p);
        let _ = std::fs::remove_file(&p);
        assert_eq!(c.map(|c| c.title).as_deref(), Some("A chat whose title crosses the first read"));
    }
}

#[cfg(test)]
mod usage_check_tests {
    use super::*;

    fn run(name: &str, text: Option<&str>) -> (Result<(), String>, Option<String>) {
        let dir = std::env::temp_dir().join(format!("ca-usage-check-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        if let Some(t) = text {
            std::fs::write(&p, t).unwrap();
        }
        let r = mark_tray_opened(&p, 1792000000123);
        let after = std::fs::read_to_string(&p).ok();
        let _ = std::fs::remove_dir_all(&dir);
        (r, after)
    }

    #[test]
    fn replaces_only_the_number() {
        let before = "{\n\t\"a\": 1,\n\t\"planUsageLastTrayOpenAt\": 1791065743420,\n\t\"z\": \"x\"\n}";
        let (r, after) = run("replace", Some(before));
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(after.unwrap(), before.replace("1791065743420", "1792000000123"));
    }

    #[test]
    fn adds_the_key_in_the_files_style() {
        let (r, after) = run("add", Some("\u{feff}{\r\n\t\"a\": 1\r\n}"));
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(after.unwrap(), "\u{feff}{\r\n\t\"planUsageLastTrayOpenAt\": 1792000000123,\r\n\t\"a\": 1\r\n}");
    }

    #[test]
    fn leaves_missing_or_broken_files_alone() {
        let (r, after) = run("missing", None);
        assert!(r.is_err() && after.is_none(), "never creates config.json");
        let (r, after) = run("broken", Some("{ \"a\": "));
        assert!(r.is_err());
        assert_eq!(after.unwrap(), "{ \"a\": ");
    }
}

