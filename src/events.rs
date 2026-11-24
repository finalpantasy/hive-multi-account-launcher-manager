//! Chat alerts. Claude Code hooks (Stop: a turn finished, Notification: Claude
//! needs you) run `Hive.exe --event stop|notify`. That command works
//! out which account's window the chat belongs to and appends one line to
//! hive-events.jsonl; the tray watches the file (no polling) and
//! shows an alert that opens the right account and chat when clicked.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::claude;
use crate::config::Config;
use crate::procs::{self, Proc};

pub const FILE: &str = "hive-events.jsonl";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Event {
    /// "stop" (turn finished), "notify" (needs you) or "codex" (Codex turn finished).
    pub kind: String,
    /// Account name; "" if the chat's account couldn't be worked out.
    pub account: String,
    /// The chat's id in the Code tab sidebar (local_...), the id claude://code/continue takes.
    pub session: String,
    pub cwd: String,
    /// What Claude said it needs (Notification hooks only).
    pub message: String,
    /// Unix seconds.
    pub at: u64,
    /// Claude Code's own session id (the hook's session_id): tells two chats apart
    /// when there is no Code tab id (a chat run outside Claude Desktop).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub chat: String,
    /// The stop hook found the chat's last reply (to look for a question).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub read_reply: bool,
    /// The chat runs on a goal (`/goal`) that had not ended when this turn stopped: the goal
    /// check may start the next turn by itself, so the tray holds the event until it knows.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub goal: bool,
    /// The chat's transcript, kept only on goal events so the tray can read the goal's verdict.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub transcript: String,
}

pub fn path() -> PathBuf {
    crate::data_file(FILE)
}

/// Process ids from `pid` up through its parents (nearest first).
fn ancestors(procs: &[Proc], pid: u32) -> Vec<u32> {
    let mut out = vec![pid];
    while let Some(p) = procs.iter().find(|p| p.pid == *out.last().unwrap()) {
        if p.ppid == 0 || out.contains(&p.ppid) || out.len() > 40 {
            break;
        }
        out.push(p.ppid);
    }
    out
}

/// The account whose main Claude process is among `chain`.
fn account_in_chain(mains: &HashMap<String, u32>, chain: &[u32]) -> Option<String> {
    chain.iter().find_map(|pid| mains.iter().find(|(_, p)| *p == pid).map(|(n, _)| n.clone()))
}

/// Which account a chat belongs to: the Claude window above the hook process,
/// else the profile folder named in the environment.
fn account_of_caller(cfg: &Config) -> String {
    let procs = procs::list();
    let mains = procs::main_pids(cfg, &procs);
    if let Some(n) = account_in_chain(&mains, &ancestors(&procs, std::process::id())) {
        return n;
    }
    if let Some(dir) = std::env::var_os("CLAUDE_USER_DATA_DIR") {
        let dir = dir.to_string_lossy().to_ascii_lowercase();
        if let Some(a) = cfg.accounts.iter().find(|a| !a.default_profile && claude::data_dir(cfg, a).to_string_lossy().to_ascii_lowercase() == dir.trim_end_matches('\\')) {
            return a.name.clone();
        }
    }
    String::new()
}

/// The hook's JSON from stdin, if it arrives quickly (a hook must never hang).
pub fn hook_input() -> Option<serde_json::Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The first complete JSON value: no need to wait for the pipe to close.
        let v = serde_json::Deserializer::from_reader(std::io::stdin().lock()).into_iter::<serde_json::Value>().next();
        let _ = tx.send(v.and_then(|r| r.ok()));
    });
    rx.recv_timeout(std::time::Duration::from_millis(400)).ok().flatten()
}

/// The `--account` value among this app's own options; what follows --then belongs
/// to the forwarded program, even when it has an option of the same name.
fn account_option(args: &[String]) -> Option<String> {
    let own = &args[..args.iter().position(|a| a == crate::codex::THEN).unwrap_or(args.len())];
    own.iter().position(|a| a == "--account").and_then(|i| own.get(i + 1)).cloned()
}

/// `--event <kind> [--account <name>]`: record one event. Silent and quick; always exits 0.
pub fn cli(args: &[String]) {
    let kind = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if !matches!(kind.as_str(), "stop" | "notify" | "codex") {
        return;
    }
    let explicit = account_option(args);
    if kind == "codex" {
        // First pass the signal on to the program that had Codex's notify setting
        // before this app (Codex's computer-use helper), exactly as Codex would have.
        let fwd = crate::codex::forwarded(args);
        let file = |p: &str| std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().to_ascii_lowercase());
        let me = std::process::id();
        let procs = crate::procs::list();
        let parent = procs.iter().find(|p| p.pid == me).and_then(|p| procs.iter().find(|q| q.pid == p.ppid));
        let called_by_it = fwd.first().is_some_and(|prog| parent.is_some_and(|p| Some(p.name.to_ascii_lowercase()) == file(prog)));
        if let Some((prog, rest)) = fwd.split_first().filter(|_| !called_by_it) {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new(prog)
                .args(rest)
                .creation_flags(0x0800_0000) // no console window
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
    let Ok(cfg) = Config::load(&crate::config_path()) else { return };
    let mut ev = Event { kind: kind.clone(), at: crate::time::now(), ..Event::default() };
    if kind == "codex" {
        let mut client = String::new();
        // Codex passes its JSON as the last argument.
        if let Some(v) = args.last().and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok()) {
            client = v.get("client").and_then(|c| c.as_str()).unwrap_or_default().to_string();
            ev.cwd = v.get("cwd").and_then(|c| c.as_str()).unwrap_or_default().to_string();
            // Codex's conversation id: tells two chats in one folder apart.
            ev.session = v.get("thread-id").and_then(|c| c.as_str()).unwrap_or_default().to_string();
            let last = v.get("last-assistant-message").and_then(|c| c.as_str()).unwrap_or_default();
            if background_reply(last) {
                // Codex's own background turns (follow-up suggestions and the like) answer
                // with JSON, never prose: not a chat you ran, so no card.
                let client = v.get("client").and_then(|c| c.as_str()).unwrap_or("?");
                let asked: String = v.get("input-messages").and_then(|m| m.get(0)).and_then(|m| m.as_str()).unwrap_or("").chars().take(60).collect();
                crate::log(&format!("codex: no card for a background turn in {} (client {client}, asked {asked:?})", ev.cwd));
                return;
            }
            // Kept long: the card cleans it up and cuts it at a word.
            ev.message = last.chars().take(MESSAGE_KEEP).collect();
            if cfg.alert_questions && cfg.alerts_needs_me() {
                if let Some(q) = question_at_end(last) {
                    ev.kind = "ask".into();
                    ev.message = q.chars().take(MESSAGE_KEEP).collect();
                }
            }
        }
        // An extra Codex sign-in passes its name; the main one is "Codex". A name no
        // longer in the list is a removed sign-in: not the main Codex's news.
        ev.account = match explicit.as_deref() {
            None => "Codex".to_string(),
            Some(n) => match cfg.find_codex(n) {
                Some(a) => a.name.clone(),
                None => return,
            },
        };
        // Subagents and `codex exec` runs finish turns too, but they are not chats you
        // can open, so no card (2026-10-15).
        let home = if explicit.is_none() { crate::codex::home(&cfg) } else { crate::codex::account_home(&cfg, &ev.account) };
        if let Some(why) = crate::codex::side_thread(&home, &ev.session, &client) {
            note_no_card(&ev.cwd, &format!("{why}, client {client}"));
            return;
        }
    } else {
        let input = hook_input().unwrap_or_default();
        ev.cwd = input.get("cwd").and_then(|c| c.as_str()).unwrap_or_default().to_string();
        ev.message = input.get("message").and_then(|c| c.as_str()).unwrap_or_default().chars().take(200).collect();
        // A finished turn that ends by asking you something counts as "needs you".
        if kind == "stop" && cfg.alert_questions && cfg.alerts_needs_me() {
            if let Some(path) = input.get("transcript_path").and_then(|p| p.as_str()) {
                let path = std::path::Path::new(path);
                let mut found = reply_question(path);
                if found.is_none() {
                    // No reply on disk yet: it may still be landing. Wait once, briefly.
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    found = reply_question(path);
                }
                ev.read_reply = found.is_some();
                if let Some(Some(q)) = found {
                    ev.kind = "ask".into();
                    ev.message = q.chars().take(MESSAGE_KEEP).collect();
                }
            }
        }
        if kind == "stop" {
            if let Some(path) = input.get("transcript_path").and_then(|p| p.as_str()) {
                if goal_active(std::path::Path::new(path)) {
                    ev.goal = true;
                    ev.transcript = path.to_string();
                }
            }
        }
        ev.session = std::env::var("CLAUDE_CODE_HOST_SESSION_ID").unwrap_or_default();
        ev.chat = input.get("session_id").and_then(|c| c.as_str()).unwrap_or_default().to_string();
        ev.account = explicit.filter(|n| cfg.find(n).is_some()).or_else(|| Some(account_of_caller(&cfg)).filter(|n| !n.is_empty())).unwrap_or_default();
    }
    let Ok(mut line) = serde_json::to_string(&ev) else { return };
    line.push('\n');
    // One write for the whole line, so two hooks finishing at once can't interleave.
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Events added after byte `from`. Returns them and the new end of the file
/// (the file is cut down when it passes 256 KB).
pub fn read_since(from: u64) -> (Vec<Event>, u64) {
    let p = path();
    let Ok(meta) = std::fs::metadata(&p) else { return (Vec::new(), 0) };
    let len = meta.len();
    let from = if from > len { 0 } else { from }; // file was reset
    if len - from > 1024 * 1024 {
        return (Vec::new(), len); // a runaway backlog: skip it rather than stall the tray
    }
    let Ok(mut f) = std::fs::File::open(&p) else { return (Vec::new(), from) };
    use std::io::Seek;
    let _ = f.seek(std::io::SeekFrom::Start(from));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    // Only whole lines: a hook may be mid-write.
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let events = String::from_utf8_lossy(&buf[..end]).lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    (events, from + end as u64)
}

/// Cut the events file down when it is big (called when the tray starts): keep only the lines
/// the restart replay can still use (the last `REPLAY_SECS`), through a temp file and a rename.
pub fn trim() {
    let p = path();
    if std::fs::metadata(&p).map(|m| m.len() <= 256 * 1024).unwrap_or(true) {
        return;
    }
    let Ok(buf) = std::fs::read(&p) else { return };
    let kept = trim_text(&String::from_utf8_lossy(&buf), crate::time::now());
    let tmp = p.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, kept).is_ok() && std::fs::rename(&tmp, &p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The whole lines of an events file whose event is at most `REPLAY_SECS` old.
pub fn trim_text(text: &str, now: u64) -> String {
    text.lines()
        .filter(|l| serde_json::from_str::<Event>(l).is_ok_and(|e| now.saturating_sub(e.at) <= REPLAY_SECS))
        .map(|l| format!("{l}
"))
        .collect()
}

/// Every event in the last 1 MB of the file, however big the file is (for the restart replay).
pub fn read_recent() -> Vec<Event> {
    const MAX: u64 = 1024 * 1024;
    let Ok(mut f) = std::fs::File::open(path()) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(MAX);
    use std::io::Seek;
    let _ = f.seek(std::io::SeekFrom::Start(from));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    parse_tail(&buf, from > 0)
}

/// Events in a chunk of the file; when it starts mid-file the first, partial line is dropped.
pub fn parse_tail(buf: &[u8], from_mid_file: bool) -> Vec<Event> {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.lines();
    if from_mid_file {
        lines.next();
    }
    lines.filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Title of one chat from its record under the account's profile.
pub fn chat_title(cfg: &Config, account: &str, session: &str) -> Option<String> {
    if !session.starts_with("local_") || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    claude::chat_title(cfg, cfg.find(account)?, session)
}

// ---- did the reply end by asking you something? ------------------------------

/// The question a finished turn ends on, if any, from the chat's transcript:
/// the desktop app's next-step box with a question, or a reply whose last lines
/// ask something. Plain rules, read locally; nothing is sent anywhere.
pub fn question_in_transcript(path: &std::path::Path) -> Option<String> {
    reply_question(path).flatten()
}

/// None: no reply found yet (still being written); Some(None): a reply without a question.
fn reply_question(path: &std::path::Path) -> Option<Option<String>> {
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.take(TAIL).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut last_text: Option<String> = None;
    // Walk back through this turn: everything after your last message.
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let content = &v["message"]["content"];
        match v["type"].as_str() {
            Some("user") => {
                let typed = content.is_string() || content.as_array().is_some_and(|a| a.iter().any(|b| b["type"] == "text"));
                if typed {
                    break;
                }
            }
            Some("assistant") => {
                for b in content.as_array().into_iter().flatten().rev() {
                    match b["type"].as_str() {
                        // The desktop app's next-step box, when it asks a question.
                        Some("tool_use") if b["name"].as_str().is_some_and(|n| n.ends_with("__wrap_up")) => {
                            if let Some(ask) = b["input"]["ask"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                                return Some(Some(ask.to_string()));
                            }
                        }
                        Some("text") if last_text.is_none() => last_text = b["text"].as_str().map(str::to_string),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(question_at_end(&last_text?))
}

/// Markdown decoration off a line: "## Asks" -> "Asks", "1. **Do it?**" -> "Do it?".
/// How much of a reply an event keeps (cards show a cleaned, shorter part).
const MESSAGE_KEEP: usize = 1200;

/// A reply that is a JSON object or list: Codex's background turns answer that way.
pub fn background_reply(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde_json::Value>(t).is_ok_and(|v| v.is_object() || v.is_array())
}

/// A reply as card text: plain prose, no markdown marks or code blocks (only if there
/// is no prose at all, the first code lines), one paragraph, cut at a word to `max`
/// characters with an ellipsis.
pub fn card_text(text: &str, max: usize) -> String {
    let (mut prose, mut code) = (Vec::new(), Vec::new());
    let mut in_code = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if t.is_empty() || is_rule(t) {
            continue;
        }
        if in_code {
            if code.len() < 3 {
                code.push(t.to_string());
            }
            continue;
        }
        let s = inline_plain(&line_plain(t));
        let s = s.trim();
        if !s.is_empty() {
            // Lines run together as one paragraph: a heading leads into what follows,
            // and a line with no closing mark of its own ends as a sentence.
            let open = !s.ends_with(['.', '!', '?', ':', ';', ',', '\u{2026}']);
            prose.push(match (t.starts_with('#'), open) {
                (true, true) => format!("{s}:"),
                (false, true) => format!("{s}."),
                _ => s.to_string(),
            });
        }
    }
    let all = if prose.is_empty() { code.join(" ") } else { prose.join(" ") };
    cut_at_word(&all.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// A table border or horizontal rule.
fn is_rule(t: &str) -> bool {
    t.len() >= 3 && t.chars().all(|c| matches!(c, '-' | '*' | '_' | '=' | '|' | ':' | ' '))
}

/// One line without its heading, quote or list marker, and table bars as spaces.
fn line_plain(t: &str) -> String {
    let mut s = t;
    loop {
        let before = s;
        s = s.trim_start_matches('#').trim_start_matches('>').trim_start();
        for m in ["- [ ] ", "- [x] ", "- ", "* ", "+ "] {
            s = s.strip_prefix(m).unwrap_or(s);
        }
        let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && (s[digits..].starts_with(". ") || s[digits..].starts_with(") ")) {
            s = &s[digits + 2..];
        }
        if s == before {
            break;
        }
    }
    s.replace('|', " ")
}

/// Link text instead of links, and no bold, italic or code marks.
fn inline_plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let (before, after) = rest.split_at(i);
        out.push_str(before.strip_suffix('!').unwrap_or(before));
        // [text](target): keep the text.
        match after.find("](").filter(|&j| !after[1..j].contains('[')).and_then(|j| after[j + 2..].find(')').map(|k| (j, j + 2 + k))) {
            Some((j, end)) => {
                out.push_str(&after[1..j]);
                rest = &after[end + 1..];
            }
            None => {
                out.push('[');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    let out = out.replace("**", "").replace("__", "").replace('`', "");
    // A lone * marks italics when it touches a word; "5 * 3" keeps its star.
    let chars: Vec<char> = out.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|&(i, &c)| {
            c != '*' || {
                let l = i.checked_sub(1).and_then(|k| chars.get(k)).is_some_and(|c| !c.is_whitespace());
                let r = chars.get(i + 1).is_some_and(|c| !c.is_whitespace());
                !(l || r)
            }
        })
        .map(|(_, &c)| c)
        .collect()
}

/// `s` cut to at most `max` characters at a word, with an ellipsis when cut.
pub fn cut_at_word(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    let cut = match head.rfind(char::is_whitespace) {
        Some(i) if i >= head.len() / 2 => &head[..i],
        _ => head.as_str(),
    };
    format!("{}\u{2026}", cut.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | '-')))
}

fn plain(line: &str) -> String {
    let s = line.trim().trim_start_matches(|c: char| matches!(c, '#' | '>' | '-' | '*' | '+') || c.is_whitespace());
    let s = s.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches(['.', ')']);
    s.trim().trim_matches(['*', '_', '`']).trim().to_string()
}

/// The question at the end of a reply: the first item under a heading like
/// "Asks" or "Questions" near the end, else a last line that ends with a
/// question mark. Code blocks and link lines don't count.
pub fn question_at_end(text: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    let mut code = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with("```") {
            code = !code;
            continue;
        }
        if !code && !t.is_empty() {
            lines.push(t);
        }
    }
    const HEADINGS: [&str; 9] = ["asks", "ask", "questions", "question", "open questions", "questions for you", "decision needed", "decisions needed", "needs your input"];
    let from = lines.len().saturating_sub(12);
    for i in (from..lines.len()).rev() {
        let h = plain(lines[i]).to_lowercase();
        if HEADINGS.contains(&h.trim_end_matches(':').trim()) {
            return lines.get(i + 1).map(|l| plain(l)).filter(|s| !s.is_empty());
        }
    }
    for l in lines.iter().rev().take(4) {
        let s = plain(l);
        if s.starts_with('[') || s.starts_with("http") || s.to_lowercase().starts_with("sources") {
            continue; // links at the bottom
        }
        let end = s.trim_end_matches(['"', '\'', ')', '*', '_', '`', ' ']);
        if end.ends_with(['?', '\u{FF1F}', '\u{061F}']) {
            return Some(s);
        }
    }
    None
}

/// The two hook entries to add to Claude Code's settings.json.
pub fn hook_entry() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default().replace('\\', "/");
    let one = |arg: &str| {
        serde_json::json!([{ "hooks": [{ "type": "command", "command": format!("\"{exe}\" --event {arg}") }] }])
    };
    let v = serde_json::json!({ "hooks": { "Stop": one("stop"), "Notification": one("notify") } });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

/// How far back a restarted tray looks for cards that were still waiting for you.
pub const REPLAY_SECS: u64 = 12 * 3600;

/// One chat: its Code tab or Codex id, else Claude Code's own session id; without either
/// (older Codex), its account and folder.
pub fn chat_key(ev: &Event) -> String {
    if !ev.session.is_empty() {
        ev.session.clone()
    } else if !ev.chat.is_empty() {
        ev.chat.clone()
    } else {
        format!("{}|{}", ev.account, ev.cwd)
    }
}

/// Claude's "waiting for your input" nudge after a finished turn: not a question.
pub fn idle_nudge(ev: &Event) -> bool {
    ev.kind == "notify" && ev.message.to_ascii_lowercase().contains("waiting for your input")
}

/// After a restart: the needs-me and question events from the last `REPLAY_SECS` whose chat
/// has had nothing newer (an answer ends in a finished turn), was not opened since
/// (`opened`), and was not closed by you since (`dismissed`: chat key to the time you closed its card).
/// One per chat, oldest first.
pub fn unanswered(events: &[Event], now: u64, dismissed: &HashMap<String, u64>, opened: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut newest: HashMap<String, &Event> = HashMap::new();
    for ev in events {
        let k = chat_key(ev);
        if newest.get(&k).map_or(true, |n| ev.at >= n.at) {
            newest.insert(k, ev);
        }
    }
    let mut out: Vec<Event> = newest
        .into_iter()
        .filter(|(k, ev)| {
            (ev.kind == "notify" || ev.kind == "ask")
                && !idle_nudge(ev)
                && now.saturating_sub(ev.at) <= REPLAY_SECS
                && dismissed.get(k).map_or(true, |&at| ev.at > at)
                && !opened(ev)
        })
        .map(|(_, ev)| ev.clone())
        .collect();
    out.sort_by_key(|e| e.at);
    out
}

fn replayed_path() -> PathBuf {
    crate::data_file("hive-replayed.json")
}

/// Chats whose card you closed yourself (chat key to when). The file also holds the entries 103
/// wrote for re-shown cards (chat key to event time); both mean "hide until a newer event".
pub fn load_replayed() -> HashMap<String, u64> {
    std::fs::read_to_string(replayed_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// You closed a chat's needs-me or question card yourself: do not bring it back after a restart.
/// (Cards a restart closes never come through here.)
pub fn mark_dismissed(chat: &str, now: u64) {
    let mut m = load_replayed();
    m.insert(chat.to_string(), now);
    save_replayed(&m);
}

pub fn save_replayed(m: &HashMap<String, u64>) {
    let now = crate::time::now();
    let m: HashMap<&String, &u64> = m.iter().filter(|(_, &at)| now.saturating_sub(at) <= REPLAY_SECS).collect();
    if let Ok(t) = serde_json::to_string(&m) {
        let _ = std::fs::write(replayed_path(), t);
    }
}

/// One log line per project per hour for Codex turns that get no card. The hook runs
/// as a short-lived process per turn, so the running count lives in a small file.
const NO_CARD_FILE: &str = "hive-codex-nocard.json";

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct NoCard {
    since: u64,
    count: u32,
    reasons: Vec<String>,
}

/// Count one no-card turn for `cwd`. Returns the lines to log: the first turn of each
/// project-hour at once (as before), and the summary with the count when the next hour's
/// first turn arrives (only when the hour had more than that one turn).
fn tally_no_card(map: &mut HashMap<String, NoCard>, cwd: &str, why: &str, now: u64) -> Vec<String> {
    let e = map.entry(cwd.to_string()).or_default();
    let mut lines = Vec::new();
    if e.count > 1 && now.saturating_sub(e.since) >= 3600 {
        lines.push(format!("codex: {} turns with no card in {cwd} in the last hour ({})", e.count, e.reasons.join("; ")));
    }
    if e.count > 0 && now.saturating_sub(e.since) >= 3600 {
        *e = NoCard::default();
    }
    if e.count == 0 {
        e.since = now;
        lines.push(format!("codex: no card for a turn in {cwd} ({why}); further turns this hour are counted, not logged"));
    }
    e.count += 1;
    if !e.reasons.iter().any(|r| r == why) && e.reasons.len() < 5 {
        e.reasons.push(why.to_string());
    }
    lines
}

/// Read-modify-write the counts under a lock file and replace the file by rename; a
/// missing or damaged file starts empty.
fn update_no_card(path: &std::path::Path, cwd: &str, why: &str, now: u64) -> Vec<String> {
    let lock = path.with_extension("lock");
    let mut tries = 0;
    loop {
        let stale = std::fs::metadata(&lock).and_then(|m| m.modified()).ok().is_some_and(|t| t.elapsed().map_or(false, |e| e.as_secs() > 5));
        if stale {
            let _ = std::fs::remove_file(&lock);
        }
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_ok() {
            break;
        }
        tries += 1;
        if tries > 120 {
            return Vec::new(); // never hold up a hook for long; this turn is not counted
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let mut map: HashMap<String, NoCard> = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let lines = tally_no_card(&mut map, cwd, why, now);
    if let Ok(t) = serde_json::to_vec(&map) {
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, t).is_ok() {
            let mut renamed = false;
            for i in 0..5 {
                if std::fs::rename(&tmp, path).is_ok() {
                    renamed = true;
                    break;
                }
                if i < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            if !renamed {
                let _ = std::fs::remove_file(&tmp);
            }
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    let _ = std::fs::remove_file(&lock);
    lines
}

fn note_no_card(cwd: &str, why: &str) {
    for l in update_no_card(&crate::data_file(NO_CARD_FILE), cwd, why, crate::time::now()) {
        crate::log(&l);
    }
}

// ---- goal chats ---------------------------------------------------------------

/// How long a goal event waits for the goal check's verdict before it is shown anyway.
pub const GOAL_WAIT_SECS: u64 = 90;
/// How much of a transcript's end is searched for goal records.
const GOAL_TAIL: u64 = 2 * 1024 * 1024;

/// One `goal_status` record of a transcript: when it was written and whether the goal is over
/// (`met:true` or `failed:true`). Otherwise the chat goes on by itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GoalRec {
    pub at: u64,
    pub ended: bool,
}

/// Unix seconds of an ISO time like `2026-11-24T15:37:00.597Z`.
fn iso_secs(t: &str) -> Option<u64> {
    let n = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, m, d, h, mi, s) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + s).ok()
}

/// The goal records in a chunk of transcript, oldest first.
pub fn goal_records(text: &str) -> Vec<GoalRec> {
    text.lines()
        .filter(|l| l.contains("goal_status"))
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            let a = &v["attachment"];
            if a["type"] != "goal_status" {
                return None;
            }
            let ended = a["met"] == true || a["failed"] == true;
            Some(GoalRec { at: iso_secs(v["timestamp"].as_str()?).unwrap_or(0), ended })
        })
        .collect()
}

/// The end of a transcript as text (the first line may be cut).
fn transcript_tail(path: &std::path::Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(len.saturating_sub(GOAL_TAIL))).ok()?;
    let mut buf = Vec::new();
    f.take(GOAL_TAIL).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A goal is running: the newest `goal_status` record is the sentinel or `met:false` without `failed`.
pub fn goal_active_in(text: &str) -> bool {
    goal_records(text).last().is_some_and(|r| !r.ended)
}

pub fn goal_active(path: &std::path::Path) -> bool {
    transcript_tail(path).is_some_and(|t| goal_active_in(&t))
}

/// What the goal check decided about the turn a goal event reported.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Goal {
    /// Not a goal event: nothing to wait for.
    None,
    /// No verdict yet (and less than `GOAL_WAIT_SECS` old).
    Pending,
    /// The goal goes on by itself: nothing for you to do.
    Continued,
    /// The goal is over (met or failed): the chat really stopped.
    Ended,
    /// No verdict in time, or the transcript cannot be read: show it.
    Unknown,
}

/// The first verdict written at or after `at` in a chunk of transcript, else Pending or Unknown by age.
pub fn goal_in(text: &str, at: u64, now: u64) -> Goal {
    match goal_records(text).into_iter().find(|r| r.at >= at) {
        Some(r) if r.ended => Goal::Ended,
        Some(_) => Goal::Continued,
        None if now.saturating_sub(at) >= GOAL_WAIT_SECS => Goal::Unknown,
        None => Goal::Pending,
    }
}

thread_local! {
    /// Settled verdicts, by transcript and event time, so a chat's file is read once for each.
    static GOAL_SETTLED: std::cell::RefCell<HashMap<(String, u64), Goal>> = std::cell::RefCell::new(HashMap::new());
}

/// The verdict for an event: reads the transcript's end until it is settled.
pub fn goal_verdict(ev: &Event, now: u64) -> Goal {
    if !ev.goal {
        return Goal::None;
    }
    let key = (ev.transcript.clone(), ev.at);
    if let Some(g) = GOAL_SETTLED.with(|m| m.borrow().get(&key).copied()) {
        return g;
    }
    let g = match transcript_tail(std::path::Path::new(&ev.transcript)) {
        Some(t) => goal_in(&t, ev.at, now),
        None => Goal::Unknown,
    };
    if matches!(g, Goal::Continued | Goal::Ended) {
        GOAL_SETTLED.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() > 200 {
                m.clear();
            }
            m.insert(key, g);
        });
    }
    g
}

/// Whether a list (waiting list, restart replay) may show this event: not while a goal chat's
/// turn is undecided, and not when the goal check kept the chat going.
pub fn goal_shown(ev: &Event, now: u64) -> bool {
    !matches!(goal_verdict(ev, now), Goal::Pending | Goal::Continued)
}

/// What the tray does with a new event.
#[derive(Debug, PartialEq)]
pub enum Offer {
    /// Handle it as usual (not a goal event, or its verdict is in and it ended).
    Pass,
    /// Kept back until the goal check decides.
    Held,
    /// The goal chat goes on by itself: no card.
    Dropped,
}

/// Goal events waiting for their verdict, one per chat (a newer event replaces it).
#[derive(Default)]
pub struct GoalHold {
    held: HashMap<String, Event>,
}

impl GoalHold {
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// A newer event (a replayed one included) replaces a held one of the same chat.
    pub fn forget(&mut self, ev: &Event) {
        self.held.remove(&chat_key(ev));
    }

    pub fn offer(&mut self, ev: &Event, now: u64) -> Offer {
        self.held.remove(&chat_key(ev));
        match goal_verdict(ev, now) {
            Goal::Pending => {
                self.held.insert(chat_key(ev), ev.clone());
                Offer::Held
            }
            Goal::Continued => Offer::Dropped,
            _ => Offer::Pass,
        }
    }

    /// Held events whose verdict has come (or whose wait ran out) as `(event, dropped)`; the rest stay.
    pub fn due(&mut self, now: u64) -> Vec<(Event, bool)> {
        let mut out = Vec::new();
        self.held.retain(|_, ev| match goal_verdict(ev, now) {
            Goal::Pending => true,
            Goal::Continued => {
                out.push((ev.clone(), true));
                false
            }
            _ => {
                out.push((ev.clone(), false));
                false
            }
        });
        out.sort_by_key(|(e, _)| e.at);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, session: &str, at: u64) -> Event {
        Event { kind: kind.into(), account: "Personal".into(), session: session.into(), at, ..Default::default() }
    }

    const NOW: u64 = 1_000_000;

    #[test]
    fn no_card_turns_log_once_per_project_per_hour() {
        let mut m = HashMap::new();
        let lines: Vec<_> = (0..12).flat_map(|i| tally_no_card(&mut m, "proj_p", "subagent", NOW + i * 60)).collect();
        // 12 turns then silence: the first turn was logged at once.
        assert_eq!(lines.len(), 1, "{lines:?}");
        // The next hour's first turn writes the 12-turn summary and its own first line.
        let l = tally_no_card(&mut m, "proj_p", "subagent", NOW + 3700);
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(l[0].contains("12 turns with no card"), "{l:?}");
        // Another project is separate.
        assert_eq!(tally_no_card(&mut m, "proj_q", "subagent", NOW + 3701).len(), 1);
        // A one-turn hour needs no summary, only the new first line.
        let l2 = tally_no_card(&mut m, "proj_p", "subagent", NOW + 3700 + 3600);
        assert_eq!(l2.len(), 1, "{l2:?}");
    }

    #[test]
    fn the_no_card_file_survives_damage_and_repeats() {
        let dir = std::env::temp_dir().join(format!("hive-nocard-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("n.json");
        std::fs::write(&f, b"{ not json").unwrap();
        assert_eq!(update_no_card(&f, "p", "x", NOW).len(), 1);
        assert!(update_no_card(&f, "p", "x", NOW + 60).is_empty());
        let m: HashMap<String, NoCard> = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(m["p"].count, 2);
        assert!(!f.with_extension("lock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unanswered_question_from_2h_ago_comes_back() {
        let got = unanswered(&[ev("ask", "local_a", NOW - 7200)], NOW, &HashMap::new(), |_| false);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session, "local_a");
    }

    #[test]
    fn too_old_finished_and_answered_do_not_come_back() {
        let none = HashMap::new();
        assert!(unanswered(&[ev("ask", "local_a", NOW - REPLAY_SECS - 1)], NOW, &none, |_| false).is_empty());
        assert!(unanswered(&[ev("stop", "local_a", NOW - 60)], NOW, &none, |_| false).is_empty());
        // asked, then the turn after your answer finished
        let answered = [ev("ask", "local_a", NOW - 600), ev("stop", "local_a", NOW - 60)];
        assert!(unanswered(&answered, NOW, &none, |_| false).is_empty());
    }

    #[test]
    fn an_opened_chat_does_not_come_back() {
        let e = [ev("notify", "local_a", NOW - 600)];
        assert!(unanswered(&e, NOW, &HashMap::new(), |_| true).is_empty());
    }

    #[test]
    fn one_card_per_chat_and_each_event_once() {
        let e = [ev("notify", "local_a", NOW - 900), ev("ask", "local_a", NOW - 600), ev("ask", "local_b", NOW - 300)];
        let got = unanswered(&e, NOW, &HashMap::new(), |_| false);
        assert_eq!(got.iter().map(|g| (g.session.as_str(), g.kind.as_str())).collect::<Vec<_>>(), vec![("local_a", "ask"), ("local_b", "ask")]);
        // after the first replay both are recorded: a second restart shows nothing
        let done: HashMap<String, u64> = got.iter().map(|g| (chat_key(g), g.at)).collect();
        assert!(unanswered(&e, NOW, &done, |_| false).is_empty());
        // a newer question in the same chat is shown again
        let newer = [e[1].clone(), ev("ask", "local_a", NOW - 10)];
        assert_eq!(unanswered(&newer, NOW, &done, |_| false).len(), 1);
    }

    #[test]
    fn trim_keeps_only_the_last_12_hours_as_whole_lines() {
        let line = |at: u64| serde_json::to_string(&ev("ask", "local_a", at)).unwrap();
        let text = format!("{}
not json
{}
{}
", line(NOW - REPLAY_SECS - 5), line(NOW - 100), line(NOW - 10));
        let kept = trim_text(&text, NOW);
        assert_eq!(kept, format!("{}
{}
", line(NOW - 100), line(NOW - 10)));
        // and the replay still finds the recent question
        let evs = parse_tail(kept.as_bytes(), false);
        assert_eq!(unanswered(&evs, NOW, &HashMap::new(), |_| false).len(), 1);
    }

    #[test]
    fn a_tail_read_drops_the_partial_first_line() {
        let line = |at: u64| serde_json::to_string(&ev("ask", "local_b", at)).unwrap();
        let full = format!("{}
{}
", line(NOW - 50), line(NOW - 40));
        let cut = &full.as_bytes()[10..]; // starts inside the first line
        let got = parse_tail(cut, true);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].at, NOW - 40);
        assert_eq!(parse_tail(full.as_bytes(), false).len(), 2);
    }

    #[test]
    fn a_dismissed_chat_is_not_replayed_but_a_newer_question_is() {
        let e = [ev("ask", "local_a", NOW - 600)];
        let dismissed = HashMap::from([("local_a".to_string(), NOW - 300)]);
        assert!(unanswered(&e, NOW, &dismissed, |_| false).is_empty());
        let newer = [ev("ask", "local_a", NOW - 100)];
        assert_eq!(unanswered(&newer, NOW, &dismissed, |_| false).len(), 1);
    }

    #[test]
    fn an_unhandled_card_returns_after_every_restart() {
        let e = [ev("ask", "local_a", NOW - 600), ev("notify", "local_b", NOW - 300)];
        let dismissed = HashMap::new(); // nothing is recorded by showing a card
        let first = unanswered(&e, NOW, &dismissed, |_| false);
        let second = unanswered(&e, NOW + 60, &dismissed, |_| false);
        assert_eq!(first.len(), 2);
        assert_eq!(first, second);
    }

    #[test]
    fn entries_from_the_old_replayed_file_still_suppress_until_a_newer_event() {
        let old: HashMap<String, u64> = serde_json::from_str(r#"{"local_a": 999000}"#).unwrap();
        assert!(unanswered(&[ev("ask", "local_a", 999000)], NOW, &old, |_| false).is_empty());
        assert_eq!(unanswered(&[ev("ask", "local_a", 999500)], NOW, &old, |_| false).len(), 1);
    }

    #[test]
    fn the_idle_nudge_is_not_a_question() {
        let mut n = ev("notify", "local_a", NOW - 60);
        n.message = "Claude is waiting for your input".into();
        assert!(unanswered(&[n], NOW, &HashMap::new(), |_| false).is_empty());
    }

    fn p(pid: u32, ppid: u32) -> Proc {
        Proc { pid, ppid, name: String::new() }
    }

    #[test]
    fn finds_the_window_above_a_hook() {
        // hook shell 50 -> claude cli 40 -> helper 30 -> Claude main 20 -> explorer 10
        let procs = vec![p(10, 1), p(20, 10), p(30, 20), p(40, 30), p(50, 40)];
        let chain = ancestors(&procs, 50);
        assert_eq!(chain, vec![50, 40, 30, 20, 10, 1]);
        let mains = HashMap::from([("Team".to_string(), 20u32), ("Other".to_string(), 99u32)]);
        assert_eq!(account_in_chain(&mains, &chain), Some("Team".to_string()));
        assert_eq!(account_in_chain(&mains, &[7, 8]), None);
    }

    #[test]
    fn a_parent_loop_does_not_hang() {
        let procs = vec![p(1, 2), p(2, 1)];
        assert!(ancestors(&procs, 1).len() <= 2);
    }

    #[test]
    fn questions_at_the_end_are_found() {
        let asks = "Done.\n\n## Asks\n1. Put all eleven in together? Recommend: yes.\n";
        assert_eq!(question_at_end(asks).as_deref(), Some("Put all eleven in together? Recommend: yes."));
        assert_eq!(question_at_end("It works.\n\nWant me to add the toggle next?").as_deref(), Some("Want me to add the toggle next?"));
        assert_eq!(question_at_end("**Questions:**\n- Which folder?").as_deref(), Some("Which folder?"));
        assert_eq!(question_at_end("Use it like this.\n\n[README](x.md)\nShould I publish it?").as_deref(), Some("Should I publish it?"));
        // Not questions: a plain ending, a question inside code, a question far above the end.
        assert_eq!(question_at_end("All done. Tests pass."), None);
        assert_eq!(question_at_end("Run:\n```\nwhy?\n```\nThat's it."), None);
        assert_eq!(question_at_end("Why did it fail?\n\nIt was the cache.\nFixed it.\nTests pass.\nInstalled.\nDone."), None);
        assert_eq!(question_at_end("Asks unchanged (2)"), None);
    }

    #[test]
    fn background_turns_are_json_replies() {
        assert!(background_reply("{\"suggestions\":[]}"));
        assert!(background_reply(" {\"suggestion\":\"Finish the refresh and reopen\"} "));
        assert!(background_reply("[1, 2]"));
        assert!(!background_reply("Done. {not json} here"));
        assert!(!background_reply("{ looks like json but isn't"));
        assert!(!background_reply("All set: the build passes."));
    }

    #[test]
    fn card_text_is_plain_prose() {
        assert_eq!(
            card_text("**[src/claude.rs:130](C:/Users/x/claude.rs:130)**: the label `&id[..8]` can *panic*.", 200),
            "src/claude.rs:130: the label &id[..8] can panic."
        );
        assert_eq!(
            card_text("```text\nraw output\n```\n\n## Result\n\n- First point\n- Second __point__\n\n---\n| a | b |\n|---|---|", 200),
            "Result: First point. Second point. a b."
        );
        assert_eq!(card_text("```\nonly code here\n```", 200), "only code here");
        assert_eq!(card_text("5 * 3 is 15, see ![chart](c.png).", 200), "5 * 3 is 15, see chart.");
        assert_eq!(card_text("1. Start with one example\n2) Then two", 200), "Start with one example. Then two.");
    }

    #[test]
    fn long_text_is_cut_at_a_word() {
        let t = card_text("Start with one example of the change, then widen it to every account once it reads well.", 40);
        assert_eq!(t, "Start with one example of the change\u{2026}");
        assert!(t.chars().count() <= 40);
        assert_eq!(cut_at_word("short", 40), "short");
    }

    #[test]
    fn a_forwarded_account_option_is_not_ours() {
        let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(account_option(&v(&["codex", "--then", "helper.exe", "--account", "h", "{}"])), None);
        assert_eq!(account_option(&v(&["codex", "--account", "Codex 2", "--then", "helper.exe", "--account", "h"])).as_deref(), Some("Codex 2"));
        assert_eq!(account_option(&v(&["stop", "--account", "Team"])).as_deref(), Some("Team"));
    }

    #[test]
    fn event_round_trips() {
        let e = Event { kind: "stop".into(), account: "Team".into(), session: "local_abc".into(), at: 5, ..Event::default() };
        let line = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), e);
    }

    // ---- goal chats ----

    const T0: &str = "2026-11-24T15:36:58.513Z";
    fn t0() -> u64 {
        iso_secs(T0).unwrap()
    }

    fn goal_line(ts: &str, body: &str) -> String {
        format!("{{\"attachment\":{{\"type\":\"goal_status\",{body}}},\"type\":\"attachment\",\"timestamp\":\"{ts}\"}}\n")
    }

    fn goal_ev(path: &std::path::Path, at: u64) -> Event {
        Event { kind: "ask".into(), session: "local_g".into(), at, goal: true, transcript: path.to_string_lossy().into(), ..Default::default() }
    }

    fn temp_transcript(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hive-goal-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join(name);
        std::fs::write(&f, text).unwrap();
        f
    }

    #[test]
    fn iso_time_parses_to_unix_seconds() {
        assert_eq!(iso_secs("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_secs("2026-11-24T15:37:00.597Z"), Some(1_795_534_620));
        assert_eq!(iso_secs("nonsense"), None);
    }

    #[test]
    fn goal_is_active_only_while_the_newest_record_goes_on() {
        let sentinel = goal_line("2026-11-24T10:00:00.000Z", "\"met\":false,\"sentinel\":true,\"condition\":\"c\"");
        let goes_on = goal_line("2026-11-24T10:05:00.000Z", "\"met\":false,\"condition\":\"c\",\"reason\":\"r\"");
        let met = goal_line("2026-11-24T10:10:00.000Z", "\"met\":true,\"condition\":\"c\"");
        let failed = goal_line("2026-11-24T10:10:00.000Z", "\"met\":false,\"failed\":true,\"condition\":\"c\"");
        assert!(goal_active_in(&sentinel));
        assert!(goal_active_in(&format!("{sentinel}{goes_on}")));
        assert!(!goal_active_in(&format!("{sentinel}{goes_on}{met}")));
        assert!(!goal_active_in(&format!("{sentinel}{goes_on}{failed}")));
        // A new goal after a finished one is active again.
        assert!(goal_active_in(&format!("{met}{sentinel}")));
        // No goal record at all: a normal chat.
        assert!(!goal_active_in("{\"type\":\"assistant\"}\nnot json\n"));
    }

    #[test]
    fn held_goal_event_is_dropped_when_the_verdict_says_the_chat_goes_on() {
        let before = goal_line("2026-11-24T15:30:00.000Z", "\"met\":false,\"condition\":\"c\",\"reason\":\"r\"");
        let ask = "{\"type\":\"assistant\",\"timestamp\":\"2026-11-24T15:36:58.513Z\"}\n";
        let f = temp_transcript("goes-on.jsonl", &format!("{before}{ask}"));
        let e = goal_ev(&f, t0());
        let mut hold = GoalHold::default();
        // The hook ran before the verdict: the older record does not count, so it waits.
        assert_eq!(hold.offer(&e, t0() + 1), Offer::Held);
        assert!(!goal_shown(&e, t0() + 1));
        assert!(hold.due(t0() + 3).is_empty());
        // The verdict lands 2 s later.
        std::fs::write(&f, format!("{before}{ask}{}", goal_line("2026-11-24T15:37:00.597Z", "\"met\":false,\"condition\":\"c\",\"reason\":\"r\""))).unwrap();
        assert!(!goal_shown(&e, t0() + 4), "not in the waiting list");
        let due = hold.due(t0() + 4);
        assert_eq!(due.len(), 1);
        assert!(due[0].1, "dropped, no card");
        assert!(hold.is_empty());
        // The same event offered afresh is dropped at once.
        assert_eq!(GoalHold::default().offer(&e, t0() + 5), Offer::Dropped);
    }

    #[test]
    fn held_goal_event_shows_when_the_goal_ended_and_stays_an_ask() {
        for (name, body) in [("met.jsonl", "\"met\":true,\"condition\":\"c\""), ("failed.jsonl", "\"met\":false,\"failed\":true,\"condition\":\"c\"")] {
            let f = temp_transcript(name, "{\"type\":\"assistant\"}\n");
            let e = goal_ev(&f, t0());
            let mut hold = GoalHold::default();
            assert_eq!(hold.offer(&e, t0() + 1), Offer::Held);
            std::fs::write(&f, format!("{{\"type\":\"assistant\"}}\n{}", goal_line("2026-11-24T15:37:00.597Z", body))).unwrap();
            let due = hold.due(t0() + 3);
            assert_eq!(due.len(), 1, "{name}");
            assert!(!due[0].1, "{name}: shown");
            assert_eq!(due[0].0.kind, "ask");
            assert!(goal_shown(&e, t0() + 3));
        }
    }

    #[test]
    fn held_goal_event_shows_after_90_seconds_without_a_verdict() {
        let f = temp_transcript("silent.jsonl", "{\"type\":\"assistant\"}\n");
        let e = goal_ev(&f, t0());
        let mut hold = GoalHold::default();
        assert_eq!(hold.offer(&e, t0() + 1), Offer::Held);
        assert!(hold.due(t0() + GOAL_WAIT_SECS - 1).is_empty());
        let due = hold.due(t0() + GOAL_WAIT_SECS);
        assert_eq!(due.len(), 1);
        assert!(!due[0].1);
        // An unreadable transcript is shown at once.
        let gone = goal_ev(std::path::Path::new("Z:/no/such/transcript.jsonl"), t0());
        assert_eq!(GoalHold::default().offer(&gone, t0() + 1), Offer::Pass);
    }

    #[test]
    fn newer_event_replaces_a_held_one_and_normal_events_pass() {
        let f = temp_transcript("replace.jsonl", "{\"type\":\"assistant\"}\n");
        let e = goal_ev(&f, t0());
        let mut hold = GoalHold::default();
        assert_eq!(hold.offer(&e, t0() + 1), Offer::Held);
        let newer = Event { kind: "stop".into(), session: "local_g".into(), at: t0() + 20, ..Default::default() };
        assert_eq!(hold.offer(&newer, t0() + 21), Offer::Pass);
        assert!(hold.is_empty());
        // A normal chat's events are untouched, whatever the kind.
        for kind in ["stop", "ask", "notify", "codex"] {
            let n = Event { kind: kind.into(), session: "local_n".into(), at: t0(), ..Default::default() };
            assert_eq!(goal_verdict(&n, t0()), Goal::None);
            assert_eq!(GoalHold::default().offer(&n, t0()), Offer::Pass);
            assert!(goal_shown(&n, t0()));
        }
    }

    #[test]
    fn goal_fields_round_trip_and_stay_out_of_normal_events() {
        let normal = serde_json::to_string(&ev("stop", "local_a", 5)).unwrap();
        assert!(!normal.contains("goal") && !normal.contains("transcript"), "{normal}");
        let f = temp_transcript("rt.jsonl", "");
        let e = goal_ev(&f, 7);
        let back: Event = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }

    /// Probe against copies of the real 02e98eed transcript (run by hand with
    /// HIVE_GOAL_CUT = copy ending at the 15:36:58 ask, HIVE_GOAL_FULL = copy through the 15:37:00 verdict).
    #[test]
    #[ignore]
    fn replay_of_the_real_goal_transcript() {
        let cut = PathBuf::from(std::env::var("HIVE_GOAL_CUT").unwrap());
        let full = PathBuf::from(std::env::var("HIVE_GOAL_FULL").unwrap());
        assert!(goal_active(&cut), "the hook marks the ask as a goal event");
        let at = t0();
        let mut e = goal_ev(&cut, at);
        println!("cut copy: goal_active=true, verdict at +1s = {:?}", goal_verdict(&e, at + 1));
        assert_eq!(goal_verdict(&e, at + 1), Goal::Pending);
        e.transcript = full.to_string_lossy().into();
        println!("full copy: verdict at +4s = {:?}", goal_verdict(&e, at + 4));
        assert_eq!(GoalHold::default().offer(&e, at + 4), Offer::Dropped);
        assert!(!goal_shown(&e, at + 4));
    }
}
