//! Codex usage, read from the rate-limit record Codex writes into its session
//! logs (~/.codex/sessions/YYYY/MM/DD/*.jsonl). Only the end of the newest
//! files is read; the logs themselves can be hundreds of MB.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};


use crate::config::Config;
use crate::time;

pub struct Window {
    pub minutes: u64,
    pub pct: u8,
    pub resets_at: u64,
}

pub struct Codex {
    pub plan: String,
    pub credits: Option<f64>,
    pub windows: Vec<Window>,
    pub running: bool,
}

pub fn home(cfg: &Config) -> PathBuf {
    let h = cfg.codex.home.trim();
    if h.is_empty() {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join(".codex")
    } else {
        PathBuf::from(h)
    }
}

fn sorted_dirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .map(|it| it.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v
}

/// The newest session logs (last two days that have any).
fn newest_logs(home: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut days = Vec::new();
    'outer: for y in sorted_dirs(&home.join("sessions")) {
        for m in sorted_dirs(&y) {
            for d in sorted_dirs(&m) {
                days.push(d);
                if days.len() == 2 {
                    break 'outer;
                }
            }
        }
    }
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = days
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(4);
    files
}

/// The last answer, by file and size: an unchanged log isn't read again.
static LAST: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, (u64, Option<serde_json::Value>)>>> = std::sync::Mutex::new(None);

/// Last non-null "rate_limits" object in the last 512 KB of a file.
fn last_rate_limits(path: &Path) -> Option<serde_json::Value> {
    let len = std::fs::metadata(path).ok()?.len();
    if let Some((l, v)) = LAST.lock().ok()?.as_ref().and_then(|m| m.get(path)) {
        if *l == len {
            return v.clone();
        }
    }
    let v = scan_rate_limits(path, len);
    if let Ok(mut last) = LAST.lock() {
        let m = last.get_or_insert_with(Default::default);
        if m.len() > 16 {
            m.clear(); // only the newest few logs are ever read
        }
        m.insert(path.to_path_buf(), (len, v.clone()));
    }
    v
}

fn scan_rate_limits(path: &Path, len: u64) -> Option<serde_json::Value> {
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.take(TAIL).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let key = "\"rate_limits\":";
    let mut end = text.len();
    while let Some(i) = text[..end].rfind(key) {
        let rest = &text[i + key.len()..];
        let v = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>().next();
        if let Some(Ok(v)) = v {
            if v.is_object() {
                return Some(v);
            }
        }
        end = i;
    }
    None
}

/// Is the Codex app open? Its window process (ChatGPT.exe in the Codex package), not
/// any codex.exe: the Codex command-line tool from npm is also called codex.exe and
/// kept the row green after the app was closed (2026-10-15).
pub fn running(cfg: &Config) -> bool {
    crate::procs::codex_pids(cfg, &crate::procs::list()).contains_key("Codex")
}

pub fn read(cfg: &Config) -> Option<Codex> {
    read_with(cfg, || running(cfg))
}

/// Same, using a process list the caller already has (saves a second process scan).
pub fn read_in(cfg: &Config, procs: &[crate::procs::Proc]) -> Option<Codex> {
    read_with(cfg, || crate::procs::codex_pids(cfg, procs).contains_key("Codex"))
}

fn read_with(cfg: &Config, is_running: impl FnOnce() -> bool) -> Option<Codex> {
    if !cfg.codex.enabled {
        return None;
    }
    read_home(&home(cfg), is_running)
}

/// Usage of one extra Codex sign-in (None until it has used Codex once).
pub fn read_account(cfg: &Config, name: &str, running: bool) -> Option<Codex> {
    read_home(&account_home(cfg, name), || running)
}

fn read_home(home: &Path, is_running: impl FnOnce() -> bool) -> Option<Codex> {
    let logs = newest_logs(home);
    let (_, v) = logs.iter().find_map(|(t, p)| last_rate_limits(p).map(|v| (*t, v)))?;
    let window = |k: &str| -> Option<Window> {
        let w = v.get(k)?;
        Some(Window {
            minutes: w.get("window_minutes")?.as_u64()?,
            pct: w.get("used_percent")?.as_f64()?.round().clamp(0.0, 100.0) as u8,
            resets_at: w.get("resets_at")?.as_u64()?,
        })
    };
    let mut windows: Vec<Window> = ["primary", "secondary"].iter().filter_map(|k| window(k)).collect();
    // A window whose reset time has passed has started over.
    let now = time::now();
    for w in &mut windows {
        if w.resets_at <= now {
            w.pct = 0;
            // Jump straight to the next reset (no loop: a bad interval can't hang the UI).
            let period = w.minutes.saturating_mul(60);
            if (1..=366 * 86_400).contains(&period) {
                w.resets_at += ((now - w.resets_at) / period + 1) * period;
            }
        }
    }
    windows.sort_by_key(|w| w.minutes);
    let credits = v.get("credits").and_then(|c| {
        let has = c.get("has_credits")?.as_bool()?;
        let unlimited = c.get("unlimited").and_then(|u| u.as_bool()).unwrap_or(false);
        if !has || unlimited {
            return None;
        }
        c.get("balance")?.as_str()?.parse::<f64>().ok()
    });
    let plan_of = |v: &serde_json::Value| v.get("plan_type").and_then(|p| p.as_str()).map(str::to_string);
    // Some records leave the plan out (null): take it from a recent one that has it.
    let plan = plan_of(&v)
        .or_else(|| logs.iter().skip(1).take(8).find_map(|(_, p)| last_rate_limits(p).and_then(|r| plan_of(&r))))
        .unwrap_or_default();
    let mut plan_title = plan.clone();
    if let Some(first) = plan_title.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Some(Codex {
        plan: plan_title,
        credits,
        windows,
        running: is_running(),
    })
}

pub fn open(cfg: &Config) {
    let target = format!("shell:AppsFolder\\{}", cfg.codex.app_id);
    crate::win::shell_open("explorer.exe", Some(&target));
}

// ---- extra Codex sign-ins ----------------------------------------------------------
//
// The Codex app (ChatGPT.exe) keeps everything in one profile folder plus the Codex
// home. Given its own of both (--user-data-dir and CODEX_HOME) it runs as a second,
// separate instance with its own sign-in, next to the main one.

/// An extra Codex sign-in's folder: `app` holds the window's profile, `codex` its Codex home.
pub fn account_dir(cfg: &Config, name: &str) -> PathBuf {
    crate::claude::profiles_root(cfg).join(name)
}

pub fn account_home(cfg: &Config, name: &str) -> PathBuf {
    account_dir(cfg, name).join("codex")
}

pub fn account_app_dir(cfg: &Config, name: &str) -> PathBuf {
    account_dir(cfg, name).join("app")
}

/// ChatGPT.exe inside the installed Codex app.
pub fn app_exe(cfg: &Config) -> Option<PathBuf> {
    let family = cfg.codex.app_id.split('!').next().unwrap_or_default();
    crate::claude::package_exe_of(family, "ChatGPT.exe").filter(|p| p.exists())
}

/// Open (or bring forward) an extra Codex sign-in's window.
pub fn open_account(cfg: &Config, name: &str) -> Result<(), String> {
    open_account_with(cfg, name, None)
}

/// A Codex conversation (thread) id from Codex's notify signal: letters, digits and dashes.
pub fn is_thread_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Why a finished Codex turn is not a chat you can open, from the first line of its
/// session log: a subagent another Codex chat started, or a `codex exec` run (a script
/// or a Claude chat driving Codex). With no log (an `--ephemeral` exec run keeps none),
/// a client other than the Codex app counts as exec. None for your own chat.
pub fn side_thread(home: &Path, thread: &str, client: &str) -> Option<&'static str> {
    match thread_meta(home, thread) {
        Some(meta) => side_kind(&meta),
        None => (!client.is_empty() && !client.to_ascii_lowercase().contains("desktop")).then_some("exec"),
    }
}

fn side_kind(meta: &serde_json::Value) -> Option<&'static str> {
    let p = &meta["payload"];
    if p["thread_source"] == "subagent" || p["source"].get("subagent").is_some() {
        Some("subagent")
    } else if p["source"] == "exec" || p["originator"] == "codex_exec" {
        Some("exec")
    } else {
        None
    }
}

/// A thread's session_meta line (sessions/YYYY/MM/DD/rollout-...-<id>.jsonl), newest days first.
fn thread_meta(home: &Path, thread: &str) -> Option<serde_json::Value> {
    if !is_thread_id(thread) {
        return None;
    }
    let tail = format!("-{thread}.jsonl");
    let mut days = 0;
    for y in sorted_dirs(&home.join("sessions")) {
        for m in sorted_dirs(&y) {
            for d in sorted_dirs(&m) {
                days += 1;
                if days > 60 {
                    return None;
                }
                let found = std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&tail)));
                if let Some(p) = found {
                    use std::io::BufRead;
                    let mut line = String::new();
                    std::io::BufReader::new(std::fs::File::open(p).ok()?).read_line(&mut line).ok()?;
                    return serde_json::from_str(&line).ok();
                }
            }
        }
    }
    None
}

/// Show one conversation in a Codex window (codex://threads/<id>, the Codex app's own
/// link), even when that window is on another chat. The main Codex gets the link
/// through Windows; an extra sign-in's instance gets it as an argument.
pub fn open_thread(cfg: &Config, account: &str, thread: &str) -> Result<(), String> {
    if !is_thread_id(thread) {
        return Err("not a Codex thread id".into());
    }
    let link = format!("codex://threads/{thread}");
    if account == "Codex" {
        crate::win::shell_open(&link, None);
        Ok(())
    } else {
        open_account_with(cfg, account, Some(&link))
    }
}

fn open_account_with(cfg: &Config, name: &str, link: Option<&str>) -> Result<(), String> {
    let exe = app_exe(cfg).ok_or("Couldn't find the Codex app. Install it from the Microsoft Store.")?;
    let (app, home) = (account_app_dir(cfg, name), account_home(cfg, name));
    for d in [&app, &home] {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let mut cmd = std::process::Command::new(exe);
    // A value from the main Codex (or a Codex-run hook) must never leak in.
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().to_ascii_uppercase().starts_with("CODEX_") {
            cmd.env_remove(&k);
        }
    }
    cmd.arg(format!("--user-data-dir={}", app.display()));
    if let Some(l) = link {
        cmd.arg(l);
    }
    cmd.env("CODEX_HOME", &home)
        .current_dir(std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

// ---- Codex alerts: sharing Codex's notify setting ----------------------------------
//
// Codex runs one program when a turn ends (`notify = [...]` in ~/.codex/config.toml),
// and its own computer-use helper may already be that program. Turning Codex alerts
// on puts this app first and passes every signal straight on to whatever was there:
//   notify = ["...\ClaudeAccounts.exe", "--event", "codex", "--then", <the old notify...>]
// Turning it off puts the old value back.

/// Marks where the program that had Codex's notify setting before begins.
pub const THEN: &str = "--then";

pub fn config_toml(cfg: &Config) -> PathBuf {
    home(cfg).join("config.toml")
}

enum Notify {
    Absent,
    /// Line number and the program plus arguments.
    Line(usize, Vec<String>),
    /// Present but in a form this app won't edit (multi-line, literal strings, ...).
    Unreadable,
}

/// Where a trailing `# comment` starts in a value (outside any string), if there is one.
fn comment_start(value: &str) -> Option<usize> {
    let (mut basic, mut literal, mut esc) = (false, false, false);
    for (i, c) in value.char_indices() {
        match c {
            _ if esc => esc = false,
            '\\' if basic => esc = true,
            '"' if !literal => basic = !basic,
            '\'' if !basic => literal = !literal,
            '#' if !basic && !literal => return Some(i),
            _ => {}
        }
    }
    None
}

/// The trailing comment of a `notify = [...]` line, with a space before it ("" if none).
fn trailing_comment(line: &str) -> String {
    let Some((_, value)) = line.split_once('=') else { return String::new() };
    comment_start(value).map_or(String::new(), |i| format!(" {}", value[i..].trim_end()))
}

fn notify_line(text: &str) -> Notify {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with('[') {
            break; // a [table] starts: top-level keys all come before it
        }
        if t.starts_with("\"notify\"") || t.starts_with("'notify'") {
            return Notify::Unreadable; // a quoted key: same setting, but not a form this app edits
        }
        let Some(rest) = t.strip_prefix("notify") else { continue };
        let Some(value) = rest.trim_start().strip_prefix('=') else { continue };
        let value = value.trim();
        let value = comment_start(value).map_or(value, |c| value[..c].trim_end());
        return match toml_strings(value) {
            Some(v) => Notify::Line(i, v),
            None => Notify::Unreadable,
        };
    }
    Notify::Absent
}

/// A one-line TOML array of strings: "basic" (with escapes) or 'literal'. None for
/// anything else (other value types, a missing bracket, text after it).
fn toml_strings(s: &str) -> Option<Vec<String>> {
    let mut c = s.strip_prefix('[')?.chars().peekable();
    let mut out = Vec::new();
    loop {
        while c.next_if(|x| x.is_whitespace()).is_some() {}
        match c.next()? {
            ']' => break,
            '\'' => {
                let mut v = String::new();
                loop {
                    match c.next()? {
                        '\'' => break,
                        x => v.push(x),
                    }
                }
                out.push(v);
            }
            '"' => {
                let mut v = String::new();
                loop {
                    match c.next()? {
                        '"' => break,
                        '\\' => match c.next()? {
                            'n' => v.push('\n'),
                            't' => v.push('\t'),
                            'r' => v.push('\r'),
                            'b' => v.push('\u{8}'),
                            'f' => v.push('\u{c}'),
                            'u' => v.push(char::from_u32(u32::from_str_radix(&(0..4).map(|_| c.next()).collect::<Option<String>>()?, 16).ok()?)?),
                            x @ ('"' | '\\') => v.push(x),
                            _ => return None,
                        },
                        x => v.push(x),
                    }
                }
                out.push(v);
            }
            _ => return None,
        }
        while c.next_if(|x| x.is_whitespace()).is_some() {}
        match c.next()? {
            ',' => continue,
            ']' => break,
            _ => return None,
        }
    }
    let rest: String = c.collect();
    let rest = rest.trim();
    (rest.is_empty() || rest.starts_with('#')).then_some(out)
}

/// Codex's computer-use helper puts itself first and keeps the setting it found as
/// `--previous-notify '<JSON list>'`, which it runs on every signal. When that list is
/// this app's command, this app is still in the chain: (index of the value, the list).
fn nested_ours(v: &[String], me: &str) -> Option<(usize, Vec<String>)> {
    let i = v.iter().position(|a| a == PREVIOUS)? + 1;
    let inner: Vec<String> = serde_json::from_str(v.get(i)?).ok()?;
    is_ours(&inner, me).then_some((i, inner))
}

const PREVIOUS: &str = "--previous-notify";

/// This app's command, directly or kept by the helper.
fn hooked(v: &[String], me: &str) -> bool {
    is_ours(v, me) || nested_ours(v, me).is_some()
}

fn is_ours(v: &[String], me: &str) -> bool {
    if v.len() < 3 {
        return false; // `notify = []` and the like
    }
    let name = |p: &str| std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().to_ascii_lowercase());
    let named = name(&v[0]) == name(me) || crate::OLD_EXES.iter().any(|o| name(&v[0]).as_deref() == Some(&o.to_ascii_lowercase()));
    named && v[1] == "--event" && v[2] == "codex"
}

/// Equivalent paths: slashes and case don't matter on Windows.
fn same_path(a: &str, b: &str) -> bool {
    let n = |p: &str| p.replace('\\', "/").to_ascii_lowercase();
    n(a) == n(b)
}

/// Our part of the setting with this exe's path in it, when it names another (the old
/// "Switchboard.exe" or "ClaudeAccounts.exe", or the app moved). None when nothing needs to change.
fn retarget(v: &[String], me: &str) -> Option<Vec<String>> {
    if is_ours(v, me) {
        return (!same_path(&v[0], me)).then(|| std::iter::once(me.to_string()).chain(v[1..].iter().cloned()).collect());
    }
    let (at, mut inner) = nested_ours(v, me)?;
    if same_path(&inner[0], me) {
        return None;
    }
    inner[0] = me.to_string();
    let mut out = v.to_vec();
    out[at] = serde_json::to_string(&inner).ok()?;
    Some(out)
}

/// The program that had the notify setting before us (passed every signal), if any.
pub fn forwarded(v: &[String]) -> &[String] {
    v.iter().position(|a| a == THEN).map_or(&[], |i| &v[i + 1..])
}

/// The new file text, or None when nothing needs to change.
#[cfg(test)]
fn rewrite(text: &str, me: &str, on: bool) -> Result<Option<String>, String> {
    rewrite_for(text, me, None, on)
}

/// This app's part of the notify setting; an extra sign-in also passes its name.
fn ours(me: &str, account: Option<&str>) -> Vec<String> {
    let mut v = vec![me.to_string(), "--event".into(), "codex".into()];
    if let Some(a) = account {
        v.extend(["--account".to_string(), a.to_string()]);
    }
    v
}

fn rewrite_for(text: &str, me: &str, account: Option<&str>, on: bool) -> Result<Option<String>, String> {
    // A byte-order mark stays where it is; the lines are worked on without it.
    if let Some(rest) = text.strip_prefix('\u{feff}') {
        return Ok(rewrite_for(rest, me, account, on)?.map(|t| format!("\u{feff}{t}")));
    }
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let toml = |v: &[String]| format!("notify = {}", serde_json::to_string(v).unwrap_or_default());
    // The same, for the line at `i`: its trailing comment stays.
    let toml_at = |i: usize, v: &[String]| format!("{}{}", toml(v), trailing_comment(text.lines().nth(i).unwrap_or_default()));
    match (notify_line(text), on) {
        (Notify::Unreadable, _) => {
            return Err("Codex's notify setting is written in a form this app won't change safely. Edit ~/.codex/config.toml by hand.".into())
        }
        (Notify::Line(i, v), true) if hooked(&v, me) => match retarget(&v, me) {
            Some(new) => lines[i] = toml_at(i, &new),
            None => return Ok(None),
        },
        (Notify::Line(i, v), true) => {
            let mut new = ours(me, account);
            new.push(THEN.into());
            new.extend(v);
            lines[i] = toml_at(i, &new);
        }
        (Notify::Absent, true) => {
            // After any comments at the top, before the first [table].
            let at = lines.iter().position(|l| !l.trim_start().starts_with('#')).unwrap_or(lines.len());
            lines.insert(at, toml(&ours(me, account)));
        }
        (Notify::Line(i, mut v), false) if nested_ours(&v, me).is_some() => {
            // Take this app out of the helper's chain. What this app passed signals on to
            // goes back in its place, unless that is the helper itself (it would call itself).
            let (at, inner) = nested_ours(&v, me).unwrap_or_default();
            let old = forwarded(&inner).to_vec();
            let same = |a: &str, b: &str| std::path::Path::new(a).file_name() == std::path::Path::new(b).file_name();
            if old.is_empty() || old.first().zip(v.first()).is_some_and(|(a, b)| same(a, b)) {
                v.drain(at - 1..=at);
            } else {
                v[at] = serde_json::to_string(&old).unwrap_or_default();
            }
            lines[i] = toml_at(i, &v);
        }
        (Notify::Line(i, v), false) if is_ours(&v, me) => {
            let old = forwarded(&v).to_vec();
            if old.is_empty() {
                lines.remove(i);
            } else {
                lines[i] = toml_at(i, &old);
            }
        }
        (_, false) => return Ok(None),
    }
    let mut out = lines.join(nl);
    if text.ends_with('\n') {
        out.push_str(nl);
    }
    Ok(Some(out))
}

fn me() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

/// Is this app in Codex's notify setting?
pub fn alerts_on(cfg: &Config) -> bool {
    std::fs::read_to_string(config_toml(cfg)).is_ok_and(|t| matches!(notify_line(&t), Notify::Line(_, v) if hooked(&v, &me())))
}

/// How Codex's notify setting stands for this app (read only; see `--doctor`).
#[derive(Debug, PartialEq)]
pub enum NotifyStatus {
    /// This exe is in the setting, directly or kept by Codex's helper.
    Ok,
    /// This app's command is there but names another exe (the program it names).
    Old(String),
    /// No notify setting, or one without this app.
    Missing,
    /// A form this app won't edit.
    Unreadable,
}

fn notify_status(text: &str, me: &str) -> NotifyStatus {
    match notify_line(text) {
        Notify::Unreadable => NotifyStatus::Unreadable,
        Notify::Absent => NotifyStatus::Missing,
        Notify::Line(_, v) if !hooked(&v, me) => NotifyStatus::Missing,
        Notify::Line(_, v) => match retarget(&v, me) {
            None => NotifyStatus::Ok,
            Some(_) => {
                let named = if is_ours(&v, me) { v[0].clone() } else { nested_ours(&v, me).map(|(_, i)| i[0].clone()).unwrap_or_default() };
                NotifyStatus::Old(named)
            }
        },
    }
}

/// The status of the main Codex settings file; None when the file can't be read.
pub fn notify_check(cfg: &Config) -> Option<NotifyStatus> {
    let text = std::fs::read_to_string(config_toml(cfg)).ok()?;
    Some(notify_status(&text, &me()))
}

/// The exact line that puts this app in Codex's notify setting (for `--doctor` to print).
pub fn notify_line_to_add(cfg: &Config) -> String {
    let text = std::fs::read_to_string(config_toml(cfg)).unwrap_or_default();
    match rewrite_for(&text, &me(), None, true) {
        Ok(Some(new)) => new.lines().find(|l| l.trim_start().starts_with("notify")).unwrap_or_default().to_string(),
        _ => format!("notify = {}", serde_json::to_string(&ours(&me(), None)).unwrap_or_default()),
    }
}

/// `--doctor --fix-codex`: put this app in the notify setting of the main Codex file, and
/// nothing else (no [desktop] changes, no extra sign-ins). Backs the file up first.
/// Returns the backup's path, or None when nothing needed to change.
pub fn fix_notify(cfg: &Config) -> Result<Option<PathBuf>, String> {
    let path = config_toml(cfg);
    let text = std::fs::read_to_string(&path).map_err(|_| format!("Codex's settings file wasn't found ({}). Open Codex once first.", path.display()))?;
    installed_exe(&me())?;
    fix_notify_in(&path, &text, &me())
}

/// Only the installed exe (`...\.claude\tools\Hive.exe`, or its Switchboard.exe / ClaudeAccounts.exe copies)
/// may be written into Codex's settings, never a build in `target\` or a worktree.
/// `HIVE_ALLOW_ANY_EXE=1` skips the check (for tests).
fn installed_exe(me: &str) -> Result<(), String> {
    if std::env::var_os("HIVE_ALLOW_ANY_EXE").is_some_and(|v| v == "1") {
        return Ok(());
    }
    let n = me.replace('\\', "/").to_ascii_lowercase();
    let (dir, file) = n.rsplit_once('/').unwrap_or(("", &n));
    let known = file == "hive.exe" || crate::OLD_EXES.iter().any(|o| o.to_ascii_lowercase() == file);
    if dir.ends_with("/.claude/tools") && known {
        Ok(())
    } else {
        Err(format!("this is {me}, not the installed Hive (...\\.claude\\tools\\Hive.exe). Run --doctor --fix-codex from the installed one."))
    }
}

fn fix_notify_in(path: &Path, text: &str, me: &str) -> Result<Option<PathBuf>, String> {
    let Some(new) = rewrite_for(text, me, None, true)? else { return Ok(None) };
    let backup = path.with_extension(format!("toml.before-notify-fix-{}", crate::time::now()));
    std::fs::copy(path, &backup).map_err(|e| format!("Couldn't back up {}: {e}", path.display()))?;
    // Codex may have written the file since it was read: write only from what is there now.
    if std::fs::read_to_string(path).ok().as_deref() != Some(text) {
        let _ = std::fs::remove_file(&backup);
        return Err("Codex's settings file changed while it was being fixed. Nothing was written; run it again.".into());
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &new).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    crate::log(&format!("codex notify fixed: {} (backup {})", path.display(), backup.display()));
    Ok(Some(backup))
}

/// The helper this app passes Codex's signals on to, when it no longer exists (Codex moved it).
pub fn missing_helper(cfg: &Config) -> Option<String> {
    let text = std::fs::read_to_string(config_toml(cfg)).ok()?;
    let Notify::Line(_, v) = notify_line(&text) else { return None };
    let old = forwarded(&v);
    let prog = old.first()?;
    (is_ours(&v, &me()) && !std::path::Path::new(prog).exists()).then(|| prog.clone())
}

/// Turn Codex alerts on or off by editing Codex's settings file (a backup is kept),
/// then the same for each extra sign-in that has been opened once.
pub fn set_alerts(cfg: &Config, on: bool) -> Result<(), String> {
    let path = config_toml(cfg);
    let text = std::fs::read_to_string(&path).map_err(|_| format!("Codex's settings file wasn't found ({}). Open Codex once first.", path.display()))?;
    set_alerts_in(&path, &text, None, on)?;
    set_account_alerts(cfg, on);
    Ok(())
}

/// Each extra sign-in's own Codex settings (skipped until it has been opened once).
pub fn set_account_alerts(cfg: &Config, on: bool) {
    for a in &cfg.codex.accounts {
        let path = account_home(cfg, &a.name).join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if let Err(e) = set_alerts_in(&path, &text, Some(&a.name), on) {
            crate::log(&format!("codex alerts for {}: {e}", a.name));
        }
    }
}

/// A sign-in leaves the list: put its Codex notify setting back as it was.
pub fn unhook_account(cfg: &Config, name: &str) {
    let path = account_home(cfg, name).join("config.toml");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Err(e) = set_alerts_in(&path, &text, Some(name), false) {
            crate::log(&format!("codex alerts for {name}: {e}"));
        }
    }
}

fn set_alerts_in(path: &Path, text: &str, account: Option<&str>, on: bool) -> Result<(), String> {
    let hooked = rewrite_for(text, &me(), account, on)?.unwrap_or_else(|| text.to_string());
    let saved = saved_path(path);
    let (new, changed, snapshot) = take_over(&hooked, on, &saved)?;
    if new == text {
        if !on {
            let _ = std::fs::remove_file(&saved); // Codex's own values are already there
        }
        return Ok(());
    }
    let write = || -> Result<(), String> {
        let backup = path.with_extension(format!("toml.before-{}-{}", if on { "alerts-on" } else { "alerts-off" }, crate::time::now()));
        std::fs::copy(path, &backup).map_err(|e| format!("Couldn't back up {}: {e}", path.display()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &new).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    };
    if let Err(e) = write() {
        if snapshot {
            let _ = std::fs::remove_file(&saved); // config.toml is unchanged: nothing to put back
        }
        return Err(e);
    }
    if !on {
        let _ = std::fs::remove_file(&saved); // Codex's own values are back
    }
    let what = if hooked != text { "notify" } else { "" };
    let sep = if what.is_empty() || changed.is_empty() { "" } else { ", " };
    crate::log(&format!("codex alerts {}: {} ({what}{sep}{})", if on { "on" } else { "off" }, path.display(), changed.join(", ")));
    Ok(())
}

// ---- this app's alerts replace Codex's own ------------------------------------------
//
// The Codex app keeps its settings in config.toml's [desktop] table. Its own
// turn-finished and question notifications (`notifications-turn-mode`, default
// "unfocused"; `notifications-questions-enabled`, default true) would pop up next
// to this app's card, so with Codex alerts on they are turned off, and the values
// that were there are kept in a small file beside config.toml and put back when
// Codex alerts go off. Permission prompts stay Codex's own: its notify signal
// doesn't report them.

/// Codex's own notification settings this app takes over, and the value that turns each off.
const TAKEOVER: [(&str, &str); 2] = [("notifications-turn-mode", "\"off\""), ("notifications-questions-enabled", "false")];

/// Where the replaced values wait to be put back.
fn saved_path(config: &Path) -> PathBuf {
    let p = config.with_file_name("config.toml.hive-notifications.json");
    for prefix in ["switchboard", "claude-accounts"] {
        let old = config.with_file_name(format!("config.toml.{prefix}-notifications.json"));
        if !p.exists() && old.exists() {
            let _ = std::fs::rename(&old, &p);
        }
    }
    p
}

/// Set (or put back) Codex's own notification settings. Returns the new text, what
/// changed, and whether this call saved the originals (the caller removes them again
/// if config.toml can't be written, so a failed try never leaves stale originals).
fn take_over(text: &str, on: bool, saved: &Path) -> Result<(String, Vec<String>, bool), String> {
    let mut out = text.to_string();
    let mut changed = Vec::new();
    if on {
        for (k, v) in TAKEOVER {
            if desktop_value(&out, k).as_deref() != Some(v) {
                out = desktop_put(&out, k, Some(v))?;
                changed.push(format!("{k} = {v}"));
            }
        }
        // Saved only once every change is known to be safe, and never over an earlier save
        // (that one holds the values from before this app first changed anything).
        if saved.exists() {
            return Ok((out, changed, false));
        }
        let before: serde_json::Map<String, serde_json::Value> = TAKEOVER
            .iter()
            .map(|(k, _)| (k.to_string(), desktop_value(text, k).map_or(serde_json::Value::Null, serde_json::Value::String)))
            .collect();
        // Whole or not at all: a half-written file would lose Codex's own values.
        let tmp = saved.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&before).unwrap_or_default())
            .and_then(|_| std::fs::rename(&tmp, saved))
            .map_err(|e| format!("Couldn't save Codex's notification settings: {e}"))?;
        return Ok((out, changed, true));
    }
    let t = match std::fs::read_to_string(saved) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((out, changed, false)), // nothing was taken over
        Err(e) => return Err(format!("Couldn't read the saved Codex notification settings ({}): {e}", saved.display())),
    };
    {
        let before: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&t).map_err(|_| "the saved Codex notification settings don't parse".to_string())?;
        for (k, _) in TAKEOVER {
            let old = before.get(k).and_then(|v| v.as_str());
            if desktop_value(&out, k).as_deref() != old {
                out = desktop_put(&out, k, old)?;
                changed.push(format!("{k} back to {}", old.unwrap_or("Codex's default")));
            }
        }
    }
    Ok((out, changed, false))
}

/// `[desktop]`, alone or with a comment after it.
fn is_desktop_header(line: &str) -> bool {
    line.trim().strip_prefix("[desktop]").is_some_and(|r| r.trim().is_empty() || r.trim_start().starts_with('#'))
}

/// Valid TOML that sets `key` in a way the simple line edit below can't follow: another
/// spelling of the table header, a quoted or dotted key, or a multi-line string.
/// Changing such a file could leave it broken, so it is left alone.
fn odd_form(lines: &[&str], key: &str) -> bool {
    lines.iter().any(|l| {
        let code = l.split('#').next().unwrap_or("");
        let squeezed: String = code.chars().filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'').collect();
        (squeezed == "[desktop]" && !is_desktop_header(l))
            || code.contains(&format!("\"{key}\""))
            || code.contains(&format!("'{key}'"))
            || code.trim_start().starts_with(&format!("desktop.{key}"))
            || (code.contains(key) && (l.contains("\"\"\"") || l.contains("\'\'\'")))
    })
}

/// Lines of the [desktop] table: (index of its header, index after its last line).
fn desktop_table(lines: &[&str]) -> Option<(usize, usize)> {
    let head = lines.iter().position(|l| is_desktop_header(l))?;
    let end = lines[head + 1..].iter().position(|l| l.trim_start().starts_with('[')).map_or(lines.len(), |i| head + 1 + i);
    Some((head, end))
}

/// The line in [desktop] that sets `key` (bare key only).
fn desktop_line(lines: &[&str], key: &str) -> Option<usize> {
    let (head, end) = desktop_table(lines)?;
    (head + 1..end).find(|&i| lines[i].trim_start().strip_prefix(key).is_some_and(|r| r.trim_start().starts_with('=')))
}

/// The raw TOML value of a [desktop] key ("\"off\"", "false").
fn desktop_value(text: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let i = desktop_line(&lines, key)?;
    let v = lines[i].split_once('=')?.1.trim();
    Some(v.split(" #").next().unwrap_or(v).trim().to_string())
}

/// Set a [desktop] key to a raw TOML value, or remove it (None). Nothing else changes.
fn desktop_put(text: &str, key: &str, value: Option<&str>) -> Result<String, String> {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let view: Vec<&str> = lines.iter().map(String::as_str).collect();
    if odd_form(&view, key) {
        return Err(format!("{key} is written in a form this app won't change safely"));
    }
    match (desktop_line(&view, key), value) {
        (Some(i), Some(v)) => lines[i] = format!("{key} = {v}"),
        (Some(i), None) => {
            lines.remove(i);
        }
        (None, Some(v)) => match desktop_table(&view) {
            Some((head, _)) => lines.insert(head + 1, format!("{key} = {v}")),
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push("[desktop]".into());
                lines.push(format!("{key} = {v}"));
            }
        },
        (None, None) => return Ok(text.to_string()),
    }
    let mut out = lines.join(nl);
    if text.ends_with('\n') || text.is_empty() {
        out.push_str(nl);
    }
    Ok(out)
}

#[cfg(test)]
mod hook_tests {
    use super::*;

    const ME: &str = "C:\\Tools\\ClaudeAccounts.exe";

    /// The form Codex's computer-use helper wrote on 2026-10-15: itself first, this app kept as its previous setting.
    const NESTED: &str = r#"model = "m"
notify = ['C:\Codex\codex-computer-use.exe', "turn-ended", "--previous-notify", '["C:\\Tools\\ClaudeAccounts.exe","--event","codex","--then","C:\\Codex\\codex-computer-use.exe","turn-ended"]']
[agents]
"#;

    #[test]
    fn empty_or_short_notify_lists_are_not_ours_and_do_not_panic() {
        assert!(!is_ours(&[], ME));
        assert!(!is_ours(&[ME.to_string()], ME));
        assert!(!hooked(&[], ME));
        assert!(retarget(&[], ME).is_none());
    }

    #[test]
    fn subagents_and_exec_runs_are_side_threads() {
        let meta = |p: serde_json::Value| serde_json::json!({ "type": "session_meta", "payload": p });
        let sub = meta(serde_json::json!({ "source": { "subagent": { "thread_spawn": { "depth": 1 } } }, "originator": "Codex Desktop", "thread_source": "subagent" }));
        let exec = meta(serde_json::json!({ "source": "exec", "originator": "codex_exec", "thread_source": "user" }));
        let mine = meta(serde_json::json!({ "source": "vscode", "originator": "Codex Desktop", "thread_source": "user" }));
        assert_eq!(side_kind(&sub), Some("subagent"));
        assert_eq!(side_kind(&exec), Some("exec"));
        assert_eq!(side_kind(&mine), None);
        // No session log: only the Codex app's own client still gets a card.
        let none = Path::new("Z:\\no-codex-home");
        let id = "01a1417f-ad45-7ea2-9137-0f7d4ee6c6d7";
        assert_eq!(side_thread(none, id, "codex_exec"), Some("exec"));
        assert_eq!(side_thread(none, id, "Codex Desktop"), None);
        assert_eq!(side_thread(none, id, ""), None);
    }

    #[test]
    fn toml_string_lists_of_both_kinds_are_read() {
        assert_eq!(toml_strings(r#"['C:\a b', "x\\y\"z", 'q' ] # note"#), Some(vec![r"C:\a b".to_string(), "x\\y\"z".into(), "q".into()]));
        assert_eq!(toml_strings("[]"), Some(vec![]));
        assert_eq!(toml_strings("['a', 1]"), None);
        assert_eq!(toml_strings("['a'"), None);
        assert_eq!(toml_strings("['a'] extra"), None);
    }

    #[test]
    fn the_old_exe_name_is_moved_to_the_new_one() {
        let me = "C:\\Tools\\Hive.exe";
        // Nested in the helper's chain: only the inner path changes.
        let on = rewrite(NESTED, me, true).unwrap().unwrap();
        let Notify::Line(_, v) = notify_line(&on) else { panic!("unreadable") };
        assert_eq!(v[0], "C:\\Codex\\codex-computer-use.exe");
        let inner: Vec<String> = serde_json::from_str(&v[3]).unwrap();
        assert_eq!(inner[0], me);
        assert_eq!(&inner[1..], &["--event", "codex", "--then", "C:\\Codex\\codex-computer-use.exe", "turn-ended"]);
        assert_eq!(rewrite(&on, me, true).unwrap(), None, "already pointing here: no edit");
        // Direct form.
        let direct = "notify = [\"C:\\\\Tools\\\\ClaudeAccounts.exe\", \"--event\", \"codex\"]\n";
        let on = rewrite(direct, me, true).unwrap().unwrap();
        assert_eq!(on, "notify = [\"C:\\\\Tools\\\\Hive.exe\",\"--event\",\"codex\"]\n");
        // The name before Hive is ours too.
        let direct = "notify = [\"C:\\\\Tools\\\\Switchboard.exe\", \"--event\", \"codex\"]\n";
        assert_eq!(rewrite(direct, me, true).unwrap().unwrap(), on);
    }

    #[test]
    fn kept_by_the_helper_counts_as_on() {
        assert!(matches!(notify_line(NESTED), Notify::Line(_, ref v) if hooked(v, ME)));
        assert_eq!(rewrite(NESTED, ME, true).unwrap(), None, "already on: no edit, no fight with the helper");
    }

    #[test]
    fn off_takes_only_this_app_out_of_the_helpers_chain() {
        let off = rewrite(NESTED, ME, false).unwrap().unwrap();
        assert_eq!(off, "model = \"m\"\nnotify = [\"C:\\\\Codex\\\\codex-computer-use.exe\",\"turn-ended\"]\n[agents]\n");
        assert!(matches!(notify_line(&off), Notify::Line(_, ref v) if !hooked(v, ME)));
    }
    const WITH_HELPER: &str = "# :schema x\r\nmodel = \"m\"\r\nnotify = [ \"C:\\\\Codex\\\\codex-computer-use.exe\", \"turn-ended\" ]\r\n[agents]\r\nenabled = true\r\n";

    #[test]
    fn on_then_off_keeps_the_helper() {
        let on = rewrite(WITH_HELPER, ME, true).unwrap().unwrap();
        let Notify::Line(_, v) = notify_line(&on) else { panic!("no notify line") };
        assert!(is_ours(&v, ME));
        assert_eq!(forwarded(&v), ["C:\\Codex\\codex-computer-use.exe", "turn-ended"]);
        assert!(on.contains("\r\n[agents]\r\n"), "rest of the file untouched");
        assert!(rewrite(&on, ME, true).unwrap().is_none(), "on twice changes nothing");
        let off = rewrite(&on, ME, false).unwrap().unwrap();
        let Notify::Line(_, back) = notify_line(&off) else { panic!() };
        assert_eq!(back, ["C:\\Codex\\codex-computer-use.exe", "turn-ended"]);
    }

    #[test]
    fn no_notify_line_is_added_then_removed() {
        let plain = "# top\nmodel = \"m\"\n[agents]\nx = 1\n";
        let on = rewrite(plain, ME, true).unwrap().unwrap();
        assert!(on.starts_with("# top\nnotify = "));
        let off = rewrite(&on, ME, false).unwrap().unwrap();
        assert_eq!(off, plain);
    }

    #[test]
    fn odd_forms_are_left_alone() {
        let multi = "notify = [\n  \"a\",\n]\n";
        assert!(rewrite(multi, ME, true).is_err());
        assert!(rewrite("\"notify\" = [\"helper.exe\"]\n", ME, true).is_err(), "a quoted key must not get a second notify");
        assert!(rewrite("[t]\nnotify = [\"x\"]\n", ME, false).unwrap().is_none(), "a notify inside a table is not Codex's");
    }

    #[test]
    fn an_extra_sign_in_passes_its_name() {
        let on = rewrite_for(WITH_HELPER, ME, Some("Codex Work"), true).unwrap().unwrap();
        let Notify::Line(_, v) = notify_line(&on) else { panic!() };
        assert!(is_ours(&v, ME));
        assert_eq!(&v[3..5], ["--account", "Codex Work"]);
        assert_eq!(forwarded(&v), ["C:\\Codex\\codex-computer-use.exe", "turn-ended"]);
        let off = rewrite_for(&on, ME, Some("Codex Work"), false).unwrap().unwrap();
        let Notify::Line(_, back) = notify_line(&off) else { panic!() };
        assert_eq!(back, ["C:\\Codex\\codex-computer-use.exe", "turn-ended"]);
    }
}

#[cfg(test)]
mod thread_id_tests {
    use super::is_thread_id;

    #[test]
    fn only_plain_ids_become_links() {
        assert!(is_thread_id("01a13db4-1a90-7cd0-b7f3-1515880bec80"));
        assert!(is_thread_id("01A13DB4"));
        for bad in ["", "1234567", "01a1;calc", "../01a13db4", "--user-data-dir", "01a13db4 x", "01a1/3db4-aa", &"a".repeat(65), "01a13db4
"] {
            assert!(!is_thread_id(bad), "{bad:?} should be refused");
        }
    }
}

#[cfg(test)]
mod takeover_tests {
    use super::*;

    const CFG: &str = "model = \"m\"\r\n[desktop]\r\nfollowUpQueueMode = \"queue\"\r\nnotifications-questions-enabled = true\r\n\r\n[desktop.open-in-target-preferences]\r\nglobal = \"fileManager\"\r\n";

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ca-takeover-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&p);
        p
    }

    #[test]
    fn odd_but_valid_forms_are_left_alone() {
        for odd in [
            "[desktop]\n'notifications-turn-mode' = \"unfocused\"\n",
            "[desktop]\n\"notifications-questions-enabled\" = true\n",
            "[ desktop ]\nnotifications-turn-mode = \"always\"\n",
            "desktop.notifications-turn-mode = \"always\"\n",
            "[desktop]\nnotifications-turn-mode = \"\"\"\nunfocused\"\"\"\n",
        ] {
            let saved = temp("odd");
            assert!(take_over(odd, true, &saved).is_err(), "{odd:?} should be refused");
            assert!(!saved.exists(), "a refused change must not save originals");
        }
    }

    #[test]
    fn a_header_with_a_comment_is_the_same_table() {
        let saved = temp("comment");
        let (on, _, made) = take_over("[desktop] # app settings\nnotifications-turn-mode = \"always\"\n", true, &saved).unwrap();
        assert!(made);
        assert_eq!(on, "[desktop] # app settings\nnotifications-questions-enabled = false\nnotifications-turn-mode = \"off\"\n");
        let _ = std::fs::remove_file(&saved);
    }

    #[test]
    fn unreadable_originals_are_an_error_not_a_silent_success() {
        let saved = temp("unreadable");
        std::fs::create_dir(&saved).unwrap(); // exists, but can't be read as a file
        assert!(take_over(CFG, false, &saved).is_err());
        let _ = std::fs::remove_dir(&saved);
        assert!(take_over(CFG, false, &saved).is_ok(), "no saved file means nothing to put back");
    }

    #[test]
    fn on_then_off_puts_codex_back_exactly() {
        let saved = std::env::temp_dir().join(format!("ca-takeover-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&saved);
        let (on, changed, _) = take_over(CFG, true, &saved).unwrap();
        assert_eq!(changed.len(), 2);
        assert_eq!(desktop_value(&on, "notifications-turn-mode").as_deref(), Some("\"off\""));
        assert_eq!(desktop_value(&on, "notifications-questions-enabled").as_deref(), Some("false"));
        assert!(on.contains("[desktop.open-in-target-preferences]\r\nglobal = \"fileManager\"\r\n"), "the rest stays");
        let (again, changed, _) = take_over(&on, true, &saved).unwrap();
        assert!(changed.is_empty() && again == on, "on twice changes nothing");
        let (off, _, _) = take_over(&on, false, &saved).unwrap();
        assert_eq!(off, CFG, "exactly what was there before");
        let _ = std::fs::remove_file(&saved);
    }

    #[test]
    fn a_file_without_a_desktop_table_gets_one() {
        let saved = std::env::temp_dir().join(format!("ca-takeover2-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&saved);
        let plain = "model = \"m\"\n";
        let (on, _, _) = take_over(plain, true, &saved).unwrap();
        assert_eq!(on, "model = \"m\"\n\n[desktop]\nnotifications-questions-enabled = false\nnotifications-turn-mode = \"off\"\n");
        let (off, _, _) = take_over(&on, false, &saved).unwrap();
        assert_eq!(off, "model = \"m\"\n\n[desktop]\n");
        let _ = std::fs::remove_file(&saved);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hive-fixnotify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const ME: &str = "C:/h/Hive.exe";

    #[test]
    fn notify_status_ok_missing_old_and_odd() {
        let ok = format!("model = \"m\"
notify = [\"{ME}\", \"--event\", \"codex\"]
[desktop]
x = 1
");
        assert_eq!(notify_status(&ok, ME), NotifyStatus::Ok);
        assert_eq!(notify_status("model = \"m\"
", ME), NotifyStatus::Missing);
        assert_eq!(notify_status("notify = [\"other.exe\", \"x\", \"y\"]
", ME), NotifyStatus::Missing);
        let old = "notify = [\"C:/old/Switchboard.exe\", \"--event\", \"codex\"]
";
        assert_eq!(notify_status(old, ME), NotifyStatus::Old("C:/old/Switchboard.exe".into()));
        let moved = "notify = [\"D:/elsewhere/Hive.exe\", \"--event\", \"codex\"]
";
        assert_eq!(notify_status(moved, ME), NotifyStatus::Old("D:/elsewhere/Hive.exe".into()));
        assert_eq!(notify_status("notify = [
  \"a\",
]
", ME), NotifyStatus::Unreadable);
    }

    #[test]
    fn fix_changes_only_the_notify_key_and_backs_up() {
        let d = temp_dir("fix");
        let p = d.join("config.toml");
        let before = "# top
model = \"m\"
notify = [\"C:/old/ClaudeAccounts.exe\", \"--event\", \"codex\"]
other = 1

[desktop]
notifications-questions-enabled = true
";
        std::fs::write(&p, before).unwrap();
        let backup = fix_notify_in(&p, before, ME).unwrap().expect("a change");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), before, "backup holds the original bytes");
        let after = std::fs::read_to_string(&p).unwrap();
        let (b, a): (Vec<&str>, Vec<&str>) = (before.split("
").collect(), after.split("
").collect());
        assert_eq!(b.len(), a.len());
        let diff: Vec<usize> = (0..b.len()).filter(|&i| b[i] != a[i]).collect();
        assert_eq!(diff, vec![2], "only the notify line differs");
        assert!(a[2].starts_with("notify = ") && a[2].contains(ME));
        assert_eq!(notify_status(&after, ME), NotifyStatus::Ok);
        // already right: no change, no new backup
        assert_eq!(fix_notify_in(&p, &after, ME).unwrap(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fix_adds_a_missing_line_and_leaves_desktop_keys_alone() {
        let d = temp_dir("add");
        let p = d.join("config.toml");
        let before = "model = \"m\"
[desktop]
notifications-turn-mode = \"unfocused\"
";
        std::fs::write(&p, before).unwrap();
        fix_notify_in(&p, before, ME).unwrap().expect("a change");
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(after.replace(&format!("notify = [\"{ME}\",\"--event\",\"codex\"]
"), ""), before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn equivalent_paths_are_not_old() {
        let t = "notify = [\"c:\\\\H\\\\HIVE.exe\", \"--event\", \"codex\"]\n";
        assert_eq!(notify_status(t, ME), NotifyStatus::Ok);
        assert_eq!(rewrite(t, ME, true).unwrap(), None, "no rewrite for the same path");
        let nested = "notify = [\"h.exe\", \"--previous-notify\", \"[\\\"C:\\\\\\\\H\\\\\\\\Hive.exe\\\", \\\"--event\\\", \\\"codex\\\"]\"]\n";
        assert_eq!(notify_status(nested, ME), NotifyStatus::Ok);
    }

    #[test]
    fn fix_refuses_a_build_exe() {
        assert!(installed_exe("C:/Users/x/.claude/tools/Hive.exe").is_ok());
        assert!(installed_exe("C:\\Users\\x\\.claude\\tools\\ClaudeAccounts.exe").is_ok());
        let e = installed_exe("C:/Users/x/.claude/tools/claude-accounts-wt/124/target/release/Hive.exe").unwrap_err();
        assert!(e.contains("not the installed Hive"), "{e}");
        assert!(installed_exe("C:/Users/x/.claude/tools/other.exe").is_err());
    }

    #[test]
    fn a_bom_file_gets_no_second_notify_key_and_keeps_its_bom() {
        let before = "\u{feff}notify = [\"C:/old/Switchboard.exe\", \"--event\", \"codex\"]\nmodel = \"m\"\n";
        assert_eq!(notify_status(before, ME), NotifyStatus::Old("C:/old/Switchboard.exe".into()));
        let after = rewrite(before, ME, true).unwrap().unwrap();
        assert!(after.starts_with("\u{feff}notify = ["), "{after:?}");
        assert_eq!(after.matches("notify").count(), 1);
        let none = rewrite("\u{feff}model = \"m\"\n", ME, true).unwrap().unwrap();
        assert!(none.starts_with("\u{feff}") && none.contains("notify = ["));
    }

    #[test]
    fn a_trailing_comment_on_the_notify_line_is_kept() {
        let before = "notify = [\"C:/old/ClaudeAccounts.exe\", \"--event\", \"codex\"] # alerts # keep\nmodel = \"m\"\n";
        assert_eq!(notify_status(before, ME), NotifyStatus::Old("C:/old/ClaudeAccounts.exe".into()));
        let after = rewrite(before, ME, true).unwrap().unwrap();
        assert!(after.lines().next().unwrap().ends_with("] # alerts # keep"), "{after}");
        let hash_in_string = "notify = [\"C:/a#b/x.exe\", \"y\", \"z\"]\n";
        assert_eq!(notify_status(hash_in_string, ME), NotifyStatus::Missing);
    }

    #[test]
    fn a_file_that_changed_since_it_was_read_is_not_overwritten() {
        let d = temp_dir("race");
        let p = d.join("config.toml");
        let read = "model = \"m\"\n";
        let newer = "model = \"m\"\nextra = 1\n";
        std::fs::write(&p, newer).unwrap();
        assert!(fix_notify_in(&p, read, ME).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), newer);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no backup or temp left");
        let _ = std::fs::remove_dir_all(&d);
    }
}
