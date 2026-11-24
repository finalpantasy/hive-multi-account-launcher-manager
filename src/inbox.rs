//! A file inbox so worker chats can reach the coordinator when SendMessage cannot.
//!
//!   Hive.exe --inbox post <to> <card> <status> <text...> [--from <name>]
//!   Hive.exe --inbox read <to> [--all]
//!   Hive.exe --inbox hook <to>        (UserPromptSubmit hook: unread lines, else nothing)
//!
//! One append-only `<to>.jsonl` per recipient, plus `<to>.cursor` (the byte offset already
//! read). The folder is `hive-control\inbox` next to the exe's folder, or HIVE_INBOX_DIR
//! (tests use a temp folder).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

const MAX_TEXT: usize = 2048;
const MAX_LINES: usize = 20;
const MAX_OUT: usize = 3072;
const STATUSES: [&str; 4] = ["done", "blocked", "question", "info"];
const USAGE: &str = "usage:\n  Hive --inbox post <to> <card> <done|blocked|question|info> <text...> [--from <name>]\n  Hive --inbox read <to> [--all]\n  Hive --inbox hook <to>";

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HIVE_INBOX_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    crate::exe_dir().join("hive-control").join("inbox")
}

/// A recipient name becomes a file name: letters, digits, `-` and `_` only.
fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// At most `max` bytes, cut on a character boundary.
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// Append one line. Returns false when the status or recipient is not valid.
pub fn post(to: &str, card: &str, status: &str, text: &str, from: &str) -> Result<(), String> {
    if !valid_name(to) {
        return Err("bad <to>".into());
    }
    let status = status.to_ascii_lowercase();
    if !STATUSES.contains(&status.as_str()) {
        return Err("bad <status>".into());
    }
    let line = serde_json::json!({
        "time": crate::win::timestamp(),
        "at": crate::time::now(),
        "from": clip(from, 64),
        "to": to,
        "card": clip(card, 32),
        "status": status,
        "text": clip(text, MAX_TEXT),
    });
    let mut bytes = serde_json::to_vec(&line).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    let path = d.join(format!("{to}.jsonl"));
    // One write call on an append handle: two posts at once do not interleave. A sharing
    // violation (a reader holding the file) is retried for about 3 seconds.
    for _ in 0..120 {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            if f.write_all(&bytes).is_ok() {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err("could not write the inbox file".into())
}

fn cursor_of(to: &str) -> u64 {
    std::fs::read_to_string(dir().join(format!("{to}.cursor"))).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn set_cursor(to: &str, at: u64) {
    let d = dir();
    let tmp = d.join(format!("{to}.cursor.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, at.to_string()).is_err() {
        return;
    }
    // A reader can hold the cursor file for a moment: retry the rename like post does.
    for _ in 0..5 {
        if std::fs::rename(&tmp, d.join(format!("{to}.cursor"))).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = std::fs::remove_file(&tmp);
}

/// Most characters of a post's text shown in a read or hook line.
const SHOW_TEXT: usize = 300;

/// A session id or placeholder is not a name worth showing in front of the card label.
fn is_session_like(from: &str) -> bool {
    let f = from.to_ascii_lowercase();
    f.is_empty()
        || f == "unknown"
        || f.starts_with("local_")
        || f.starts_with("session_")
        || (f.len() == 36 && f.matches('-').count() == 4 && f.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
}

/// The card label: "card 121" whether the post said `121` or `card 121`. The second value
/// is the bare id.
fn card_label(card: &str) -> (String, String) {
    let c = card.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = c.to_ascii_lowercase();
    let id = match lower.strip_prefix("card") {
        Some(rest) if rest.is_empty() || rest.starts_with(|ch: char| !ch.is_ascii_alphanumeric()) => c[4..].trim().to_string(),
        _ => c,
    };
    if id.is_empty() {
        ("card".into(), id)
    } else {
        (format!("card {id}"), id)
    }
}

/// Drop a leading copy of the card label and of the status word ("card 121 DONE: ...") that
/// workers often type into the text, since the line already shows both.
fn strip_dupes(text: &str, id: &str, status: &str) -> String {
    let mut t = text.trim();
    loop {
        let lower = t.to_ascii_lowercase();
        let mut cut = 0;
        let card_prefix = format!("card {}", id.to_ascii_lowercase());
        if !id.is_empty() && lower.starts_with(&card_prefix) {
            let rest = &lower[card_prefix.len()..];
            if rest.is_empty() || rest.starts_with(|c: char| !c.is_ascii_alphanumeric()) {
                cut = card_prefix.len();
            }
        }
        if cut == 0 && !status.is_empty() {
            let st = status.to_ascii_lowercase();
            if lower.starts_with(&st) && lower[st.len()..].starts_with(':') {
                cut = st.len() + 1;
            }
        }
        if cut == 0 {
            break;
        }
        t = t[cut..].trim_start_matches(|c: char| c.is_whitespace() || c == ':' || c == '-').trim_start();
    }
    t.to_string()
}

/// Whitespace runs (newlines and tabs included) become one space; control characters go.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// At most `max` characters; a longer text ends in "...".
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// One stored JSON line as a readable line, "[time] card <id> <STATUS>: <text>". A line that
/// is not JSON is shown as it is.
fn show(raw: &str) -> String {
    let line = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => {
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let (label, id) = card_label(&s("card"));
            let from = s("from");
            let from = from.trim();
            let who = if is_session_like(from) || from.eq_ignore_ascii_case(&label) || from.eq_ignore_ascii_case(&id) {
                String::new()
            } else {
                format!("{} ", one_line(from))
            };
            let status = s("status");
            let text = one_line(&strip_dupes(&one_line(&s("text")), &id, &status));
            let text = if text.is_empty() { "(no text)".to_string() } else { cap_chars(&text, SHOW_TEXT) };
            format!("[{}] {who}{label} {}: {text}", s("time"), status.to_uppercase())
        }
        Err(_) => raw.to_string(),
    };
    // Whatever the line holds, it prints within the output cap (with its newline).
    clip(&line, MAX_OUT - 1).to_string()
}

/// Unread lines (at most 20 lines and 3 KB, the first line always) and the offset just
/// after the last one returned. With `mark`, that offset becomes the cursor.
pub fn read_unread(to: &str, mark: bool) -> Vec<String> {
    let path = dir().join(format!("{to}.jsonl"));
    let Ok(mut f) = std::fs::File::open(&path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut start = cursor_of(to);
    if start > len {
        start = 0; // the file was replaced
    }
    // A cursor must sit at a line start; otherwise the file was re-created: read it all.
    if start > 0 {
        let mut b = [0u8; 1];
        let ok = f.seek(SeekFrom::Start(start - 1)).is_ok() && f.read_exact(&mut b).is_ok() && b[0] == b'\n';
        if !ok {
            start = 0;
        }
    }
    if start == len {
        return Vec::new();
    }
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.take(len - start).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut used = 0;
    let mut end = 0usize;
    for chunk in buf.split_inclusive(|b| *b == b'\n') {
        if !chunk.ends_with(b"\n") {
            break; // a post still being written
        }
        let line = show(String::from_utf8_lossy(chunk).trim_end());
        if out.len() >= MAX_LINES || (!out.is_empty() && used + line.len() + 1 > MAX_OUT) {
            break;
        }
        used += line.len() + 1;
        out.push(line);
        end += chunk.len();
    }
    if mark && end > 0 {
        set_cursor(to, start + end as u64);
    }
    out
}

/// The last 20 lines (3 KB at most), oldest first, without marking anything read.
pub fn read_all(to: &str) -> Vec<String> {
    let Ok(text) = std::fs::read(dir().join(format!("{to}.jsonl"))) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    let mut used = 0;
    for chunk in text.split_inclusive(|b| *b == b'\n').rev() {
        if !chunk.ends_with(b"\n") {
            continue;
        }
        let line = show(String::from_utf8_lossy(chunk).trim_end());
        if out.len() >= MAX_LINES || (!out.is_empty() && used + line.len() + 1 > MAX_OUT) {
            break;
        }
        used += line.len() + 1;
        out.push(line);
    }
    out.reverse();
    out
}

/// What `--inbox hook <to>` prints: nothing for no unread lines, or for missing, bad or
/// extra arguments.
fn hook_lines(args: &[String]) -> Vec<String> {
    let Some(to) = args.first().filter(|t| valid_name(t)) else { return Vec::new() };
    let lines = read_unread(to, true);
    if lines.is_empty() {
        return lines;
    }
    let mut out = vec![format!("Hive inbox ({} new for {to}):", lines.len())];
    out.extend(lines);
    out
}

/// The calling chat's own session id from the environment (empty when there is none).
fn session_from_env() -> String {
    std::env::var("CLAUDE_CODE_HOST_SESSION_ID").or_else(|_| std::env::var("CLAUDE_CODE_SESSION_ID")).unwrap_or_default()
}

/// `post <to> <card> <status> <text...> [--from <name>]`: the one confirmation line on
/// success, or (exit code, message) on failure. 2 is bad arguments, 1 is a write failure.
fn run_post(args: &[String], default_from: &str) -> Result<String, (i32, String)> {
    let mut from = default_from.to_string();
    let mut rest: Vec<&String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--from" {
            match it.next() {
                Some(v) => from = v.clone(),
                None => return Err((2, format!("error: --from needs a name\n{USAGE}"))),
            }
        } else {
            rest.push(a);
        }
    }
    if rest.len() < 4 {
        return Err((2, format!("error: post needs <to> <card> <status> <text...>\n{USAGE}")));
    }
    if from.is_empty() {
        from = "unknown".into();
    }
    let text = rest[3..].iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
    match post(rest[0], rest[1], rest[2], &text, &from) {
        Ok(()) => Ok(format!("posted to {}: {} {}", rest[0], rest[1], rest[2].to_ascii_lowercase())),
        Err(e) if e == "bad <to>" => Err((2, format!("error: bad <to> \"{}\": use letters, digits, - and _ only", rest[0]))),
        Err(e) if e == "bad <status>" => {
            Err((2, format!("error: bad <status> \"{}\": use done, blocked, question or info", rest[2])))
        }
        Err(e) => Err((1, format!("error: {e}"))),
    }
}

/// Write one line, ignoring a closed or broken pipe: println! panics there, and a post that
/// already landed must still exit 0 (a nonzero exit makes the worker retry and double-post).
fn emit_to<W: Write>(w: &mut W, line: &str) {
    let _ = writeln!(w, "{line}");
}

fn emit(line: &str) {
    emit_to(&mut std::io::stdout(), line);
}

fn usage_exit() -> ! {
    crate::console();
    emit(USAGE);
    std::process::exit(2);
}

pub fn cli(args: &[String]) {
    let sub = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    match sub.as_str() {
        "post" => {
            crate::console();
            match run_post(&args[1..], &session_from_env()) {
                Ok(line) => emit(&line),
                Err((code, msg)) => {
                    emit(&msg);
                    std::process::exit(code);
                }
            }
        }
        "read" | "hook" => {
            // A hook must never fail (exit 2 blocks every prompt): bad args print nothing.
            if sub == "hook" {
                crate::console();
                for l in hook_lines(&args[1..]) {
                    emit(&l);
                }
                return;
            }
            let Some(to) = args.get(1).filter(|t| valid_name(t)) else { usage_exit() };
            crate::console();
            let lines = if args[2..].iter().any(|a| a == "--all") { read_all(to) } else { read_unread(to, true) };
            for l in lines {
                emit(&l);
            }
        }
        _ => usage_exit(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenPipe;
    impl Write for BrokenPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn emit_ignores_broken_pipe() {
        emit_to(&mut BrokenPipe, "posted to coordinator: 128 done");
    }
    use std::sync::Mutex;

    // HIVE_INBOX_DIR is process-wide: tests that use it run one at a time.
    static ENV: Mutex<()> = Mutex::new(());

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hive-inbox-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::env::set_var("HIVE_INBOX_DIR", &d);
        d
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn post_confirms_once_and_fails_clearly() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("confirm");
        let ok = run_post(&argv(&["coordinator", "128", "DONE", "all", "good"]), "").unwrap();
        assert_eq!(ok, "posted to coordinator: 128 done");
        assert_eq!(std::fs::read_to_string(d.join("coordinator.jsonl")).unwrap().lines().count(), 1);
        let (code, msg) = run_post(&argv(&["co/ord", "1", "info", "t"]), "").unwrap_err();
        assert_eq!(code, 2);
        assert!(msg.starts_with("error: bad <to>"), "{msg}");
        let (code, msg) = run_post(&argv(&["coordinator", "1", "nope", "t"]), "").unwrap_err();
        assert_eq!(code, 2);
        assert!(msg.contains("bad <status>") && msg.contains("done, blocked"), "{msg}");
        let (code, msg) = run_post(&argv(&["coordinator", "1", "info"]), "").unwrap_err();
        assert_eq!(code, 2);
        assert!(msg.starts_with("error:"), "{msg}");
        // A write that cannot happen (the inbox folder is a file) is exit 1.
        let blocked = std::env::temp_dir().join(format!("hive-inbox-test-{}-blockfile", std::process::id()));
        std::fs::write(&blocked, "x").unwrap();
        std::env::set_var("HIVE_INBOX_DIR", &blocked);
        let (code, msg) = run_post(&argv(&["coordinator", "1", "info", "t"]), "").unwrap_err();
        assert_eq!(code, 1);
        assert!(msg.starts_with("error:"), "{msg}");
        let _ = std::fs::remove_file(blocked);
        let _ = std::fs::remove_dir_all(d);
    }

    fn line(card: &str, status: &str, text: &str, from: &str) -> String {
        show(&serde_json::json!({"time":"2026-11-23 19:10:31","from":from,"to":"coordinator","card":card,"status":status,"text":text}).to_string())
    }

    #[test]
    fn labels_show_once() {
        // The seen-live case: card label and status typed into the text, from = "card 121".
        assert_eq!(
            line("card 121", "done", "card 121 DONE: commit abc", "card 121"),
            "[2026-11-23 19:10:31] card 121 DONE: commit abc"
        );
        assert_eq!(line("121", "done", "DONE: DONE: commit abc", "card 121"), "[2026-11-23 19:10:31] card 121 DONE: commit abc");
        assert_eq!(line("121", "blocked", "Card 121: blocked: need a file", "121"), "[2026-11-23 19:10:31] card 121 BLOCKED: need a file");
        // A text that only starts with the same word is not cut.
        assert_eq!(line("121", "done", "done with the tests", ""), "[2026-11-23 19:10:31] card 121 DONE: done with the tests");
        assert_eq!(line("1210", "info", "card 121 is the other one", ""), "[2026-11-23 19:10:31] card 1210 INFO: card 121 is the other one");
    }

    #[test]
    fn session_ids_do_not_replace_the_card_label() {
        for f in ["local_1a89f162-5776-4c3e-9d42-3e6456cbed19", "1a89f162-5776-4c3e-9d42-3e6456cbed19", "unknown", ""] {
            assert_eq!(line("128", "info", "hi", f), "[2026-11-23 19:10:31] card 128 INFO: hi", "from {f}");
        }
        // A real name still shows.
        assert_eq!(line("117", "question", "which file?", "w1"), "[2026-11-23 19:10:31] w1 card 117 QUESTION: which file?");
    }

    #[test]
    fn newlines_and_long_text_stay_one_line() {
        let l = line("5", "info", "first\nsecond\r\nthird\t\tfourth\n\n", "");
        assert_eq!(l, "[2026-11-23 19:10:31] card 5 INFO: first second third fourth");
        let long = line("5", "info", &format!("start\n{}", "word ".repeat(400)), "");
        assert!(!long.contains('\n') && !long.contains('\r'));
        let text = long.split_once("INFO: ").unwrap().1;
        assert_eq!(text.chars().count(), 300, "{text}");
        assert!(text.ends_with("..."));
        // Through the hook path: a multi-line, very long post is exactly one body line.
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("oneline");
        post("coordinator", "5", "info", &format!("a\nb\n{}", "é".repeat(5000)), "card 5").unwrap();
        let out = hook_lines(&argv(&["coordinator"]));
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[1].starts_with('[') && !out[1].contains('\n'));
        assert!(out[1].chars().count() <= 300 + 50, "{}", out[1].chars().count());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn post_then_read_once() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("once");
        post("coord", "117", "question", "which file?", "w1").unwrap();
        let first = read_unread("coord", true);
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("w1 card 117 QUESTION: which file?"), "{first:?}");
        assert!(read_unread("coord", true).is_empty());
        // --all shows it again without marking.
        assert_eq!(read_all("coord").len(), 1);
        post("coord", "117", "done", "finished", "w1").unwrap();
        assert_eq!(read_unread("coord", true).len(), 1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn sixteen_posts_at_once_lose_nothing() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("par");
        let hs: Vec<_> = (0..16)
            .map(|i| std::thread::spawn(move || post("coord", "117", "info", &format!("msg {i} {}", "x".repeat(500)), &format!("w{i}")).unwrap()))
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let raw = std::fs::read_to_string(d.join("coord.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 16);
        for l in raw.lines() {
            serde_json::from_str::<serde_json::Value>(l).expect("whole line");
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn caps_hold() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("caps");
        // Text over 2 KB is cut (on a character boundary).
        post("coord", "1", "info", &"é".repeat(3000), "w").unwrap();
        let raw = std::fs::read_to_string(d.join("coord.jsonl")).unwrap();
        let v: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert!(v["text"].as_str().unwrap().len() <= MAX_TEXT);
        let _ = std::fs::remove_dir_all(&d);
        let d = temp("caps2");
        // 40 short posts: 20 lines per read, the rest on the next read.
        for i in 0..40 {
            post("coord", "1", "info", &format!("m{i}"), "w").unwrap();
        }
        assert_eq!(read_unread("coord", true).len(), 20);
        assert_eq!(read_unread("coord", true).len(), 20);
        assert!(read_unread("coord", true).is_empty());
        assert_eq!(read_all("coord").len(), 20);
        let _ = std::fs::remove_dir_all(&d);
        let d = temp("caps3");
        // Big lines: 3 KB per read, nothing skipped.
        for i in 0..6 {
            post("coord", "1", "info", &format!("{i}{}", "y".repeat(1500)), "w").unwrap();
        }
        let mut seen = 0;
        loop {
            let got = read_unread("coord", true);
            if got.is_empty() {
                break;
            }
            assert!(got.iter().map(|l| l.len() + 1).sum::<usize>() <= MAX_OUT);
            seen += got.len();
        }
        assert_eq!(seen, 6);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn bad_args_are_rejected() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("bad");
        assert!(post("co/ord", "1", "info", "t", "w").is_err());
        assert!(post("", "1", "info", "t", "w").is_err());
        assert!(post("coord", "1", "nope", "t", "w").is_err());
        assert!(!d.join("coord.jsonl").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn hook_never_fails_on_bad_args() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("hook");
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(hook_lines(&a(&[])).is_empty());
        assert!(hook_lines(&a(&["bad/name"])).is_empty());
        assert!(hook_lines(&a(&["coordinator"])).is_empty());
        post("coordinator", "1", "info", "hi", "w").unwrap();
        assert_eq!(hook_lines(&a(&["coordinator", "extra", "--x"])).len(), 2);
        assert!(hook_lines(&a(&["coordinator"])).is_empty());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_recreated_file_is_read_from_the_start() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("recreate");
        for i in 0..3 {
            post("coord", "1", "info", &format!("old message number {i}"), "w").unwrap();
        }
        assert_eq!(read_unread("coord", true).len(), 3);
        // Replace the file with different, longer lines: the old cursor lands mid-line.
        let line = format!("{{\"text\":\"{}\"}}\n", "a".repeat(700));
        std::fs::write(d.join("coord.jsonl"), format!("{line}{line}{line}")).unwrap();
        let got = read_unread("coord", true);
        assert_eq!(got.len(), 3, "{got:?}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn non_json_lines_obey_the_cap() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("rawcap");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("coord.jsonl"), format!("{}
", "z".repeat(10_000))).unwrap();
        let got = read_unread("coord", true);
        assert_eq!(got.len(), 1);
        assert!(got[0].len() < MAX_OUT);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn unread_empty_when_no_file() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("none");
        assert!(read_unread("coord", true).is_empty());
        assert!(read_all("coord").is_empty());
        let _ = std::fs::remove_dir_all(d);
    }
}
