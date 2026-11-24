//! Tool-output trimmer: `Hive.exe --trim-hook`, a Claude Code PostToolUse hook.
//!
//! Reads the hook JSON on stdin. When a Bash, PowerShell or MCP tool returned more than
//! HIVE_TRIM_LIMIT characters (default 12000), it prints the documented
//! `hookSpecificOutput.updatedToolOutput` reply: the first 60 and last 40 lines plus one note
//! line, with the full text saved under `%TEMP%\hive-trim` (newest 200 files kept; HIVE_TRIM_DIR
//! overrides the folder, tests use it). Anything else prints nothing, and the hook always exits 0.
//! Read, Edit, Write and every other built-in tool are never touched.

use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;

const DEFAULT_LIMIT: usize = 12_000;
const HEAD_LINES: usize = 60;
const TAIL_LINES: usize = 40;
const KEEP_FILES: usize = 200;
/// One line is never kept longer than this (a minified blob is a single line).
const MAX_LINE: usize = 400;
/// When the text has few lines, keep this many characters at each end instead.
const HEAD_CHARS: usize = 6_000;
const TAIL_CHARS: usize = 4_000;

pub fn limit() -> usize {
    std::env::var("HIVE_TRIM_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HIVE_TRIM_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    std::env::temp_dir().join("hive-trim")
}

/// Only shell and MCP tools are ever trimmed.
fn eligible(tool: &str) -> bool {
    tool == "Bash" || tool == "PowerShell" || tool.starts_with("mcp__")
}

fn clip_end(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

fn clip_start(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = s.len() - max;
    while !s.is_char_boundary(n) {
        n += 1;
    }
    &s[n..]
}

fn clip_line(l: &str) -> String {
    if l.len() <= MAX_LINE {
        l.to_string()
    } else {
        format!("{} [line cut at {} chars]", clip_end(l, MAX_LINE), MAX_LINE)
    }
}

/// Shorten `text`; `saved` is where the full text was written. Returns the new text and the
/// number of lines (or, for a few very long lines, characters) left out.
fn shorten(text: &str, saved: &str, limit: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() > HEAD_LINES + TAIL_LINES {
        let cut = lines.len() - HEAD_LINES - TAIL_LINES;
        let mut out: Vec<String> = lines[..HEAD_LINES].iter().map(|l| clip_line(l)).collect();
        out.push(format!("[Hive trimmed {cut} lines; full output saved to {saved}]"));
        out.extend(lines[lines.len() - TAIL_LINES..].iter().map(|l| clip_line(l)));
        let joined = out.join("\n");
        // 100 long lines can still be big: then cut by characters instead.
        if joined.len() <= limit {
            return joined;
        }
    }
    // Scale to the limit so a small HIVE_TRIM_LIMIT never keeps more than it allows.
    let head = clip_end(text, HEAD_CHARS.min(limit * 3 / 5));
    let tail = clip_start(text, TAIL_CHARS.min(limit * 2 / 5));
    let cut = text.len().saturating_sub(head.len() + tail.len());
    format!("{head}\n[Hive trimmed {cut} chars; full output saved to {saved}]\n{tail}")
}

/// Write the full text, then keep only the newest KEEP_FILES files in the folder.
fn save(text: &str, id: &str) -> Option<PathBuf> {
    let d = dir();
    std::fs::create_dir_all(&d).ok()?;
    let id: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(40).collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_millis())
        .unwrap_or(0);
    let mut path = d.join(format!("{stamp}-{id}.txt"));
    let mut n = 0;
    while path.exists() {
        n += 1;
        path = d.join(format!("{stamp}-{id}-{n}.txt"));
    }
    std::fs::write(&path, text).ok()?;
    prune(&d, KEEP_FILES);
    Some(path)
}

fn prune(d: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "txt"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

/// Trim one string field when it is over the limit.
fn trim_text(text: &str, limit: usize, id: &str) -> Option<String> {
    if text.chars().count() <= limit {
        return None;
    }
    let path = save(text, id)?;
    let out = shorten(text, &path.display().to_string(), limit);
    // Never hand back something as big as the original (tiny limit, text barely over it).
    if out.len() >= text.len() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(out)
}

/// The full hook reply for this input, or None when nothing should change.
pub fn respond(input: &str, limit: usize) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    if v.get("hook_event_name").and_then(Value::as_str).is_some_and(|e| e != "PostToolUse") {
        return None;
    }
    let tool = v.get("tool_name")?.as_str()?;
    if !eligible(tool) {
        return None;
    }
    let id = v.get("tool_use_id").and_then(Value::as_str).unwrap_or("call");
    let resp = v.get("tool_response")?;
    // Claude Code already saved this output and swapped in a preview: leave it alone.
    if resp.get("persistedOutputPath").and_then(Value::as_str).is_some_and(|p| !p.is_empty()) {
        return None;
    }
    let new: Value = match resp {
        // A failed call is the error text; an MCP result may also be plain text.
        Value::String(s) => Value::String(trim_text(s, limit, id)?),
        // MCP content blocks: trim each text block, keep the rest as it is.
        Value::Array(blocks) => {
            let mut changed = false;
            let out: Vec<Value> = blocks
                .iter()
                .map(|b| {
                    let t = b.get("text").and_then(Value::as_str);
                    if let (Some("text"), Some(t)) = (b.get("type").and_then(Value::as_str), t) {
                        if let Some(n) = trim_text(t, limit, id) {
                            changed = true;
                            let mut b2 = b.clone();
                            b2["text"] = Value::String(n);
                            return b2;
                        }
                    }
                    b.clone()
                })
                .collect();
            if !changed {
                return None;
            }
            Value::Array(out)
        }
        // Bash and PowerShell: {stdout, stderr, interrupted, isImage}. Keep every other field.
        Value::Object(o) if o.get("stdout").is_some_and(Value::is_string) => {
            if o.get("isImage").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let mut o2 = o.clone();
            let mut changed = false;
            for key in ["stdout", "stderr"] {
                if let Some(s) = o.get(key).and_then(Value::as_str) {
                    if let Some(n) = trim_text(s, limit, &format!("{id}-{key}")) {
                        o2.insert(key.into(), Value::String(n));
                        changed = true;
                    }
                }
            }
            if !changed {
                return None;
            }
            Value::Object(o2)
        }
        _ => return None,
    };
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": new}})
            .to_string(),
    )
}

/// `--trim-hook`: never fails, never blocks; bad input prints nothing.
pub fn cli() {
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(64 * 1024 * 1024).read_to_end(&mut buf);
    let input = String::from_utf8_lossy(&buf);
    if let Some(out) = respond(&input, limit()) {
        println!("{out}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV: Mutex<()> = Mutex::new(());

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hive-trim-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::env::set_var("HIVE_TRIM_DIR", &d);
        d
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("line {i} {}", "x".repeat(20))).collect::<Vec<_>>().join("\n")
    }

    fn hook(tool: &str, resp: Value) -> String {
        json!({"hook_event_name": "PostToolUse", "tool_name": tool, "tool_input": {},
               "tool_response": resp, "tool_use_id": "toolu_01ABC"})
        .to_string()
    }

    fn updated(out: &str) -> Value {
        let v: Value = serde_json::from_str(out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        v["hookSpecificOutput"]["updatedToolOutput"].clone()
    }

    #[test]
    fn bash_over_limit_keeps_head_and_tail() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("bash");
        let full = lines(1000);
        let inp = hook("Bash", json!({"stdout": full, "stderr": "", "interrupted": false, "isImage": false}));
        let out = respond(&inp, 12_000).expect("trimmed");
        let u = updated(&out);
        let s = u["stdout"].as_str().unwrap();
        assert!(s.starts_with("line 1 "));
        assert!(s.contains("line 60 "));
        assert!(!s.contains("line 61 "));
        assert!(s.contains("line 961 "));
        assert!(s.ends_with(&format!("line 1000 {}", "x".repeat(20))));
        assert!(s.contains("[Hive trimmed 900 lines; full output saved to "));
        assert_eq!(u["stderr"], "");
        assert_eq!(u["interrupted"], false);
        assert_eq!(u["isImage"], false);
        // The saved file holds everything.
        let saved: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        assert_eq!(saved.len(), 1);
        assert_eq!(std::fs::read_to_string(saved[0].path()).unwrap(), full);
        assert!(s.contains(&saved[0].path().display().to_string()));
    }

    #[test]
    fn small_limit_never_grows_the_text() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("small");
        // 5000 chars in 30 lines, limit 2000: result must be shorter than the input.
        let text = (0..30).map(|i| format!("{i:02} {}", "y".repeat(163))).collect::<Vec<_>>().join("
");
        assert!(text.len() > 2000 && text.len() < 10_000);
        let inp = hook("Bash", json!({"stdout": text, "stderr": "", "interrupted": false, "isImage": false}));
        let u = updated(&respond(&inp, 2000).expect("trimmed"));
        let s = u["stdout"].as_str().unwrap();
        assert!(s.len() < text.len(), "{} !< {}", s.len(), text.len());
        assert!(!s.contains("trimmed 0 chars"));
        // 150 lines, 8000 chars, limit 3000.
        let text2 = (0..150).map(|i| format!("{i:03} {}", "z".repeat(49))).collect::<Vec<_>>().join("
");
        let inp = hook("Bash", json!({"stdout": text2, "stderr": "", "interrupted": false, "isImage": false}));
        let u = updated(&respond(&inp, 3000).expect("trimmed"));
        assert!(u["stdout"].as_str().unwrap().len() < text2.len());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn barely_over_tiny_limit_is_left_alone() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("barely");
        // Over the limit by 10 chars: the note alone would make it bigger, so no change, no file.
        let text = "a".repeat(110);
        let inp = hook("Bash", json!({"stdout": text, "stderr": "", "interrupted": false, "isImage": false}));
        assert!(respond(&inp, 100).is_none());
        assert!(!d.exists() || std::fs::read_dir(&d).unwrap().count() == 0);
    }

    #[test]
    fn powershell_works_like_bash() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        temp("ps");
        let inp = hook("PowerShell", json!({"stdout": lines(500), "stderr": "warn", "interrupted": false, "isImage": false}));
        let u = updated(&respond(&inp, 12_000).expect("trimmed"));
        assert!(u["stdout"].as_str().unwrap().contains("[Hive trimmed 400 lines"));
        assert_eq!(u["stderr"], "warn");
    }

    #[test]
    fn under_limit_prints_nothing() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("under");
        let inp = hook("Bash", json!({"stdout": lines(100), "stderr": "", "interrupted": false, "isImage": false}));
        assert!(respond(&inp, 12_000).is_none());
        assert!(!d.exists());
    }

    #[test]
    fn read_edit_write_are_never_trimmed() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("never");
        for t in ["Read", "Edit", "Write", "Grep", "Glob", "WebFetch", "Agent"] {
            let inp = hook(t, json!(lines(2000)));
            assert!(respond(&inp, 1000).is_none(), "{t}");
        }
        assert!(!d.exists());
    }

    #[test]
    fn mcp_string_and_blocks() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        temp("mcp");
        let inp = hook("mcp__reports__run_sql", json!(lines(600)));
        let u = updated(&respond(&inp, 12_000).expect("string"));
        assert!(u.as_str().unwrap().contains("[Hive trimmed 500 lines"));
        let inp = hook(
            "mcp__x__y",
            json!([{"type": "text", "text": lines(600)}, {"type": "image", "data": "abc"}]),
        );
        let u = updated(&respond(&inp, 12_000).expect("blocks"));
        assert!(u[0]["text"].as_str().unwrap().contains("[Hive trimmed 500 lines"));
        assert_eq!(u[1]["type"], "image");
        // Short blocks stay as they are.
        let inp = hook("mcp__x__y", json!([{"type": "text", "text": "short"}]));
        assert!(respond(&inp, 12_000).is_none());
    }

    #[test]
    fn few_long_lines_are_cut_by_characters() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        temp("long");
        let blob = "a".repeat(100_000);
        let inp = hook("Bash", json!({"stdout": blob, "stderr": "", "interrupted": false, "isImage": false}));
        let u = updated(&respond(&inp, 12_000).expect("trimmed"));
        let s = u["stdout"].as_str().unwrap();
        assert!(s.len() < 11_000, "{}", s.len());
        assert!(s.contains("[Hive trimmed 90000 chars; full output saved to "));
    }

    #[test]
    fn image_and_bad_input_are_left_alone() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        temp("bad");
        let inp = hook("Bash", json!({"stdout": lines(900), "stderr": "", "interrupted": false, "isImage": true}));
        assert!(respond(&inp, 100).is_none());
        assert!(respond("not json", 100).is_none());
        assert!(respond("", 100).is_none());
        assert!(respond("{}", 100).is_none());
        let other = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_response": lines(900)}).to_string();
        assert!(respond(&other, 100).is_none());
    }

    #[test]
    fn persisted_output_is_left_alone() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("persisted");
        let inp = hook("Bash", json!({"stdout": lines(1000), "stderr": "", "interrupted": false, "isImage": false,
            "persistedOutputPath": "/tmp/out.txt", "persistedOutputSize": 40000}));
        assert!(respond(&inp, 12_000).is_none());
        assert!(!d.exists() || std::fs::read_dir(&d).unwrap().count() == 0);
    }

    #[test]
    fn empty_or_missing_persisted_path_still_trims() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        for (tag, extra) in [("pmiss", json!({})), ("pnull", json!({"persistedOutputPath": null})),
                             ("pempty", json!({"persistedOutputPath": ""}))] {
            let d = temp(tag);
            let mut r = json!({"stdout": lines(1000), "stderr": "", "interrupted": false, "isImage": false});
            for (k, v) in extra.as_object().unwrap() {
                r[k] = v.clone();
            }
            assert!(respond(&hook("Bash", r), 12_000).is_some(), "{tag}");
            assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "{tag}");
        }
    }

    #[test]
    fn keeps_only_newest_200_files() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = temp("prune");
        for i in 0..205 {
            save("x", &format!("id{i}")).unwrap();
        }
        let n = std::fs::read_dir(&d).unwrap().count();
        assert_eq!(n, 200);
    }

    #[test]
    fn limit_env_and_default() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("HIVE_TRIM_LIMIT");
        assert_eq!(limit(), 12_000);
        std::env::set_var("HIVE_TRIM_LIMIT", "500");
        assert_eq!(limit(), 500);
        std::env::set_var("HIVE_TRIM_LIMIT", "junk");
        assert_eq!(limit(), 12_000);
        std::env::remove_var("HIVE_TRIM_LIMIT");
    }
}
