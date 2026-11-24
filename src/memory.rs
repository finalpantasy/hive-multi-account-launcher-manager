//! Shared memory for every Claude and Codex account: one Obsidian vault of plain
//! Markdown notes (2026-10-16: replaces claude-mem, which ran an AI on every
//! tool call). Nothing here calls a model:
//!
//! - `--memory start` is the start-of-chat hook for Claude Code and Codex. It prints
//!   the project's index of notes (one line each, capped) plus the shared index, so a
//!   chat sees what exists and opens only the notes it needs.
//! - Chats write the notes themselves, one fact per file with a `description:` line;
//!   the indexes are rebuilt here from those lines, so they never drift or overflow.
//! - `harvest` (the tray, every 30 minutes) appends free history to per-project
//!   digests: what Claude Desktop already says about each chat (its status summary),
//!   the pending work in Claude's own summaries of long chats, the last reply of each
//!   Codex thread, and new handoff files. Then it commits the vault to git once a day.
//! - `search <words>` gives a chat the 10 best matching lines instead of a grep, with
//!   the one-line descriptions of a few notes each hit links to (`[[name]]`).
//! - `links <note>` lists what a note links to and what links to it; `check [days]`
//!   reports old notes, broken links, missing paths and likely duplicates (changes nothing).
//! - `prompt` (each prompt, Claude and Codex) adds one line, at most three times in a
//!   chat, when the chat grows long: save what a future chat needs before compaction.
//!
//! Every output has a hard size cap, and the vault, each digest file and each harvest's
//! reading have limits, so nothing here can run away with tokens or disk.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::config::Config;

/// Lines of a project's index shown at the start of a chat (and kept in its INDEX.md).
const INDEX_LINES: usize = 20;
/// Lines of the shared index shown at the start of a chat.
const SHARED_LINES: usize = 15;
/// The whole start-of-chat text stays under this (about 1,000 tokens).
const START_BYTES: usize = 4000;
/// To-do list names shown at the start of a chat (the most recently changed).
const WORKING_NAMES: usize = 5;
/// Characters of a note's description in the index.
const DESC_CHARS: usize = 100;
/// History older than this is not harvested on the first run.
const BACKFILL_DAYS: u64 = 30;
const LINE_CHARS: usize = 200;
/// Notes older than this are marked in the index as worth checking before use.
const STALE_DAYS: u64 = 90;
/// Hard limits, so a fault can never fill the disk or a chat's context.
const DIGEST_FILE_CAP: u64 = 512 * 1024;
const VAULT_CAP: u64 = 300 * 1024 * 1024;
/// Transcript bytes one harvest may read; the rest waits for the next one.
const READ_BUDGET: u64 = 256 * 1024 * 1024;
const SEARCH_LINES: usize = 10;
const CHAT_LINES: usize = 2;
const SEARCH_BYTES: usize = 3000;
/// Linked notes shown under one search hit, and under all of them together.
const HIT_LINKS: usize = 3;
const SEARCH_LINKS: usize = 5;
const LINK_DESC_CHARS: usize = 70;
/// `--memory check` lists notes not updated in this many days (unless told otherwise).
const CHECK_DAYS: u64 = 60;
/// Lines per section of `--memory check`.
const CHECK_LINES: usize = 12;
/// The setup check points at `--memory check` once this many notes need a look.
const CHECK_WARN: usize = 25;
/// Transcript sizes at which a long chat is reminded, once each, to save its notes.
const REMIND_AT: [u64; 3] = [4 << 20, 15 << 20, 40 << 20];
/// Claude's own summaries of long chats are read back this far on the first run.
const SUMMARY_BACKFILL_DAYS: u64 = 7;

pub fn vault() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"));
    if let Some(d) = std::env::var_os("HIVE_MEMORY_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    home.join("Documents").join("Hive Memory")
}

/// What the harvest state keeps for a thread in a private folder, instead of its path.
const PRIVATE: &str = "private";

/// Private material never goes in or comes out (the same folders claude-mem skipped).
fn private(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.contains("private notes") || l.contains("private-notes") || l.contains("read ai") || l.contains("read-ai") || l.contains("readai")
}

/// A project's name in the vault, from a working folder:
/// - under a projects folder (`%USERPROFILE%\dev`, Games, Claude's tools, or any folder listed in
///   `HIVE_PROJECT_ROOTS`, separated by `;`), the first folder inside it, so a repo's subfolders and art folders
///   count as the repo;
/// - a Claude scratchpad in Temp: the project of the chat that made it;
/// - otherwise its git repository's folder, else the folder itself.
/// Worktrees use their repository's name. Throwaway folders (Claude's scratch folders,
/// Codex's task folders, the home folder) share "general".
pub fn project_of(cwd: &str) -> Option<String> {
    project_of_at(cwd, 0)
}

fn project_of_at(cwd: &str, depth: u8) -> Option<String> {
    if cwd.trim().is_empty() || private(cwd) {
        return None;
    }
    let p = Path::new(cwd.trim_end_matches(['\\', '/']));
    let lower = p.to_string_lossy().to_lowercase().replace('/', "\\");
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let h = home.to_lowercase();
    // A chat's scratchpad: ...\AppData\Local\Temp\claude\<chat's project folder, encoded>\...
    const TEMP: &str = "\\appdata\\local\\temp\\claude\\";
    if let Some(i) = lower.find(TEMP) {
        let enc = lower[i + TEMP.len()..].split('\\').next().unwrap_or("");
        let from = if depth == 0 { chat_cwd(enc) } else { None };
        return from.and_then(|c| project_of_at(&c, depth + 1)).or_else(|| Some("general".into()));
    }
    let general = [format!("{h}\\documents\\codex"), format!("{h}\\appdata")];
    if general.iter().any(|g| lower == *g || lower.starts_with(&format!("{g}\\"))) || (!h.is_empty() && lower == h) {
        return Some("general".into());
    }
    let mut containers = vec![format!("{h}\\dev"), format!("{h}\\games"), format!("{h}\\.claude\\tools")];
    if let Ok(extra) = std::env::var("HIVE_PROJECT_ROOTS") {
        containers.extend(extra.split(';').map(|e| e.trim().trim_end_matches(['\\', '/']).to_lowercase().replace('/', "\\")).filter(|e| !e.is_empty()));
    }
    let mut root: Option<PathBuf> = None;
    for c in &containers {
        if lower == *c {
            return Some("general".into());
        }
        if let Some(rest) = lower.strip_prefix(&format!("{c}\\")) {
            let n = rest.split('\\').next().unwrap_or("").chars().count();
            let original: String = p.to_string_lossy().chars().take(c.chars().count() + 1 + n).collect();
            root = Some(PathBuf::from(original));
            break;
        }
    }
    let root = root.unwrap_or_else(|| p.ancestors().find(|d| d.join(".git").exists()).unwrap_or(p).to_path_buf());
    let name = repo_name(&root)?;
    let name = name.strip_suffix("-wt").unwrap_or(&name).to_string();
    let parent_is_date = root.parent().and_then(|q| q.file_name()).is_some_and(|n| is_date(&n.to_string_lossy()));
    if name.starts_with("scratch-") || parent_is_date || is_date(&name) {
        return Some("general".into());
    }
    let slug: String = name.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect();
    let slug = slug.trim_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

/// A folder's name, or for a git worktree its repository's name.
fn repo_name(root: &Path) -> Option<String> {
    let own = root.file_name().map(|n| n.to_string_lossy().to_string());
    if !root.join(".git").is_file() {
        return own;
    }
    std::fs::read_to_string(root.join(".git"))
        .ok()
        .and_then(|t| t.trim().strip_prefix("gitdir:").map(|g| g.trim().to_string()))
        .and_then(|g| {
            // <repo>\.git\worktrees\<name>
            PathBuf::from(g).ancestors().find(|a| a.file_name().is_some_and(|n| n == ".git")).and_then(|a| a.parent()).and_then(|r| r.file_name()).map(|n| n.to_string_lossy().to_string())
        })
        .or(own)
}

/// The working folder of the Claude Code chats whose transcripts sit in
/// ~\.claude\projects\<enc> (how a scratchpad in Temp names its chat).
fn chat_cwd(enc: &str) -> Option<String> {
    thread_local! {
        static SEEN: std::cell::RefCell<HashMap<String, Option<String>>> = std::cell::RefCell::new(HashMap::new());
    }
    if let Some(v) = SEEN.with(|s| s.borrow().get(enc).cloned()) {
        return v;
    }
    let dir = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".claude").join("projects");
    let found = std::fs::read_dir(&dir).into_iter().flatten().flatten().find(|e| e.file_name().to_string_lossy().to_lowercase() == enc).and_then(|e| {
        std::fs::read_dir(e.path()).into_iter().flatten().flatten().filter(|f| f.path().extension().is_some_and(|x| x == "jsonl")).take(3).find_map(|f| {
            let file = std::fs::File::open(f.path()).ok()?;
            std::io::BufReader::new(file).lines().take(40).flatten().find_map(|l| {
                serde_json::from_str::<serde_json::Value>(&l).ok()?.get("cwd")?.as_str().map(String::from)
            })
        })
    });
    SEEN.with(|s| s.borrow_mut().insert(enc.to_string(), found.clone()));
    found
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && s.chars().filter(|c| c.is_ascii_digit()).count() == 8
}

/// A note's index line: (file name, description, updated) from its frontmatter.
fn note_meta(p: &Path) -> Option<(String, String, String)> {
    // Bytes, not text: 2 KB can end inside a letter, and the note must not vanish for it.
    let mut raw = Vec::new();
    std::fs::File::open(p).ok()?.take(2048).read_to_end(&mut raw).ok()?;
    let head = String::from_utf8_lossy(&raw);
    let head = head.strip_prefix('\u{feff}').unwrap_or(&head);
    let fm = head.strip_prefix("---")?.split("\n---").next()?;
    let field = |k: &str| {
        fm.lines().find_map(|l| l.trim_start().strip_prefix(k).and_then(|r| r.strip_prefix(':')).map(|v| v.trim().trim_matches('"').to_string()))
    };
    let description = field("description").filter(|d| !d.is_empty())?;
    let updated = field("updated").or_else(|| field("modified")).unwrap_or_default();
    let mtime = std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let updated = match updated.get(..10) {
        Some(d) if is_date(d) => d.to_string(),
        _ => ymd(mtime),
    };
    Some((p.file_name()?.to_string_lossy().to_string(), description, updated))
}

/// Rebuild a folder's INDEX.md from its notes (newest first, capped). Returns the lines.
fn rebuild_index(dir: &Path, cap: usize) -> Vec<String> {
    let mut notes: Vec<(String, String, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md") && p.file_name().is_some_and(|n| n != "INDEX.md"))
        .filter_map(|p| note_meta(&p))
        .collect();
    // Newest first. Pointers to the old claude-mem history stay out of the index: search
    // finds them, and every chat in every project would otherwise pay for the line.
    notes.retain(|n| !n.0.starts_with("claude-mem-archive"));
    notes.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    let total = notes.len();
    let stale_before = ymd(crate::time::now().saturating_sub(STALE_DAYS * crate::time::DAY));
    // `[[name]]` is a link in Obsidian and the file name for a chat; the date is month-day.
    let mut lines: Vec<String> = notes
        .iter()
        .take(cap)
        .map(|(f, d, u)| {
            let old = if u.as_str() < stale_before.as_str() { " (old: verify)" } else { "" };
            format!("- [[{}]] {}: {}{old}", f.trim_end_matches(".md"), u.get(5..).unwrap_or(u), clip(d, DESC_CHARS))
        })
        .collect();
    if total > cap {
        lines.push(format!("- and {} older notes (search them)", total - cap));
    }
    let text = format!("<!-- Rebuilt by Hive from each note's description. Do not edit. -->\n{}\n", lines.join("\n"));
    let path = dir.join("INDEX.md");
    // Whole or not at all: several chats can start in one project at the same moment.
    if !lines.is_empty() && std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        let tmp = dir.join(format!("INDEX.md.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, &text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    lines
}

fn clip(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::events::cut_at_word(&one, max)
}

/// The text for the start of a chat in `cwd`.
pub fn start_text(cwd: &str) -> String {
    project_of(cwd).map_or(String::new(), |p| start_for(&p))
}

/// The text for the start of a chat in this project.
fn start_for(project: &str) -> String {
    let project = project.to_string();
    let v = vault();
    if !v.exists() {
        return String::new();
    }
    let pdir = v.join("projects").join(&project);
    let lines = if pdir.exists() { rebuild_index(&pdir, INDEX_LINES) } else { Vec::new() };
    let shared = rebuild_index(&v.join("shared"), SHARED_LINES);
    let head = format!("# Memory: project \"{project}\" in {}\n", v.display());
    let mut tail = String::new();
    if !shared.is_empty() {
        tail.push_str(&format!("Shared (shared\\):\n{}\n", shared.join("\n")));
    }
    let mut working: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(v.join("working").join(&project))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let at = e.metadata().and_then(|m| m.modified()).ok()?;
            e.file_name().to_str().map(|n| (at, n.trim_end_matches(".md").to_string()))
        })
        .collect();
    if !working.is_empty() {
        working.sort_by(|a, b| b.0.cmp(&a.0));
        let more = working.len().saturating_sub(WORKING_NAMES);
        let names: Vec<&str> = working.iter().take(WORKING_NAMES).map(|w| w.1.as_str()).collect();
        let more = if more > 0 { format!(" and {more} more") } else { String::new() };
        tail.push_str(&format!("To-do lists (working\\{project}\\): {}{more}\n", names.join(", ")));
    }
    if pdir.join("digest").exists() {
        tail.push_str(&format!("Past work, by day: projects\\{project}\\digest\\ (search it; do not read it whole).\n"));
    }
    let mut out = head;
    if lines.is_empty() {
        out.push_str(&format!("No notes for this project yet. Save to projects\\{project}\\<short-name>.md.\n"));
    } else {
        // The shared rules and the pointers always fit; the project's own list gives way
        // (newest notes kept), and the rest is counted for search.
        let title = format!("Notes (projects\\{project}\\, open only what is relevant):\n");
        let room = START_BYTES.saturating_sub(out.len() + title.len() + tail.len() + 48);
        let older_line = |l: &String| l.starts_with("- and ");
        let older: usize = lines.iter().find(|l| older_line(l)).and_then(|l| l.split_whitespace().nth(2)).and_then(|n| n.parse().ok()).unwrap_or(0);
        let notes: Vec<&String> = lines.iter().filter(|l| !older_line(l)).collect();
        let mut used = 0;
        let kept: Vec<&String> = notes.iter().take_while(|l| {
            used += l.len() + 1;
            used <= room
        }).copied().collect();
        let left = older + notes.len() - kept.len();
        out.push_str(&title);
        for l in &kept {
            out.push_str(l);
            out.push('\n');
        }
        if left > 0 {
            out.push_str(&format!("- and {left} older notes (search them)\n"));
        }
    }
    out.push_str(&tail);
    if out.len() > START_BYTES {
        let mut cut = START_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n(index cut short)\n");
    }
    out
}

/// `--memory start|harvest|index`.
pub fn cli(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("start") => {
            let input = crate::events::hook_input();
            // A resumed chat already has the index in its history: printing it again only
            // pays for it twice. (A cleared or compacted chat lost it, so it gets it again.)
            if input.as_ref().and_then(|v| v.get("source")).and_then(|s| s.as_str()) == Some("resume") {
                if let Some(v) = &input {
                    record_start(v, 0);
                }
                return;
            }
            // The hook's input names the chat's folder; without it, this folder.
            let cwd = input
                .as_ref()
                .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from))
                .or_else(|| std::env::current_dir().ok().map(|d| d.to_string_lossy().to_string()))
                .unwrap_or_default();
            let text = start_text(&cwd);
            print!("{text}");
            if let Some(v) = &input {
                record_start(v, text.len());
                // The Codex CLI runs this hook; its first prompt must not print the text again.
                if let Some(id) = v.get("session_id").and_then(|s| s.as_str()).filter(|_| tool_of(v) == "codex") {
                    mark_started(id);
                }
            }
        }
        Some("harvest") => {
            let Ok(cfg) = Config::load(&crate::config_path()) else { return };
            let n = harvest(&cfg);
            println!("harvested {n} lines into {}", vault().display());
        }
        Some("index") => {
            let v = vault();
            let mut n = 0;
            for e in std::fs::read_dir(v.join("projects")).into_iter().flatten().flatten() {
                n += rebuild_index(&e.path(), INDEX_LINES).len();
            }
            n += rebuild_index(&v.join("shared"), SHARED_LINES).len();
            println!("{n} index lines");
        }
        Some("prompt") => {
            let input = crate::events::hook_input().unwrap_or_default();
            let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            print!("{}", codex_first_prompt(&input));
            let text = prompt_text(&s("session_id"), &s("transcript_path"));
            print!("{text}");
            record_prompt(&input, text.len());
        }
        Some("search") => {
            let words = args[1..].join(" ");
            let cwd = std::env::current_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
            print!("{}", search(&words, project_of(&cwd).as_deref()));
        }
        Some("links") => print!("{}", links_text(&vault(), &args[1..].join(" "))),
        Some("check") => {
            let days = args[1..].iter().find_map(|d| d.parse().ok()).unwrap_or(CHECK_DAYS);
            let all = args[1..].iter().any(|a| a == "--all");
            print!("{}", check_text(&check_at(&vault(), days, crate::time::now()), days, all));
        }
        _ => println!("usage: --memory start|prompt|search <words>|links <note>|check [days] [--all]|harvest|index"),
    }
}

// ---- Harvest -----------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct State {
    /// Chat records changed after this (ms) are read again.
    records_ms: u64,
    /// Codex rollouts: bytes already read, and the folder each one works in.
    offsets: HashMap<String, u64>,
    cwds: HashMap<String, String>,
    /// What is already in a digest (newest last, capped).
    seen: VecDeque<String>,
    /// The day the vault was last committed to git.
    committed: String,
}

fn state_path() -> PathBuf {
    vault().join(".hive").join("harvest.json")
}

/// One digest line: (project, unix secs, text, dedupe key).
type Line = (String, u64, String, String);

/// Append new history to the digests. Returns how many lines were added.
pub fn harvest(cfg: &Config) -> usize {
    let v = vault();
    if !v.exists() {
        return 0;
    }
    let _ = std::fs::create_dir_all(v.join(".hive"));
    // One harvest at a time (the tray and a command-line run).
    let lock = v.join(".hive").join("harvest.lock");
    if !take_lock(&lock) {
        return 0;
    }
    let mut st: State = std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    // A full vault means something is wrong: stop adding and say so, rather than grow.
    let size = dir_size(&v);
    if size > VAULT_CAP {
        crate::log(&format!("memory: vault is {} MB, over its {} MB limit; harvest skipped", size >> 20, VAULT_CAP >> 20));
        let _ = std::fs::remove_file(&lock);
        return 0;
    }
    let now = crate::time::now();
    let cutoff = now.saturating_sub(BACKFILL_DAYS * crate::time::DAY);
    let mut lines: Vec<Line> = Vec::new();
    let started_ms = crate::time::now_ms();
    let mut budget = READ_BUDGET;
    chat_records(cfg, &mut st, cutoff, &mut lines);
    claude_summaries(&mut st, now.saturating_sub(SUMMARY_BACKFILL_DAYS * crate::time::DAY), &mut budget, &mut lines);
    codex_turns(cfg, &mut st, cutoff, &mut budget, &mut lines);
    handoffs(cutoff, &mut lines);
    st.records_ms = started_ms;
    let seen: HashSet<String> = st.seen.iter().cloned().collect();
    lines.retain(|l| !seen.contains(&l.3));
    lines.sort_by_key(|l| l.1);
    let mut dedupe = HashSet::new();
    lines.retain(|l| dedupe.insert(l.3.clone()));
    let kept = write_digests(&v, &lines);
    let added = kept.len();
    st.seen.extend(kept);
    while st.seen.len() > 50_000 {
        st.seen.pop_front();
    }
    if let Ok(t) = serde_json::to_vec(&st) {
        let tmp = state_path().with_extension("json.tmp");
        if std::fs::write(&tmp, t).is_ok() {
            let _ = std::fs::rename(&tmp, state_path());
        }
    }
    let today = ymd(now);
    if st.committed != today && commit(&v, &today) {
        st.committed = today;
        if let Ok(t) = serde_json::to_vec(&st) {
            let _ = std::fs::write(state_path(), t);
        }
    }
    let _ = std::fs::remove_file(&lock);
    if added > 0 {
        crate::log(&format!("memory: {added} new digest lines"));
    }
    added
}

/// The harvest lock: a stale one (over 10 minutes) is cleared; `false` when another
/// harvest holds it. Two overlapping harvests therefore never both write a digest.
fn take_lock(lock: &Path) -> bool {
    let stale = std::fs::metadata(lock).and_then(|m| m.modified()).ok().is_some_and(|t| t.elapsed().map_or(false, |e| e.as_secs() > 600));
    if stale {
        let _ = std::fs::remove_file(lock);
    }
    std::fs::OpenOptions::new().write(true).create_new(true).open(lock).is_ok()
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| match e.file_type() {
        Ok(t) if t.is_dir() => dir_size(&e.path()),
        Ok(_) => e.metadata().map_or(0, |m| m.len()),
        _ => 0,
    }).sum()
}

/// The vault's daily git commit: history to undo a bad edit from any account. Local
/// only (never pushed); `.hive` (the harvest's own state) is not committed.
fn commit(v: &Path, day: &str) -> bool {
    commit_msg(v, &format!("memory {day}"))
}

fn commit_msg(v: &Path, msg: &str) -> bool {
    use std::os::windows::process::CommandExt;
    let git = |args: &[&str]| {
        std::process::Command::new("git").args(args).current_dir(v).creation_flags(0x0800_0000).output().ok().filter(|o| o.status.success())
    };
    if !v.join(".git").exists() {
        let _ = std::fs::write(v.join(".gitignore"), ".hive/\n.obsidian/workspace*.json\n*.tmp\n");
        if git(&["init", "-q"]).is_none() {
            return false;
        }
        let _ = git(&["config", "user.name", "Hive"]);
        let _ = git(&["config", "user.email", "hive@localhost"]);
    }
    if git(&["add", "-A"]).is_none() {
        return false;
    }
    // Nothing staged is fine: the day is done either way.
    let staged = git(&["diff", "--cached", "--quiet"]).is_none();
    !staged || git(&["commit", "-q", "-m", msg]).is_some()
}

/// One matching line, before ranking.
struct Hit {
    score: usize,
    /// A note with `type: dead-end` in its frontmatter: a failed approach, shown first.
    dead: bool,
    note: bool,
    mine: bool,
    mtime: u64,
    line_no: usize,
    /// The chat a digest line belongs to; `None` for notes.
    chat: Option<String>,
    text: String,
    path: PathBuf,
    links: Vec<String>,
}

/// `<account> · <chat title>` of a digest line (`- HH:MM <account> · <title> [state]: <status>`),
/// without the state tag. Without a tag the title ends at the first ": ".
fn chat_key(line: &str) -> Option<String> {
    let rest = line.trim_start_matches(['-', ' ']);
    let (clock, rest) = rest.split_once(' ')?;
    if clock.len() != 5 || clock.as_bytes()[2] != b':' {
        return None;
    }
    chat_of(rest).map(|c| c.0)
}

/// The chat of a digest entry without its clock, and whether it carried a state tag.
fn chat_of(rest: &str) -> Option<(String, bool)> {
    let (end, tagged) = match rest.find("]: ").and_then(|c| rest[..c].rfind(" [")) {
        Some(open) => (open, true),
        None => (rest.find(": ")?, false),
    };
    Some((rest[..end].trim().to_lowercase(), tagged)).filter(|k| !k.0.is_empty())
}

/// The `[state]` tag of a digest entry without its clock; empty when it has none.
fn state_of(rest: &str) -> String {
    rest.find("]: ").and_then(|c| rest[..c].rfind(" [").map(|o| rest[o + 2..c].to_string())).unwrap_or_default()
}

/// Whether two digest entries (no clock) are about one chat. Claude chats only: the
/// entry must name `<account> · <title>`. An untagged entry's title ends at its first
/// ": ", so it matches a tagged chat whose title starts with it.
fn same_chat(a: &str, b: &str) -> bool {
    let (Some((ka, ta)), Some((kb, tb))) = (chat_of(a), chat_of(b)) else { return false };
    if !ka.contains(" · ") || !kb.contains(" · ") {
        return false;
    }
    ka == kb || (!ta && kb.starts_with(&format!("{ka}: "))) || (!tb && ka.starts_with(&format!("{kb}: ")))
}

/// Best first: more matching words, then dead-end notes, then notes before digest lines, the chat's own project,
/// newest. A chat's digest lines keep only the newest 2 (progress ticks of a busy chat
/// would otherwise fill every slot).
fn rank_hits(mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.dead.cmp(&a.dead)).then(b.note.cmp(&a.note)).then(b.mine.cmp(&a.mine)).then(b.mtime.cmp(&a.mtime)).then(b.line_no.cmp(&a.line_no)));
    // A line without a state tag cuts its title at the first ": ", so "acct · AS W5" is the
    // same chat as a tagged "acct · AS W5: recipe installer".
    let keys: HashSet<String> = hits.iter().filter_map(|h| h.chat.clone()).collect();
    for h in &mut hits {
        if let Some(c) = &h.chat {
            let longer = keys.iter().filter(|k| k.len() > c.len() && k.starts_with(c.as_str()) && k[c.len()..].starts_with(": ")).max_by_key(|k| k.len());
            if let Some(k) = longer {
                h.chat = Some(k.clone());
            }
        }
    }
    let mut kept: HashMap<String, Vec<(u64, usize)>> = HashMap::new();
    for h in &hits {
        if let Some(c) = &h.chat {
            kept.entry(c.clone()).or_default().push((h.mtime, h.line_no));
        }
    }
    // The newest two (file time, then line) of each chat survive.
    let mut newest: HashMap<String, (u64, usize)> = HashMap::new();
    for (c, mut v) in kept {
        v.sort_by(|a, b| b.cmp(a));
        newest.insert(c, v.get(CHAT_LINES - 1).copied().unwrap_or((0, 0)));
    }
    hits.retain(|h| h.chat.as_ref().is_none_or(|c| (h.mtime, h.line_no) >= newest[c]));
    hits
}

/// The best matching lines across notes and digests: notes before digest lines at equal
/// match, then the chat's own project, newest first, at most 2 lines per chat. Capped at 10
/// lines and 3 KB.
pub fn search(words: &str, project: Option<&str>) -> String {
    let v = vault();
    let (out, shown) = search_at(&v, words, project);
    record_hits(&v, &shown, crate::time::now());
    out
}

/// Whether a note's frontmatter says `type: dead-end` (an approach that failed after real effort).
fn is_dead_end(text: &str) -> bool {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(fm) = t.strip_prefix("---").and_then(|r| r.split("\n---").next()) else { return false };
    fm.lines().any(|l| l.trim_start().strip_prefix("type:").is_some_and(|v| v.trim().trim_matches(['"', '\'']).eq_ignore_ascii_case("dead-end")))
}

/// What one file gave a search: its name entry (notes only) and its matching lines.
struct FileHits {
    name: Option<(String, PathBuf)>,
    hits: Vec<Hit>,
}

/// Read one file and collect its matching lines; `None` when it cannot be read.
fn search_file(v: &Path, path: &Path, mtime: u64, terms: &[String], need: usize, project: Option<&str>) -> Option<FileHits> {
    let text = std::fs::read_to_string(path).ok()?;
    let note = is_note_in(v, path);
    let dead = note && is_dead_end(&text);
    let name = if note { path.file_stem().map(|stem| (stem.to_string_lossy().to_lowercase(), path.to_path_buf())) } else { None };
    let rel = path.strip_prefix(v).map(|r| r.to_string_lossy().to_string()).unwrap_or_default();
    let mine = project.is_some_and(|p| rel.starts_with(&format!("projects\\{p}\\")) || rel.starts_with(&format!("working\\{p}\\")));
    // A note's links are the whole note's; a digest line's are its own.
    let mut note_links: Option<Vec<String>> = None;
    let mut hits = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.len() < 8 || line.starts_with("<!--") {
            continue;
        }
        let l = line.to_lowercase();
        let score = terms.iter().filter(|t| l.contains(t.as_str())).count();
        if score >= need {
            let links = if note { note_links.get_or_insert_with(|| links_in(&text)).clone() } else { links_in(line) };
            let text = format!("- {}{rel}:{}: {}", if dead { "Dead end: " } else { "" }, n + 1, clip(line.trim_start_matches(['-', ' ', '#']), 180));
            let chat = if note { None } else { chat_key(line) };
            hits.push(Hit { score, dead, note, mine, mtime, line_no: n + 1, chat, text, path: path.to_path_buf(), links });
        }
    }
    Some(FileHits { name, hits })
}

/// `search` against the vault at `v`: the text, and the notes (vault-relative paths) it showed.
fn search_at(v: &Path, words: &str, project: Option<&str>) -> (String, Vec<String>) {
    // Short words ("up", "to") match nearly everything: used only when nothing longer is given.
    let all: Vec<String> = words.split_whitespace().map(|w| w.to_lowercase()).filter(|w| w.chars().count() >= 2).collect();
    let long: Vec<String> = all.iter().filter(|w| w.chars().count() >= 3).cloned().collect();
    let terms = if long.is_empty() { all } else { long };
    if terms.is_empty() {
        return ("Usage: --memory search <words>\n".into(), Vec::new());
    }
    let mut files = Vec::new();
    md_files(v, &mut files);
    let need = terms.len().div_ceil(2);
    // Note names (lower case) to their files, for the links under each hit.
    let mut names: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut hits: Vec<Hit> = Vec::new();
    // Reading 1,600 files is most of the time: workers take files in turn and the results are
    // put back in walk order, so the ranking ties break exactly as a single pass would.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let mut done: Vec<(usize, Option<FileHits>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((path, mtime)) = files.get(i) else { break };
                        mine.push((i, search_file(v, path, *mtime, &terms, need, project)));
                    }
                    mine
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    done.sort_by_key(|(i, _)| *i);
    for (_, f) in done {
        let Some(f) = f else { continue };
        if let Some((stem, path)) = f.name {
            names.entry(stem).or_default().push(path);
        }
        hits.extend(f.hits);
    }
    let total = hits.len();
    let mut hits = rank_hits(hits);
    hits.truncate(SEARCH_LINES);
    // A linked note already in the results, or already shown once, is not shown again.
    let mut seen: HashSet<PathBuf> = hits.iter().map(|h| h.path.clone()).collect();
    let mut linked = 0;
    let mut out = String::new();
    let mut shown: Vec<String> = Vec::new();
    let mut show = |p: &Path| {
        let rel = rel_of(v, p);
        if !shown.contains(&rel) {
            shown.push(rel);
        }
    };
    for Hit { text: line, path, links, note, .. } in hits {
        if out.len() + line.len() > SEARCH_BYTES {
            break;
        }
        if note {
            show(&path);
        }
        out.push_str(&line);
        out.push('\n');
        let mut here = 0;
        for name in &links {
            if here == HIT_LINKS || linked == SEARCH_LINKS {
                break;
            }
            let Some(target) = resolve(&names, name, path.parent(), v) else { continue };
            if !seen.insert(target.clone()) {
                continue;
            }
            let Some((file, d, _)) = note_meta(target) else { continue };
            let l = format!("  > [[{}]] {}\n", file.trim_end_matches(".md"), clip(&d, LINK_DESC_CHARS));
            if out.len() + l.len() > SEARCH_BYTES {
                break;
            }
            out.push_str(&l);
            show(target);
            here += 1;
            linked += 1;
        }
    }
    if out.is_empty() {
        return (format!("No matches for \"{words}\" in {}.\n", v.display()), Vec::new());
    }
    if total > SEARCH_LINES {
        out.push_str(&format!("({total} matching lines; add a word to narrow it)\n"));
    }
    (out, shown)
}

// ---- Which notes searches return ----------------------------------------------

/// A note no search returned for this long is listed by `--memory check`.
const UNUSED_DAYS: u64 = 30;

/// `<vault>\.hive\search-hits.json`: one entry per note a search returned, never the query.
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct SearchHits {
    /// Unix seconds of the first recorded search (when the tracking began).
    since: u64,
    /// Unix seconds entries for deleted notes were last dropped (once a day).
    pruned: u64,
    /// Vault-relative note path to its hits.
    notes: HashMap<String, NoteHits>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Copy)]
#[serde(default)]
struct NoteHits {
    count: u64,
    /// Unix seconds of the last search that returned it.
    last: u64,
}

/// Where the hits live: `HIVE_MEMORY_HITS=off` records nothing (timing runs against the
/// real vault), any other value is a file to use instead of the vault's own.
fn hits_path(v: &Path) -> Option<PathBuf> {
    match std::env::var("HIVE_MEMORY_HITS") {
        Ok(x) if x.eq_ignore_ascii_case("off") => None,
        Ok(x) if !x.is_empty() => Some(PathBuf::from(x)),
        _ => Some(v.join(".hive").join("search-hits.json")),
    }
}

fn read_hits(file: &Path) -> Option<SearchHits> {
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

/// Count each note in `shown` as returned at `now`. A short lock file keeps two searches at
/// once from losing each other's update; when it cannot be had in about 3 seconds the
/// search simply is not counted. The final rename is retried a few times (a reader or
/// scanner can hold the file for a moment on Windows). Entries for notes that no longer exist are dropped once a day.
fn record_hits(v: &Path, shown: &[String], now: u64) {
    let Some(file) = hits_path(v) else { return };
    if shown.is_empty() {
        return;
    }
    let Some(dir) = file.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let lock = file.with_extension("lock");
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
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let mut h = read_hits(&file).unwrap_or_default();
    if h.since == 0 {
        h.since = now;
    }
    // Stat-ing every recorded note costs more than the rest of the write: once a day.
    if now.saturating_sub(h.pruned) >= crate::time::DAY {
        h.notes.retain(|rel, _| v.join(rel).is_file());
        h.pruned = now;
    }
    for rel in shown {
        let e = h.notes.entry(rel.clone()).or_default();
        e.count += 1;
        e.last = now;
    }
    if let Ok(t) = serde_json::to_vec(&h) {
        let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, t).is_ok() {
            let mut renamed = false;
            for i in 0..5 {
                if std::fs::rename(&tmp, &file).is_ok() {
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
}

fn md_files(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        match e.file_type() {
            Ok(t) if t.is_dir() && !name.starts_with('.') => md_files(&p, out),
            Ok(t) if t.is_file() && name.ends_with(".md") && name != "INDEX.md" => {
                let mtime = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                out.push((p, mtime));
            }
            _ => {}
        }
    }
}

// ---- Links between notes, and the check --------------------------------------

/// The notes a text links to with `[[name]]` (also `[[name|shown]]`, `[[name#part]]`,
/// `[[folder/name]]`), lower case, in order, once each. Links inside code are examples,
/// not links.
fn links_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        for (i, part) in line.split('`').enumerate() {
            if i % 2 == 1 {
                continue;
            }
            let mut rest = part;
            while let Some(at) = rest.find("[[") {
                rest = &rest[at + 2..];
                let Some(end) = rest.find("]]") else { break };
                let inner = &rest[..end];
                rest = &rest[end + 2..];
                let name = inner.split(['|', '#']).next().unwrap_or("").trim();
                let name = name.rsplit(['/', '\\']).next().unwrap_or("").trim_end_matches(".md").trim().to_lowercase();
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// The note a link names: one in the linking note's folder first, then shared, then any.
fn resolve<'a>(names: &'a HashMap<String, Vec<PathBuf>>, name: &str, from: Option<&Path>, v: &Path) -> Option<&'a PathBuf> {
    let all = names.get(name)?;
    let shared = v.join("shared");
    all.iter()
        .find(|p| p.parent() == from)
        .or_else(|| all.iter().find(|p| p.parent() == Some(shared.as_path())))
        .or_else(|| all.first())
}

fn rel_of(v: &Path, p: &Path) -> String {
    p.strip_prefix(v).map(|r| r.to_string_lossy().to_string()).unwrap_or_else(|_| p.display().to_string())
}

/// Note names (lower case) to their files.
fn name_map(all: &[Note]) -> HashMap<String, Vec<PathBuf>> {
    let mut names: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for n in all {
        names.entry(n.name.to_lowercase()).or_default().push(n.path.clone());
    }
    names
}

/// `--memory links <note>`: what a note links to and what links to it, one line each.
fn links_text(v: &Path, name: &str) -> String {
    let key = name.trim().trim_start_matches("[[").trim_end_matches("]]").trim_end_matches(".md").to_lowercase();
    if key.is_empty() {
        return "Usage: --memory links <note name>\n".into();
    }
    let all = notes_at(v);
    let names = name_map(&all);
    let by_path: HashMap<&Path, &Note> = all.iter().map(|n| (n.path.as_path(), n)).collect();
    let Some(found) = names.get(&key) else { return format!("No note named \"{}\" in {}.\n", name.trim(), v.display()) };
    let line = |p: &Path| match by_path.get(p) {
        Some(n) => format!("- [[{}]] {}\n", n.name, clip(&n.description, LINK_DESC_CHARS)),
        None => format!("- {}\n", rel_of(v, p)),
    };
    let section = |out: &mut String, title: &str, lines: Vec<String>| {
        out.push_str(&format!("{title} ({}):\n", lines.len()));
        for l in lines.iter().take(CHECK_LINES) {
            out.push_str(l);
        }
        if lines.len() > CHECK_LINES {
            out.push_str(&format!("- and {} more\n", lines.len() - CHECK_LINES));
        }
    };
    let mut out = String::new();
    for p in found.iter().take(3) {
        let Some(n) = by_path.get(p.as_path()) else { continue };
        out.push_str(&format!("[[{}]] {}: {}\n", n.name, rel_of(v, p), clip(&n.description, DESC_CHARS)));
        let to: Vec<String> = links_in(&n.text)
            .iter()
            .map(|l| match resolve(&names, l, p.parent(), v) {
                Some(t) => line(t),
                None => format!("- [[{l}]] (no such note)\n"),
            })
            .collect();
        section(&mut out, "Links to", to);
        let from: Vec<String> = all
            .iter()
            .filter(|o| o.path != *p && links_in(&o.text).iter().any(|l| resolve(&names, l, o.path.parent(), v) == Some(p)))
            .map(|o| line(&o.path))
            .collect();
        section(&mut out, "Linked from", from);
    }
    out
}

/// Words that say a path in a line is gone on purpose (history, not a live pointer), or is
/// only an example, a suggestion or a before/after of a move.
const GONE_WORDS: [&str; 31] = [
    "removed", "deleted", "retired", "moved", "renamed", "no longer", "gone", "uninstalled", "was at", "used to", "does not exist", "doesn't exist", "not exist", "never existed",
    "missing", "dropped", "wiped", "rewrote", "e.g.", "for example", "for instance", "such as", "example", "examples", "consider", "->", "\u{2192}", "=>", "cleaned up", "purged", "abandoned",
];

/// File extensions a spaced file name may end in (`Open sample tool.bat`).
const FILE_EXTS: [&str; 40] = [
    "bat", "cmd", "ps1", "py", "md", "txt", "json", "jsonl", "html", "htm", "exe", "dll", "cfg", "toml", "yaml", "yml", "sh", "rs", "js", "mjs", "ts", "png", "jpg", "jpeg", "gif", "mp4", "pptx", "xlsx",
    "docx", "pdf", "csv", "log", "ini", "dat", "bin", "gd", "tscn", "glb", "blend", "zip",
];

/// Little words that end a path: the prose after it, never part of a file name.
const STOP_WORDS: [&str; 35] = [
    "a", "an", "and", "or", "with", "is", "are", "was", "were", "has", "have", "had", "in", "on", "at", "to", "for", "from", "by", "its", "it", "as", "but", "not", "so", "then", "that", "this",
    "can", "be", "copies", "died", "during", "of", "the",
];

fn has_ext(w: &str) -> bool {
    let w = w.trim_end_matches(['.', ',', ';', ':', ')', '"', '\'']);
    w.rsplit_once('.').is_some_and(|(stem, x)| !stem.is_empty() && FILE_EXTS.contains(&x.to_ascii_lowercase().as_str()))
}

/// Lowercase only: `The` in `Call Of The Wild` is a name, `the` after a path is prose.
fn is_stop(w: &str) -> bool {
    STOP_WORDS.contains(&w.trim_matches(|c: char| !c.is_alphanumeric()))
}

/// Small words inside a capitalized name (`World of Warcraft`).
fn is_connector(w: &str) -> bool {
    ["of", "the", "a", "an", "de", "la", "vs"].contains(&w)
}

/// `s` with a leading `~` or `%USERPROFILE%` made real.
fn expand(s: &str) -> String {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    if let Some(r) = s.strip_prefix('~') {
        format!("{home}{r}")
    } else if s.get(..13).is_some_and(|p| p.eq_ignore_ascii_case("%userprofile%")) && s.len() > 13 {
        format!("{home}{}", &s[13..])
    } else {
        s.to_string()
    }
}

/// Where a path written in running prose really ends. A space ends it, unless the longer
/// text is a path that exists, or the words run on to a file name (`.../Open sample tool.bat`),
/// or each next word is capitalized or holds a separator (`D:\My Drive\Project Notes`).
/// So `A:\projects\same names` stops at `same`, and `F:\X\tool.dll is X` at `tool.dll`.
fn real_end(c: &str) -> &str {
    let words: Vec<&str> = c.split(' ').collect();
    let mut ends: Vec<usize> = Vec::new();
    let mut at = 0;
    for w in &words {
        at += w.len();
        ends.push(at);
        at += 1;
    }
    let first = &c[..ends[0]];
    if words.len() == 1 || first.ends_with(['\\', '/', ':']) || has_ext(first) {
        return first;
    }
    // 1. The longest run of words that exists on disk.
    for k in (1..words.len().min(9)).rev() {
        if Path::new(&expand(&c[..ends[k]])).exists() {
            return &c[..ends[k]];
        }
    }
    // 2. Words that run on to a file name, with no prose between.
    for k in 1..words.len().min(7) {
        if is_stop(words[k]) {
            break;
        }
        if has_ext(words[k]) {
            return &c[..ends[k]];
        }
    }
    // 3. Capitalized words or words with a separator.
    let named = |w: &str| w.contains(['\\', '/']) || w.chars().next().is_some_and(char::is_uppercase);
    let mut end = ends[0];
    let mut k = 1;
    while k < words.len() {
        let w = words[k];
        if is_stop(w) && !is_connector(w) {
            break;
        }
        if is_connector(w) {
            // Only inside a name (`Call of the Wild`): a capitalized word must follow the small ones.
            let mut j = k + 1;
            while words.get(j).is_some_and(|n| is_connector(n)) {
                j += 1;
            }
            if words.get(j).is_some_and(|n| named(n)) {
                end = ends[j];
                k = j + 1;
                continue;
            }
            break;
        }
        if !named(w) {
            break;
        }
        end = ends[k];
        k += 1;
    }
    &c[..end]
}

/// A lock, pid or temp file: it exists only while something runs, so missing is normal.
/// An exact `.lock` (or `.pid`, `.tmp`...) extension or name, never a name that merely ends in "lock" (Warlock, block).
fn transient(s: &str) -> bool {
    let name = s.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
    ["lock", "pid", "tmp", "temp", "swp", "pause", "gamelock"].iter().any(|x| name == *x || name.ends_with(&format!(".{x}")))
}

/// A clear placeholder: `sN` (a letter and a literal N), `NAME`, a lone `X`, a `s1..s6` range.
/// Real folders like FILE, PATH, CARD, DIR, ID, USER, IN or ON are not placeholders.
fn placeholder_segment(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or(seg);
    let b = stem.as_bytes();
    seg.contains("..") || stem == "X" || stem == "NAME" || b.len() == 2 && b[0].is_ascii_lowercase() && b[1] == b'N'
}

/// Whether a path holds a placeholder (`slots\sN\x`, `...\NAME`) or is a transient file.
fn skip_path(s: &str) -> bool {
    transient(s) || s.split(['\\', '/']).skip(1).any(placeholder_segment)
}

/// `D:/F:` or `F:/D:`: two drive letters run together in prose, not a path.
fn double_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 5 && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\') && b[3].is_ascii_alphabetic() && b[4] == b':'
}

/// Whether `phrase` is in `low` as whole words.
fn has_word(low: &str, phrase: &str) -> bool {
    let first = phrase.chars().next().is_some_and(char::is_alphanumeric);
    let last = phrase.chars().last().is_some_and(char::is_alphanumeric);
    low.match_indices(phrase).any(|(i, m)| {
        let before = low[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = low[i + m.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        (!first || before) && (!last || after)
    })
}

/// Whether a clause says its path is gone or only an example. The clause's own paths and
/// backticked text are left out, so `A:\proj\examples\x` never trips it.
fn says_gone(clause: &str) -> bool {
    let mut words: Vec<&str> = Vec::new();
    for (i, part) in clause.split('`').enumerate() {
        if i % 2 == 0 {
            words.extend(part.split_whitespace().filter(|w| !(w.contains(":\\") || w.contains(":/") || w.starts_with("~\\") || w.starts_with("~/") || w.starts_with('%'))));
        }
    }
    let low = words.join(" ").to_lowercase();
    GONE_WORDS.iter().any(|w| has_word(&low, w))
}

/// A line cut into sentences at "; " and ". ", and with `commas` into clauses at ", " too (not inside backticks, not after e.g. or i.e.).
fn clauses(line: &str, commas: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut from, mut ticks) = (0, false);
    let b = line.as_bytes();
    for (i, c) in line.char_indices() {
        if c == '`' {
            ticks = !ticks;
        }
        if !ticks && (matches!(c, ';' | '.') || commas && c == ',') && b.get(i + 1) == Some(&b' ') {
            let head = &line[from..i];
            if c == '.' && (head.ends_with("e.g") || head.ends_with("i.e")) {
                continue;
            }
            out.push(&line[from..i]);
            from = i + 2;
        }
    }
    out.push(&line[from.min(line.len())..]);
    out
}

/// Whether `line` is a prose sentence whose next line carries on in lowercase (a hard-wrapped sentence).
fn wraps_onto(line: &str, next: &str) -> bool {
    let (l, n) = (line.trim(), next.trim());
    let kv = l.find(": ").is_some_and(|i| i < 30) || l.ends_with(':');
    !l.is_empty()
        && !l.ends_with(['.', '!', '?', ':', ';'])
        && !l.starts_with(['-', '*', '#', '|', '>', '`'])
        && !l.starts_with(|c: char| c.is_ascii_digit())
        && !kv
        && n.starts_with(|c: char| c.is_lowercase())
        && (l.ends_with(',') || l.rsplit(' ').next().is_some_and(is_stop))
}

/// Absolute file paths a note names (`C:\...`, `~\...`, `%USERPROFILE%\...`), in order.
/// A path may hold spaces, so outside backticks one runs to the end of its clause and
/// `path_missing` tries it word by word.
fn paths_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (n, line) in lines.iter().copied().enumerate() {
        let sentences = clauses(line, false);
        let last_sentence = sentences.len() - 1;
        for (si, sentence) in sentences.iter().copied().enumerate() {
            let subs = clauses(sentence, true);
            let mut said: Vec<String> = subs.iter().map(|c| c.to_string()).collect();
            // A wrapped sentence's next line says what happened to this line's last clause.
            if si == last_sentence {
                if let Some(next) = lines.get(n + 1).filter(|next| wraps_onto(line, next)) {
                    let l = said.len() - 1;
                    said[l].push(' ');
                    said[l].push_str(clauses(next, true)[0]);
                }
            }
            // A clause that says "removed" about its own path covers only that path; one with no
            // path of its own ("..., requirements there (removed with the folder") covers the sentence.
            let gone: Vec<bool> = said.iter().map(|c| says_gone(c)).collect();
            let found: Vec<Vec<String>> = subs
                .iter()
                .map(|c| {
                    let mut v = Vec::new();
                    paths_in_clause(c, &mut v);
                    v
                })
                .collect();
            let general = gone.iter().zip(&found).any(|(g, f)| *g && f.is_empty());
            for (i, f) in found.into_iter().enumerate() {
                if gone[i] || general {
                    continue;
                }
                for p in f {
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

fn paths_in_clause(line: &str, out: &mut Vec<String>) {
    {
        let mut spans: Vec<&str> = Vec::new();
        for (i, part) in line.split('`').enumerate() {
            if i % 2 == 1 {
                // A program's arguments (` --flag`) are not part of the path.
                spans.push(real_end(part.split(" -").next().unwrap_or(part)));
                continue;
            }
            let mut rest = part;
            while let Some(at) = path_start(rest) {
                let tail = &rest[at..];
                let mut clause = tail;
                for stop in [", ", "; ", ". "] {
                    if let Some(i) = clause.find(stop) {
                        clause = &clause[..i];
                    }
                }
                let end = clause.find(|c: char| "\"'()<>|[]{}".contains(c)).unwrap_or(clause.len());
                // `tool-<name>` or `x[0]`: a pattern, not one real file.
                let placeholder = clause[end..].starts_with(['<', '[', '{']) && !clause[..end].ends_with(char::is_whitespace);
                let cand = if placeholder { "" } else { real_end(&clause[..end]) };
                spans.push(cand);
                rest = &tail[cand.len().max(1)..];
            }
        }
        for s in spans {
            // A trailing * stays: `tool-*` is a pattern, not a path.
            let s = s.trim().trim_matches('"');
            if double_drive(s) {
                continue;
            }
            let s = s.trim_end_matches(['.', ',', ';', ':', ')', '"']);
            if looks_like_path(s) && !skip_path(s) && !out.iter().any(|o| o == s) {
                out.push(s.to_string());
            }
        }
    }
}

/// Where the next absolute path starts in `s`: at the start or after a space or bracket.
fn path_start(s: &str) -> Option<usize> {
    let mut prev = ' ';
    for (i, c) in s.char_indices() {
        if (prev.is_whitespace() || "(\"'[<=,*".contains(prev)) && looks_like_path(s[i..].split_whitespace().next().unwrap_or("")) {
            return Some(i);
        }
        prev = c;
    }
    None
}

fn looks_like_path(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let home = s.starts_with("~\\") || s.starts_with("~/") || s.to_lowercase().starts_with("%userprofile%\\");
    let body = s.get(if home && s.starts_with('%') { 13 } else { 0 }..).unwrap_or("");
    // Placeholders and patterns (<name>, *, ...) are not one real file.
    (drive || home) && !body.contains(['<', '>', '*', '?', '|', '{', '}', '$', '%']) && !s.contains("...") && !s.contains('\u{2026}')
}

/// Whether a path a note names is missing. A path in backticks may be followed by a
/// program's arguments, so any leading part of it that exists counts as there. A drive
/// that isn't connected says nothing, so its paths are never missing.
fn path_missing(s: &str) -> bool {
    let full = expand(s);
    if full.get(..3).is_none_or(|d| !Path::new(d).exists()) {
        return false;
    }
    let mut cut = full.len();
    loop {
        if Path::new(full[..cut].trim_end_matches(['\\', '/', '.', ',', ';', ':', ')', '*', '"', '\''])).exists() {
            return false;
        }
        match full[..cut].rfind(' ') {
            Some(at) => cut = at,
            None => return true,
        }
    }
}

/// Where a missing `X:` path lives under another drive letter (`F:\dev\x` to `A:\dev\x`), if it does.
fn moved_to(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 4 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return None;
    }
    let from = b[0].to_ascii_uppercase();
    // The connected drives, found once per run.
    static DRIVES: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let drives = DRIVES.get_or_init(|| "ABCDEFGHIJKLMNOPQRSTUVWXYZ".bytes().filter(|d| Path::new(&format!("{}:\\", *d as char)).exists()).collect());
    // Whether a top folder exists on a drive, asked once per run (slow drives are not asked again).
    static TOPS: std::sync::Mutex<Option<HashMap<String, bool>>> = std::sync::Mutex::new(None);
    let top = s[2..].trim_start_matches(['\\', '/']).split(['\\', '/']).next().unwrap_or("").to_string();
    drives.iter().filter(|d| **d != from).find_map(|d| {
        let key = format!("{}:{}", *d as char, top.to_lowercase());
        let known = TOPS.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&key).copied()));
        let there = known.unwrap_or_else(|| {
            let t = Path::new(&format!("{}:\\{top}", *d as char)).exists();
            if let Ok(mut g) = TOPS.lock() {
                g.get_or_insert_with(HashMap::new).insert(key, t);
            }
            t
        });
        if !there {
            return None;
        }
        let cand = format!("{}{}", *d as char, &s[1..]);
        // The whole path must be there: `path_missing` also accepts a leading part of it.
        Path::new(expand(&cand).trim_end_matches(['\\', '/'])).exists().then_some(cand)
    })
}

/// Edit distance, for names that differ by a letter or two.
fn edits(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(here + 1);
            prev = here;
        }
    }
    row[b.len()]
}

/// Names that are probably the same note twice: equal once case and punctuation are
/// gone, or a letter or two apart (two for long names).
fn near_names(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
    let (a, b) = (norm(a), norm(b));
    let short = a.len().min(b.len());
    if a == b {
        return true;
    }
    if short < 6 || a.len().abs_diff(b.len()) > 2 {
        return false;
    }
    edits(&a, &b) <= if short >= 12 { 2 } else { 1 }
}

/// What `--memory check` found. Report only: nothing is changed.
#[derive(Default, Debug)]
struct Check {
    notes: usize,
    /// (note, updated), oldest first.
    old: Vec<(String, String)>,
    /// (note, link names that match no file in the vault).
    broken: Vec<(String, Vec<String>)>,
    /// (note, paths it names that no longer exist).
    missing: Vec<(String, Vec<String>)>,
    /// One line per pair.
    dupes: Vec<(String, String, &'static str)>,
    /// Date searches began to be counted; `None` before the first one.
    tracking_since: Option<String>,
    /// Whether `UNUSED_DAYS` of searches exist, so `unused` means something.
    tracked_enough: bool,
    /// (note, last returned or "never") for notes over `UNUSED_DAYS` old that no search
    /// returned in that time, oldest first. Information only: not part of `count`.
    unused: Vec<(String, String)>,
}

impl Check {
    /// Notes that need a look, each counted once.
    fn count(&self) -> usize {
        let mut s: HashSet<&str> = HashSet::new();
        s.extend(self.old.iter().map(|o| o.0.as_str()));
        s.extend(self.broken.iter().map(|o| o.0.as_str()));
        s.extend(self.missing.iter().map(|o| o.0.as_str()));
        for d in &self.dupes {
            s.insert(&d.0);
            s.insert(&d.1);
        }
        s.len()
    }
}

/// Check every note in the vault at `v`: not updated in `days`, links to notes that
/// don't exist, paths that no longer exist, and likely duplicates.
fn check_at(v: &Path, days: u64, now: u64) -> Check {
    let all = notes_at(v);
    let before = ymd(now.saturating_sub(days * crate::time::DAY));
    // A link may name any Markdown file in the vault (a to-do list, a digest), not only notes.
    let mut files = Vec::new();
    md_files(v, &mut files);
    let mut known: HashSet<String> = files.iter().filter_map(|(p, _)| p.file_stem()).map(|s| s.to_string_lossy().to_lowercase()).collect();
    known.insert("index".into());
    let mut c = Check { notes: all.len(), ..Default::default() };
    for n in &all {
        let rel = rel_of(v, &n.path);
        if n.updated.as_str() < before.as_str() {
            c.old.push((rel.clone(), n.updated.clone()));
        }
        // Links to pictures and other files are not note links.
        let asset = |l: &str| l.rsplit_once('.').is_some_and(|(_, x)| ["png", "jpg", "jpeg", "gif", "svg", "webp", "pdf", "mp4", "canvas"].contains(&x));
        let broken: Vec<String> = links_in(&n.text).into_iter().filter(|l| !known.contains(l) && !asset(l)).collect();
        if !broken.is_empty() {
            c.broken.push((rel.clone(), broken));
        }
        // The original text, for the paths' real spelling.
        let text = std::fs::read_to_string(&n.path).unwrap_or_default();
        let missing: Vec<String> = paths_in(after_frontmatter(&text))
            .into_iter()
            .filter(|p| path_missing(p))
            .map(|p| match moved_to(&p) {
                Some(to) => format!("{p} (moved to {to}?)"),
                None => p,
            })
            .collect();
        if !missing.is_empty() {
            c.missing.push((rel, missing));
        }
    }
    c.old.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    if let Some(h) = hits_path(v).and_then(|f| read_hits(&f)).filter(|h| h.since > 0) {
        c.tracking_since = Some(ymd(h.since));
        c.tracked_enough = now.saturating_sub(h.since) >= UNUSED_DAYS * crate::time::DAY;
        if c.tracked_enough {
            let cutoff = now.saturating_sub(UNUSED_DAYS * crate::time::DAY);
            let aged = ymd(cutoff);
            for n in all.iter().filter(|n| n.updated.as_str() < aged.as_str()) {
                let rel = rel_of(v, &n.path);
                match h.notes.get(&rel) {
                    Some(e) if e.last >= cutoff => {}
                    Some(e) => c.unused.push((rel, ymd(e.last))),
                    None => c.unused.push((rel, "never".into())),
                }
            }
            c.unused.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        }
    }
    // Duplicates: the same description anywhere, or near-same names in one folder (or
    // one of them in shared, which every project sees).
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            let same_desc = !a.description.is_empty() && a.description.trim().eq_ignore_ascii_case(b.description.trim());
            let together = a.folder == b.folder || a.folder == "shared" || b.folder == "shared";
            let why = if same_desc {
                "same description"
            } else if together && near_names(&a.name, &b.name) {
                "near-same name"
            } else {
                continue;
            };
            c.dupes.push((rel_of(v, &a.path), rel_of(v, &b.path), why));
        }
    }
    c
}

/// The "never returned" part of the check: the notes, or when it can start to say.
fn unused_text(c: &Check) -> String {
    let title = format!("Notes no search returned in {UNUSED_DAYS} days");
    let Some(since) = &c.tracking_since else {
        return format!("{title}: tracking starts with the next search.\n");
    };
    if !c.tracked_enough {
        return format!("{title}: tracking since {since}, not enough data yet.\n");
    }
    if c.unused.is_empty() {
        return format!("{title}: none (tracking since {since}).\n");
    }
    let mut out = format!("{title} ({}, tracking since {since}):\n", c.unused.len());
    for (n, last) in c.unused.iter().take(CHECK_LINES) {
        out.push_str(&format!("- {n}: last returned {last}\n"));
    }
    if c.unused.len() > CHECK_LINES {
        out.push_str(&format!("- and {} more\n", c.unused.len() - CHECK_LINES));
    }
    out
}

fn check_text(c: &Check, days: u64, all: bool) -> String {
    if c.count() == 0 {
        return format!(
            "Memory check: all {} notes look fine (none older than {days} days, no broken links or missing paths, no likely duplicates).\n{}",
            c.notes,
            unused_text(c)
        );
    }
    let mut out = format!("Memory check: {} of {} notes need a look. Report only, nothing was changed.\n", c.count(), c.notes);
    let mut section = |title: String, lines: Vec<String>| {
        if lines.is_empty() {
            return;
        }
        out.push_str(&format!("{title} ({}):\n", lines.len()));
        let cap = if all { usize::MAX } else { CHECK_LINES };
        for l in lines.iter().take(cap) {
            out.push_str(&format!("- {l}\n"));
        }
        if lines.len() > cap {
            out.push_str(&format!("- and {} more\n", lines.len() - cap));
        }
    };
    let more = |v: &[String]| if v.len() > 1 { format!(" (+{} more)", v.len() - 1) } else { String::new() };
    section(format!("Not updated in {days} days"), c.old.iter().map(|(n, u)| format!("{n}: {u}")).collect());
    section(
        "Links to notes that don't exist".into(),
        c.broken.iter().map(|(n, l)| format!("{n}: {}", l.iter().take(3).map(|x| format!("[[{x}]]")).collect::<Vec<_>>().join(", ") + &if l.len() > 3 { format!(" (+{} more)", l.len() - 3) } else { String::new() })).collect(),
    );
    section("Paths that no longer exist".into(), c.missing.iter().map(|(n, p)| if all { format!("{n}: {}", p.join(" | ")) } else { format!("{n}: {}{}", clip(&p[0], 70), more(p)) }).collect());
    section("Possible duplicates".into(), c.dupes.iter().map(|(a, b, why)| format!("{a} and {b} ({why})")).collect());
    out.push_str(&unused_text(c));
    out
}

/// For the setup check: a line when many notes need a look.
pub fn check_problem() -> Option<String> {
    let v = vault();
    if !v.exists() {
        return None;
    }
    let n = check_at(&v, CHECK_DAYS, crate::time::now()).count();
    (n >= CHECK_WARN).then(|| {
        format!("{n} memory notes need a look (old, broken links, missing paths or possible duplicates). Run Hive.exe --memory check for the list; it changes nothing.")
    })
}

/// "Open memory in Obsidian": in Obsidian when it already has the vault folder as a
/// vault, else in Explorer. Hive never makes the folder a vault itself (that would add
/// a .obsidian folder). Returns a note for the user when Obsidian is installed but doesn't
/// know the folder yet.
pub fn open_in_obsidian() -> Option<String> {
    let v = vault();
    let config = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join("obsidian").join("obsidian.json");
    let known = std::fs::read_to_string(config).ok().map(|t| obsidian_knows(&t, &v));
    match known {
        Some(true) => {
            crate::win::shell_open(&format!("obsidian://open?path={}", url_encode(&v.to_string_lossy())), None);
            None
        }
        Some(false) => {
            crate::win::shell_open(&v.to_string_lossy(), None);
            Some(format!(
                "Obsidian doesn't have the memory folder as a vault yet, so it opened in Explorer.\n\nTo use Obsidian: in Obsidian choose Open folder as vault and pick\n{}\n\nAfter that, this item opens it in Obsidian.",
                v.display()
            ))
        }
        None => {
            crate::win::shell_open(&v.to_string_lossy(), None);
            None
        }
    }
}

/// Whether Obsidian's own list of vaults (obsidian.json) has the folder `v`.
fn obsidian_knows(config: &str, v: &Path) -> bool {
    let norm = |s: &str| s.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    let want = norm(&v.to_string_lossy());
    let Ok(j) = serde_json::from_str::<serde_json::Value>(config.trim_start_matches('\u{feff}')) else { return false };
    j.get("vaults").and_then(|x| x.as_object()).is_some_and(|m| m.values().any(|e| e.get("path").and_then(|p| p.as_str()).is_some_and(|p| norm(p) == want)))
}

fn url_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The reminder level a chat of `size` bytes has reached, if it has not been told yet.
fn remind_level(size: u64, told: usize) -> Option<usize> {
    let reached = REMIND_AT.iter().filter(|&&at| size >= at).count();
    (reached > told).then_some(reached)
}

/// One line for a long chat, at most three times in its life: save notes now, while
/// the details are still in view (compaction will shorten them). Nothing otherwise.
pub fn prompt_text(session: &str, transcript: &str) -> String {
    if session.is_empty() || transcript.is_empty() {
        return String::new();
    }
    let Ok(size) = std::fs::metadata(transcript).map(|m| m.len()) else { return String::new() };
    let path = vault().join(".hive").join("reminded.json");
    let mut told: HashMap<String, (usize, u64)> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let before = told.get(session).map_or(0, |t| t.0);
    let Some(level) = remind_level(size, before) else { return String::new() };
    let now = crate::time::now();
    told.insert(session.to_string(), (level, now));
    // Forget chats not seen for two weeks.
    told.retain(|_, (_, at)| now.saturating_sub(*at) < 14 * crate::time::DAY);
    if let Ok(t) = serde_json::to_vec(&told) {
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, t).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    "Memory: this chat is getting long. If it settled anything a future chat needs (a decision and its why, a preference, a gotcha), save it now as a note in the memory vault, one fact per file. If nothing new, carry on.\n".into()
}

/// One digest file held in memory while a harvest folds its lines in.
struct Digest {
    /// Every line of the file, without newlines.
    lines: Vec<String>,
    dirty: bool,
    /// Dedupe keys of the lines folded in, kept only if the file is written.
    keys: Vec<String>,
}

impl Digest {
    fn size(&self) -> u64 {
        self.lines.iter().map(|l| l.len() as u64 + 1).sum()
    }
}

/// Fold one entry (`HH:MM text`) into the day section that starts at `from`. Per Claude chat
/// and day the section keeps the chat's first line, every line whose `[state]` changes,
/// and one latest line: when the last line of the chat is a plain progress tick (same state
/// as the line before it, so neither first nor a state change) the new line replaces it.
fn fold_entry(lines: &mut Vec<String>, from: usize, entry: String) {
    let text = |l: &str| -> String { l.strip_prefix("- ").and_then(|r| r.split_once(' ')).map(|x| x.1.to_string()).unwrap_or_default() };
    let new_text = text(&entry);
    let mine: Vec<usize> = (from..lines.len()).filter(|&i| lines[i].starts_with("- ") && same_chat(&text(&lines[i]), &new_text)).collect();
    if let [.., prev, last] = mine[..] {
        if state_of(&text(&lines[prev])) == state_of(&text(&lines[last])) {
            lines.remove(last);
        }
    }
    lines.push(entry);
}

/// Write the new lines into `projects\<p>\digest\<yyyy-mm>.md`, each file read once and
/// written once (temp file, then rename, so a reader never sees half a file). Returns the
/// dedupe keys of the lines that reached disk. A digest that reaches its cap takes one
/// last line saying so, and nothing after.
fn write_digests(v: &Path, items: &[Line]) -> Vec<String> {
    let mut files: HashMap<PathBuf, Digest> = HashMap::new();
    let mut order = Vec::new();
    let mut done = Vec::new();
    for (project, at, text, key) in items {
        let st = crate::time::local(*at);
        let dir = v.join("projects").join(project).join("digest");
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let file = dir.join(format!("{:04}-{:02}.md", st.wYear, st.wMonth));
        let day = format!("## {:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay);
        if !files.contains_key(&file) {
            let existing = std::fs::read_to_string(&file).unwrap_or_default();
            files.insert(file.clone(), Digest { lines: existing.lines().map(String::from).collect(), dirty: false, keys: Vec::new() });
            order.push(file.clone());
        }
        let d = files.get_mut(&file).unwrap();
        if d.size() >= DIGEST_FILE_CAP {
            done.push(key.clone()); // full for this month: counted as handled, never retried
            continue;
        }
        if d.lines.is_empty() {
            d.lines.push("<!-- Written by Hive from what Claude and Codex already record. No AI involved. -->".into());
            d.lines.push(format!("# {project}: {:04}-{:02}", st.wYear, st.wMonth));
        }
        let from = match d.lines.iter().rposition(|l| l.starts_with("## ")) {
            Some(i) if d.lines[i] == day => i,
            _ => {
                d.lines.push(String::new());
                d.lines.push(day);
                d.lines.len() - 1
            }
        };
        fold_entry(&mut d.lines, from, format!("- {:02}:{:02} {}", st.wHour, st.wMinute, clip(text, LINE_CHARS)));
        if d.size() >= DIGEST_FILE_CAP {
            d.lines.push("- (this month's digest is full; later lines are not kept)".into());
        }
        d.dirty = true;
        d.keys.push(key.clone());
    }
    for file in order {
        let d = &files[&file];
        if !d.dirty {
            continue;
        }
        let mut text = d.lines.join("\n");
        text.push('\n');
        let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &file).is_ok() {
            done.extend(d.keys.iter().cloned());
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    done
}

/// What Claude Desktop says about each chat after a turn (its status summary).
fn chat_records(cfg: &Config, st: &mut State, cutoff: u64, out: &mut Vec<Line>) {
    let since_ms = if st.records_ms == 0 { cutoff * 1000 } else { st.records_ms };
    for a in &cfg.accounts {
        let root = crate::claude::data_dir(cfg, a).join("claude-code-sessions");
        let files = std::fs::read_dir(&root).into_iter().flatten().flatten()
            .flat_map(|acct| std::fs::read_dir(acct.path()).into_iter().flatten().flatten())
            .flat_map(|org| std::fs::read_dir(org.path()).into_iter().flatten().flatten());
        for e in files {
            let p = e.path();
            if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("local_")) {
                continue;
            }
            let changed = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
            if changed <= since_ms {
                continue;
            }
            let Ok(text) = std::fs::read(&p) else { continue };
            let Ok(r) = serde_json::from_slice::<serde_json::Value>(&text) else { continue };
            let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let Some(sum) = r.get("postTurnSummary") else { continue };
            let detail = s(sum, "status_detail");
            if detail.is_empty() {
                continue;
            }
            let Some(project) = project_of(&s(&r, "cwd")) else { continue };
            let at = r.get("lastActivityAt").and_then(|x| x.as_f64()).map_or(changed / 1000, |ms| (ms / 1000.0) as u64);
            let id = s(&r, "sessionId");
            let mark = match sum.get("summarizes_uuid").and_then(|x| x.as_str()) {
                Some(u) => u.to_string(),
                None => format!("{:x}", hash(&detail)),
            };
            let title = s(&r, "title");
            // A chat about private material stays out, whatever its folder.
            if private(&title) || private(&detail) {
                continue;
            }
            let pr = r.get("prs").and_then(|x| x.as_array()).and_then(|v| v.iter().filter_map(|p| p.get("url").and_then(|u| u.as_str())).last()).map(|u| format!(" ([PR]({u}))")).unwrap_or_default();
            let status = s(sum, "status_category");
            let tag = if status.is_empty() || status == "completed" { String::new() } else { format!(" [{}]", status.replace('_', " ")) };
            out.push((project, at, format!("{} · {}{tag}: {detail}{pr}", a.name, clip(&title, 60)), format!("r:{id}:{mark}")));
        }
    }
}

/// The last reply of each Codex thread, once per thread per harvest.
fn codex_turns(cfg: &Config, st: &mut State, cutoff: u64, budget: &mut u64, out: &mut Vec<Line>) {
    if !cfg.codex.enabled {
        return;
    }
    let homes = std::iter::once(("Codex".to_string(), crate::codex::home(cfg))).chain(cfg.codex.accounts.iter().map(|a| (a.name.clone(), crate::codex::account_home(cfg, &a.name))));
    for (name, home) in homes {
        let mut files = Vec::new();
        for sub in ["sessions", "archived_sessions"] {
            jsonl_files(&home.join(sub), &mut files);
        }
        for (p, len, mtime) in files {
            let key = p.to_string_lossy().to_string();
            let from = st.offsets.get(&key).copied();
            // Old threads are not read on the first run: start them at their end.
            if from.is_none() && mtime < cutoff {
                st.offsets.insert(key, len);
                continue;
            }
            let from = from.unwrap_or(0);
            let from = if len < from { 0 } else { from };
            if len == from || *budget == 0 {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(&p) else { continue };
            if f.seek(SeekFrom::Start(from)).is_err() {
                continue;
            }
            let mut r = std::io::BufReader::with_capacity(1 << 20, f);
            let mut pos = from;
            let mut buf = Vec::new();
            let mut last: Option<(u64, String)> = None;
            loop {
                buf.clear();
                let Ok(n) = r.read_until(b'\n', &mut buf) else { break };
                if n == 0 || buf.last() != Some(&b'\n') {
                    break; // a line still being written: read it next time
                }
                pos += n as u64;
                *budget = budget.saturating_sub(n as u64);
                if *budget == 0 {
                    break; // the rest waits for the next harvest
                }
                let has = |k: &[u8]| buf.windows(k.len()).any(|w| w == k);
                if !has(b"\"turn_context\"") && !has(b"\"session_meta\"") && !has(b"\"task_complete\"") {
                    continue;
                }
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) else { continue };
                let payload = &v["payload"];
                if let Some(c) = payload.get("cwd").and_then(|c| c.as_str()) {
                    // A private folder is remembered only as private: its path is not kept.
                    st.cwds.insert(key.clone(), if private(c) { PRIVATE.to_string() } else { c.to_string() });
                }
                if st.cwds.get(&key).is_some_and(|c| c == PRIVATE) {
                    continue; // nothing from a private thread is read
                }
                if payload.get("type").and_then(|t| t.as_str()) == Some("task_complete") {
                    let msg = payload.get("last_agent_message").and_then(|m| m.as_str()).unwrap_or("");
                    if msg.trim().is_empty() || crate::events::background_reply(msg) || private(msg) {
                        continue;
                    }
                    let at = v.get("timestamp").and_then(|t| t.as_str()).and_then(crate::stats::utc_secs).unwrap_or(mtime);
                    last = Some((at, crate::events::card_text(msg, LINE_CHARS)));
                }
            }
            st.offsets.insert(key.clone(), pos);
            let Some((at, text)) = last else { continue };
            if at < cutoff || text.is_empty() {
                continue;
            }
            let Some(project) = st.cwds.get(&key).filter(|c| c.as_str() != PRIVATE).and_then(|c| project_of(c)) else { continue };
            let thread = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            out.push((project, at, format!("{name}: {text}"), format!("c:{thread}:{at}")));
        }
    }
    // Forget threads whose files are gone.
    st.offsets.retain(|k, _| Path::new(k).exists());
    st.cwds.retain(|k, _| st.offsets.contains_key(k));
}

/// The pending work in Claude's own summaries of long chats (written when a chat is
/// compacted, so already paid for): its "Pending Tasks" and "Current Work" sections.
fn claude_summaries(st: &mut State, cutoff: u64, budget: &mut u64, out: &mut Vec<Line>) {
    let root = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".claude").join("projects");
    let mut files = Vec::new();
    for dir in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        // Chats only: a helper agent's file (under subagents) repeats its parent's work.
        for e in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(m) = e.metadata() {
                    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                    files.push((p, m.len(), mtime));
                }
            }
        }
    }
    for (p, len, mtime) in files {
        let key = p.to_string_lossy().to_string();
        let from = st.offsets.get(&key).copied();
        if from.is_none() && mtime < cutoff {
            st.offsets.insert(key, len);
            continue;
        }
        let from = from.unwrap_or(0);
        let from = if len < from { 0 } else { from };
        if len == from || *budget == 0 {
            continue;
        }
        let Ok(mut f) = std::fs::File::open(&p) else { continue };
        if f.seek(SeekFrom::Start(from)).is_err() {
            continue;
        }
        let mut r = std::io::BufReader::with_capacity(1 << 20, f);
        let mut pos = from;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let Ok(n) = r.read_until(b'\n', &mut buf) else { break };
            if n == 0 || buf.last() != Some(&b'\n') {
                break;
            }
            pos += n as u64;
            *budget = budget.saturating_sub(n as u64);
            if *budget == 0 {
                break;
            }
            if !buf.windows(16).any(|w| w == b"isCompactSummary") {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) else { continue };
            if v.get("isCompactSummary").and_then(|b| b.as_bool()) != Some(true) {
                continue;
            }
            let cwd = v.get("cwd").and_then(|c| c.as_str()).unwrap_or("");
            let Some(project) = project_of(cwd) else { continue };
            let content = &v["message"]["content"];
            let text = match content {
                serde_json::Value::String(t) => t.clone(),
                serde_json::Value::Array(a) => a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"),
                _ => continue,
            };
            let Some(line) = pending_of(&text) else { continue };
            if private(&line) {
                continue;
            }
            let at = v.get("timestamp").and_then(|t| t.as_str()).and_then(crate::stats::utc_secs).unwrap_or(mtime);
            let id = v.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
            out.push((project, at, format!("chat summary: {line}"), format!("s:{id}")));
        }
        st.offsets.insert(key, pos);
    }
}

/// "pending: <...> | now: <...>" from a Claude chat summary's numbered sections.
fn pending_of(summary: &str) -> Option<String> {
    let section = |name: &str| -> Option<String> {
        let start = summary.find(name)? + name.len();
        let rest = &summary[start..];
        let rest = rest.trim_start_matches([':', ' ', '\r', '\n', '*']);
        // Up to the next numbered heading ("\n9. ") or the end.
        let end = rest.match_indices('\n').find(|(i, _)| {
            let after = &rest[i + 1..];
            let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
            digits > 0 && after[digits..].starts_with(". ")
        });
        let body = end.map_or(rest, |(i, _)| &rest[..i]);
        let t = clip(body, 120);
        (!t.is_empty() && !t.eq_ignore_ascii_case("none") && !t.eq_ignore_ascii_case("none.")).then_some(t)
    };
    let pending = section("Pending Tasks");
    let now = section("Current Work");
    match (pending, now) {
        (None, None) => None,
        (p, n) => Some([p.map(|p| format!("pending: {p}")), n.map(|n| format!("now: {n}"))].into_iter().flatten().collect::<Vec<_>>().join(" | ")),
    }
}

fn jsonl_files(dir: &Path, out: &mut Vec<(PathBuf, u64, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => jsonl_files(&p, out),
            Ok(_) if p.extension().is_some_and(|x| x == "jsonl") => {
                if let Ok(m) = e.metadata() {
                    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                    out.push((p, m.len(), mtime));
                }
            }
            _ => {}
        }
    }
}

/// New handoff notes (the handoff skill writes them to %USERPROFILE%\handoffs\<project>\).
fn handoffs(cutoff: u64, out: &mut Vec<Line>) {
    let root = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("handoffs");
    for proj in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let folder = proj.file_name().to_string_lossy().to_string();
        if private(&folder) {
            continue;
        }
        for e in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
            let p = e.path();
            if !p.extension().is_some_and(|x| x == "md") {
                continue;
            }
            let mtime = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            if mtime < cutoff {
                continue;
            }
            if private(&p.to_string_lossy()) {
                continue;
            }
            let Some(project) = project_of(&folder).or_else(|| Some("general".into())) else { continue };
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let url = format!("file:///{}", p.to_string_lossy().replace('\\', "/").replace(' ', "%20"));
            out.push((project, mtime, format!("handoff written: [{name}]({url})"), format!("h:{}", p.to_string_lossy())));
        }
    }
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// The day key the hook records use (yyyy-mm-dd, local time).
pub fn day_key(secs: u64) -> String {
    ymd(secs)
}

fn ymd(secs: u64) -> String {
    let t = crate::time::local(secs);
    format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay)
}

// ---- health: what the memory costs and whether it runs ---------------------------

/// What the hooks did, kept in `.hive\hooks.json` (one small file, rewritten at a chat's
/// start and at most every 10 minutes from the prompt hook).
#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Hooks {
    /// Unix seconds of the last start-of-chat run, by tool ("claude", "codex").
    pub last_start: HashMap<String, u64>,
    /// Unix seconds the prompt hook last ran.
    pub last_prompt: u64,
    /// Long-chat reminders printed.
    pub reminders: u64,
    /// By day (yyyy-mm-dd): chat starts and the bytes printed into them, by tool.
    pub days: std::collections::BTreeMap<String, HashMap<String, (u64, u64)>>,
}

fn hooks_path() -> PathBuf {
    vault().join(".hive").join("hooks.json")
}

pub fn read_hooks() -> Hooks {
    std::fs::read(hooks_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_hooks(h: &Hooks) {
    let path = hooks_path();
    if path.parent().is_some_and(|d| d.exists()) {
        if let Ok(t) = serde_json::to_vec(h) {
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, t).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Which tool ran the hook: Codex's transcripts live under its `sessions` folder, and only
/// Codex's prompt hook input has a `turn_id`.
fn tool_of(input: &serde_json::Value) -> &'static str {
    let t = input.get("transcript_path").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase().replace('/', "\\");
    let gpt = input.get("model").and_then(|v| v.as_str()).is_some_and(|m| m.starts_with("gpt"));
    if (t.contains("\\sessions\\") && t.contains("codex")) || input.get("turn_id").is_some() || gpt { "codex" } else { "claude" }
}

/// Codex Desktop does not run SessionStart hooks, so a new Codex thread gets the start
/// text on its first prompt instead. `started.json` keeps the threads that already had it
/// (from either hook); a thread whose transcript is older than `NEW_THREAD` is an old one
/// resumed, which already had its chance and is skipped, like a resumed Claude chat.
const NEW_THREAD: u64 = 30 * 60;

fn started_path() -> PathBuf {
    vault().join(".hive").join("started.json")
}

/// Mark a Codex thread as given the start text. False when it already was.
fn mark_started(session: &str) -> bool {
    let path = started_path();
    let mut seen: HashMap<String, u64> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if seen.contains_key(session) {
        return false;
    }
    let now = crate::time::now();
    seen.insert(session.to_string(), now);
    seen.retain(|_, at| now.saturating_sub(*at) < 14 * crate::time::DAY);
    if path.parent().is_some_and(|d| d.exists()) {
        if let Ok(t) = serde_json::to_vec(&seen) {
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, t).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    true
}

/// A Codex prompt that opens a new thread: the start text for its folder, else nothing.
fn codex_first_prompt(input: &serde_json::Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let (session, transcript, cwd) = (s("session_id"), s("transcript_path"), s("cwd"));
    if tool_of(input) != "codex" || session.is_empty() || cwd.is_empty() {
        return String::new();
    }
    let born = std::fs::metadata(transcript).and_then(|m| m.created()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    if born.is_some_and(|b| crate::time::now().saturating_sub(b.as_secs()) > NEW_THREAD) {
        return String::new();
    }
    if !mark_started(session) {
        return String::new();
    }
    let text = start_text(cwd);
    record_start(input, text.len());
    text
}

fn record_start(input: &serde_json::Value, bytes: usize) {
    if input.get("session_id").is_none() && input.get("hook_event_name").is_none() {
        return; // run by hand, not by a hook
    }
    let now = crate::time::now();
    let mut h = read_hooks();
    let tool = tool_of(input).to_string();
    h.last_start.insert(tool.clone(), now);
    let day = h.days.entry(ymd(now)).or_default().entry(tool).or_default();
    day.0 += 1;
    day.1 += bytes as u64;
    while h.days.len() > 30 {
        let first = h.days.keys().next().cloned().unwrap_or_default();
        h.days.remove(&first);
    }
    write_hooks(&h);
}

fn record_prompt(input: &serde_json::Value, printed: usize) {
    if input.get("session_id").is_none() && input.get("hook_event_name").is_none() {
        return; // run by hand, not by a hook
    }
    let now = crate::time::now();
    let mut h = read_hooks();
    if printed == 0 && now.saturating_sub(h.last_prompt) < 600 {
        return;
    }
    h.last_prompt = now;
    if printed > 0 {
        h.reminders += 1;
    }
    write_hooks(&h);
}

/// One project in the health view.
#[derive(Clone, Debug, Default)]
pub struct ProjectHealth {
    pub name: String,
    pub notes: usize,
    pub stale: usize,
    /// Unix seconds of the newest note change.
    pub newest: u64,
    /// Bytes a chat starting in this project is given.
    pub start_bytes: usize,
    pub digest_bytes: u64,
}

/// Everything the dashboard's Memory view shows.
#[derive(Clone, Debug, Default)]
pub struct Health {
    pub vault_bytes: u64,
    pub cap_bytes: u64,
    pub git_bytes: u64,
    pub notes: usize,
    pub notes_week: usize,
    pub stale: usize,
    pub shared: usize,
    pub projects: Vec<ProjectHealth>,
    /// Last 30 days, oldest first: (yyyy-mm-dd, digest lines added, notes changed).
    pub days: Vec<(String, u32, u32)>,
    /// Unix seconds: the last harvest and the last git commit (0: never).
    pub harvest_at: u64,
    pub commit_at: u64,
    pub hooks: Hooks,
    /// Things worth a look, worst first.
    pub problems: Vec<String>,
    /// Every note, newest first, for the note browser.
    pub list: Vec<Note>,
}

/// One note in the dashboard's note browser.
#[derive(Clone, Debug, Default)]
pub struct Note {
    pub path: PathBuf,
    /// The project's folder name, or "shared".
    pub folder: String,
    /// File name without ".md".
    pub name: String,
    /// Its `description:` line; empty when it has none (no index shows it then).
    pub description: String,
    /// yyyy-mm-dd.
    pub updated: String,
    pub stale: bool,
    /// Unix seconds of the last change to the file (orders notes of the same day).
    pub changed: u64,
    /// The whole note in lower case, for search.
    pub text: String,
}

/// Every note in the vault's project folders and shared, newest first (digests, to-do
/// lists and the archive are not notes).
pub fn notes() -> Vec<Note> {
    notes_at(&vault())
}

fn notes_at(v: &Path) -> Vec<Note> {
    let stale_before = ymd(crate::time::now().saturating_sub(STALE_DAYS * crate::time::DAY));
    let mut out = Vec::new();
    let mut folder = |dir: &Path, name: &str| {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if !p.is_file() || p.extension().map_or(true, |x| x != "md") || p.file_name().is_some_and(|n| n == "INDEX.md") {
                continue;
            }
            let Ok(raw) = std::fs::read(&p) else { continue };
            let (description, updated) = note_meta(&p).map_or_else(|| (String::new(), ymd(mtime_secs(&p))), |m| (m.1, m.2));
            out.push(Note {
                folder: name.to_string(),
                name: p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                stale: updated.as_str() < stale_before.as_str(),
                changed: mtime_secs(&p),
                // A quoted YAML line keeps its escapes (\"skills\"): show plain quotes.
                description: description.replace("\\\"", "\""),
                updated,
                text: String::from_utf8_lossy(&raw).to_lowercase(),
                path: p,
            });
        }
    };
    for e in std::fs::read_dir(v.join("projects")).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            folder(&e.path(), &e.file_name().to_string_lossy());
        }
    }
    folder(&v.join("shared"), "shared");
    out.sort_by(|a, b| b.updated.cmp(&a.updated).then(b.changed.cmp(&a.changed)).then(a.name.cmp(&b.name)));
    out
}

/// Whether `p` is a note the dashboard may change: a .md file directly in a project
/// folder or in shared, never an index, digest, to-do list or anything outside.
fn is_note(p: &Path) -> bool {
    is_note_in(&vault(), p) && p.is_file()
}

/// Whether `p` is where a note lives in the vault at `v` (it need not exist).
fn is_note_in(v: &Path, p: &Path) -> bool {
    let parent = p.parent().unwrap_or(p);
    let in_folder = parent == v.join("shared") || parent.parent() == Some(v.join("projects").as_path());
    in_folder && p.extension().is_some_and(|x| x == "md") && p.file_name().is_some_and(|n| n != "INDEX.md")
}

/// Text after the frontmatter (a note's body, for reading).
fn after_frontmatter(text: &str) -> &str {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = t.strip_prefix("---") else { return t };
    match rest.find("\n---") {
        Some(end) => {
            let tail = &rest[end + 4..];
            tail.find('\n').map_or("", |n| &tail[n + 1..])
        }
        None => t,
    }
}

/// A note's text for reading in the dashboard: no frontmatter, at most 256 KB.
pub fn note_body(p: &Path) -> String {
    let raw = std::fs::read(p).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    let body = after_frontmatter(&text).trim_matches(['\r', '\n']);
    crate::events::cut_at_word(body, 256 * 1024)
}

/// The note with its `updated:` line set to `day` (added when missing), keeping the
/// line endings and a byte order mark. None: the frontmatter never closes.
fn with_updated(text: &str, day: &str) -> Option<String> {
    let (bom, t) = match text.strip_prefix('\u{feff}') {
        Some(t) => ("\u{feff}", t),
        None => ("", text),
    };
    let crlf = t.contains("\r\n");
    let Some(rest) = t.strip_prefix("---") else {
        let nl = if crlf { "\r\n" } else { "\n" };
        return Some(format!("{bom}---{nl}updated: {day}{nl}---{nl}{t}"));
    };
    let end = rest.find("\n---")?;
    let (fm, after) = (&rest[..end], &rest[end..]);
    let mut found = false;
    let mut lines: Vec<String> = fm
        .split('\n')
        .map(|l| {
            if !found && l.trim_start().starts_with("updated:") {
                found = true;
                format!("updated: {day}{}", if l.ends_with('\r') { "\r" } else { "" })
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("updated: {day}{}", if crlf { "\r" } else { "" }));
    }
    Some(format!("{bom}---{}{after}", lines.join("\n")))
}

/// The folder's index again, after a note in it changed (the start of a chat rebuilds
/// it anyway; this keeps INDEX.md right for Obsidian in the meantime).
fn reindex(dir: &Path) {
    let cap = if dir.file_name().is_some_and(|n| n == "shared") { SHARED_LINES } else { INDEX_LINES };
    if rebuild_index(dir, cap).is_empty() {
        let _ = std::fs::remove_file(dir.join("INDEX.md"));
    }
}

/// Mark a note as checked today (the dashboard's Keep): its `updated:` line becomes
/// today, so it is no longer over 90 days old.
pub fn mark_checked(p: &Path) -> Result<(), String> {
    if !is_note(p) {
        return Err("That file isn't a note in the vault.".into());
    }
    let text = std::fs::read_to_string(p).map_err(|e| format!("Couldn't read the note: {e}"))?;
    let new = with_updated(&text, &ymd(crate::time::now())).ok_or("The note's frontmatter never closes (no second ---).")?;
    let tmp = p.with_extension("md.tmp");
    std::fs::write(&tmp, new).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Couldn't save the note: {e}")
    })?;
    if let Some(dir) = p.parent() {
        reindex(dir);
    }
    Ok(())
}

/// Delete a note (the dashboard's Delete). The vault is committed first and again
/// after, so git history keeps the note. Returns its path in the vault.
pub fn delete_note(p: &Path) -> Result<String, String> {
    if !is_note(p) {
        return Err("That file isn't a note in the vault.".into());
    }
    let v = vault();
    let rel = p.strip_prefix(&v).map(|r| r.to_string_lossy().to_string()).unwrap_or_default();
    if !commit_msg(&v, "memory: before a delete in the dashboard") {
        return Err("Git couldn't save the vault first, so nothing was deleted.".into());
    }
    std::fs::remove_file(p).map_err(|e| format!("Couldn't delete the note: {e}"))?;
    if let Some(dir) = p.parent() {
        reindex(dir);
    }
    commit_msg(&v, &format!("memory: deleted {} in the dashboard", rel.replace('\\', "/")));
    Ok(rel)
}

fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

/// Read the vault for the health view (a worker thread: about 100 ms).
pub fn health() -> Health {
    let v = vault();
    let now = crate::time::now();
    let mut h = Health { cap_bytes: VAULT_CAP, hooks: read_hooks(), ..Default::default() };
    if !v.exists() {
        h.problems.push(format!("The vault folder {} is missing.", v.display()));
        return h;
    }
    h.git_bytes = dir_size(&v.join(".git"));
    h.vault_bytes = dir_size(&v).saturating_sub(h.git_bytes);
    h.harvest_at = mtime_secs(&state_path());
    h.commit_at = mtime_secs(&v.join(".git").join("logs").join("HEAD"));
    let week_ago = now.saturating_sub(7 * crate::time::DAY);
    let stale_before = ymd(now.saturating_sub(STALE_DAYS * crate::time::DAY));
    let first_day = ymd(now.saturating_sub(29 * crate::time::DAY));
    let mut changed: HashMap<String, u32> = HashMap::new();
    let mut digest: HashMap<String, u32> = HashMap::new();
    let (mut blind, mut dupes, mut week) = (0usize, 0usize, 0usize);
    let mut cut_short = Vec::new();
    let mut big_digest = Vec::new();
    let mut tally = |dir: &Path, name: &str| -> ProjectHealth {
        let mut ph = ProjectHealth { name: name.to_string(), ..Default::default() };
        let mut descs = HashSet::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().map_or(true, |x| x != "md") || p.file_name().is_some_and(|n| n == "INDEX.md") {
                continue;
            }
            let at = mtime_secs(&p);
            match note_meta(&p) {
                Some((_, d, u)) => {
                    ph.notes += 1;
                    if u.as_str() < stale_before.as_str() {
                        ph.stale += 1;
                    }
                    if !descs.insert(d.to_lowercase()) {
                        dupes += 1;
                    }
                    ph.newest = ph.newest.max(at);
                    if at >= week_ago {
                        week += 1;
                    }
                    let day = ymd(at);
                    if day >= first_day {
                        *changed.entry(day).or_default() += 1;
                    }
                }
                None => blind += 1,
            }
        }
        // Digest lines by day, from this month's and last month's files.
        for e in std::fs::read_dir(dir.join("digest")).into_iter().flatten().flatten() {
            let size = e.metadata().map_or(0, |m| m.len());
            ph.digest_bytes += size;
            if size > DIGEST_FILE_CAP * 8 / 10 {
                big_digest.push(format!("{name}\\digest\\{}", e.file_name().to_string_lossy()));
            }
            let stem = e.file_name().to_string_lossy().trim_end_matches(".md").to_string();
            if stem.as_str() < &first_day[..7] {
                continue;
            }
            let mut day = String::new();
            for l in std::fs::read_to_string(e.path()).unwrap_or_default().lines() {
                if let Some(d) = l.strip_prefix("## ") {
                    day = d.trim().to_string();
                } else if l.starts_with("- ") && day >= first_day {
                    *digest.entry(day.clone()).or_default() += 1;
                }
            }
        }
        ph
    };
    for e in std::fs::read_dir(v.join("projects")).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            let name = e.file_name().to_string_lossy().to_string();
            let mut ph = tally(&e.path(), &name);
            if ph.notes > 0 || ph.digest_bytes > 0 {
                ph.start_bytes = start_for(&name).len();
                if ph.start_bytes > START_BYTES {
                    cut_short.push(name.clone());
                }
                h.projects.push(ph);
            }
        }
    }
    let shared = tally(&v.join("shared"), "shared");
    h.shared = shared.notes;
    h.notes_week = week;
    h.notes = h.projects.iter().map(|p| p.notes).sum::<usize>() + shared.notes;
    h.stale = h.projects.iter().map(|p| p.stale).sum::<usize>() + shared.stale;
    h.projects.sort_by(|a, b| b.notes.cmp(&a.notes).then(a.name.cmp(&b.name)));
    for i in (0..30).rev() {
        let d = ymd(now.saturating_sub(i * crate::time::DAY));
        let (a, c) = (digest.get(&d).copied().unwrap_or(0), changed.get(&d).copied().unwrap_or(0));
        h.days.push((d, a, c));
    }
    // What needs a look, worst first.
    let ago = |t: u64| now.saturating_sub(t);
    if h.vault_bytes > VAULT_CAP * 8 / 10 {
        h.problems.push(format!("The vault is {} MB, near its {} MB limit.", h.vault_bytes >> 20, VAULT_CAP >> 20));
    }
    if h.harvest_at == 0 || ago(h.harvest_at) > 2 * 3600 {
        h.problems.push("Harvest hasn't run in 2 hours (should be every 30 min).".into());
    }
    if h.hooks.last_start.get("claude").map_or(true, |t| ago(*t) > crate::time::DAY) {
        h.problems.push("No Claude chat got memory in a day: check its hook.".into());
    }
    if h.hooks.last_start.get("codex").map_or(true, |t| ago(*t) > 3 * crate::time::DAY) {
        h.problems.push("No Codex chat got memory in 3 days: /hooks in codex.".into());
    }
    if !big_digest.is_empty() {
        h.problems.push(format!("Digest near its 512 KB cap: {}.", big_digest.join(", ")));
    }
    if !cut_short.is_empty() {
        h.problems.push(format!("Start text cut short: {}.", cut_short.join(", ")));
    }
    if blind > 0 {
        h.problems.push(format!("{blind} notes have no description, so no index shows them."));
    }
    if dupes > 0 {
        h.problems.push(format!("{dupes} notes repeat another's description (same folder)."));
    }
    if h.stale > 0 {
        h.problems.push(format!("{} notes are over {STALE_DAYS} days old (marked in the index).", h.stale));
    }
    h.list = notes();
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_sets_the_updated_line_and_reading_skips_the_frontmatter() {
        let day = "2026-10-17";
        assert_eq!(with_updated("---\ndescription: d\nupdated: 2025-01-02\n---\nbody\n", day).unwrap(), "---\ndescription: d\nupdated: 2026-10-17\n---\nbody\n");
        // Windows line endings and a byte order mark stay as they were.
        assert_eq!(with_updated("\u{feff}---\r\ndescription: d\r\nupdated: 2025-01-02\r\n---\r\nb", day).unwrap(), "\u{feff}---\r\ndescription: d\r\nupdated: 2026-10-17\r\n---\r\nb");
        // No updated line: one is added at the end of the frontmatter.
        assert_eq!(with_updated("---\ndescription: d\n---\nb", day).unwrap(), "---\ndescription: d\nupdated: 2026-10-17\n---\nb");
        // No frontmatter: one is made.
        assert_eq!(with_updated("just text", day).unwrap(), "---\nupdated: 2026-10-17\n---\njust text");
        assert_eq!(with_updated("---\nnever closed", day), None);
        assert_eq!(after_frontmatter("---\ndescription: d\n---\nline one\nline two"), "line one\nline two");
        assert_eq!(after_frontmatter("\u{feff}---\r\nd: x\r\n---\r\nbody"), "body");
        assert_eq!(after_frontmatter("no frontmatter"), "no frontmatter");
    }

    #[test]
    fn only_notes_can_be_changed_from_the_dashboard() {
        let v = vault();
        assert!(!is_note(&v.join("projects").join("x").join("INDEX.md")));
        assert!(!is_note(&v.join("projects").join("x").join("digest").join("2026-10.md")));
        assert!(!is_note(&v.join("README.md")));
        assert!(!is_note(Path::new("C:\\Windows\\win.ini")));
        if let Some(n) = notes().first() {
            assert!(is_note(&n.path), "{}", n.path.display());
        }
    }

    #[test]
    fn codex_hooks_are_told_apart_from_claude_hooks() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(tool_of(&j(r#"{"session_id":"a","turn_id":"t","transcript_path":null}"#)), "codex");
        assert_eq!(tool_of(&j(r#"{"transcript_path":"C:\\Users\\x\\.codex\\sessions\\2026\\r.jsonl"}"#)), "codex");
        assert_eq!(tool_of(&j(r#"{"session_id":"a","transcript_path":"C:\\Users\\x\\.claude\\projects\\p\\s.jsonl"}"#)), "claude");
        // A Claude prompt never gets the start text from the prompt hook.
        assert_eq!(codex_first_prompt(&j(r#"{"session_id":"a","cwd":"C:\\","transcript_path":"C:\\x.jsonl"}"#)), "");
    }

    #[test]
    fn a_note_whose_first_2_kb_end_inside_a_letter_still_counts() {
        let dir = PathBuf::from(std::env::var("USERPROFILE").unwrap()).join(format!(".hive-meta-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("n.md");
        let mut text = String::from("\u{feff}---\ndescription: d\nupdated: 2026-10-16\n---\n");
        while text.len() < 2047 {
            text.push('a');
        }
        text.push('\u{00e9}'); // two bytes: the 2 KB read ends between them
        std::fs::write(&p, &text).unwrap();
        let meta = note_meta(&p);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(meta.map(|m| (m.1, m.2)), Some(("d".to_string(), "2026-10-16".to_string())));
    }

    #[test]
    fn a_long_chat_is_reminded_once_per_level_and_never_more_than_three_times() {
        assert_eq!(remind_level(1 << 20, 0), None);
        assert_eq!(remind_level(5 << 20, 0), Some(1));
        assert_eq!(remind_level(5 << 20, 1), None, "already told at this size");
        assert_eq!(remind_level(20 << 20, 1), Some(2));
        assert_eq!(remind_level(900 << 20, 3), None, "three times at most");
    }

    #[test]
    fn pending_work_is_read_from_a_chat_summary() {
        let s = "Summary:\n1. Primary Request: build it\n7. Pending Tasks:\n- Alex to run Tidy up\n8. Current Work:\nThe rename was verified.\n9. Optional Next Step: none";
        assert_eq!(pending_of(s).as_deref(), Some("pending: - Alex to run Tidy up | now: The rename was verified."));
        assert_eq!(pending_of("7. Pending Tasks: None\n8. Current Work: none."), None);
        assert_eq!(pending_of("nothing here"), None);
    }

    fn hit(score: usize, note: bool, mtime: u64, line_no: usize, chat: Option<&str>) -> Hit {
        Hit { score, dead: false, note, mine: false, mtime, line_no, chat: chat.map(String::from), text: format!("{note}{line_no}"), path: PathBuf::new(), links: Vec::new() }
    }

    #[test]
    fn a_note_outranks_digest_lines_with_the_same_words() {
        let r = rank_hits(vec![hit(2, false, 900, 5, Some("a · c")), hit(2, true, 100, 1, None), hit(3, false, 50, 2, Some("b · d"))]);
        assert_eq!(r.iter().map(|h| (h.score, h.note)).collect::<Vec<_>>(), vec![(3, false), (2, true), (2, false)]);
    }

    #[test]
    fn five_lines_of_one_chat_collapse_to_its_newest_two() {
        let mut v: Vec<Hit> = (1..=5).map(|n| hit(2, false, 100, n * 10, Some("acct · chat"))).collect();
        v.push(hit(2, false, 100, 7, Some("acct · other")));
        v.push(hit(2, true, 1, 1, None));
        let r = rank_hits(v);
        let mut chat: Vec<usize> = r.iter().filter(|h| h.chat.as_deref() == Some("acct · chat")).map(|h| h.line_no).collect();
        chat.sort();
        assert_eq!(chat, vec![40, 50]);
        assert_eq!(r.len(), 4, "the other chat and the note stay");
        assert!(r[0].note);
    }

    #[test]
    fn an_untagged_line_joins_the_tagged_lines_of_its_chat() {
        let v = vec![hit(2, false, 100, 1, Some("o · w5")), hit(2, false, 100, 2, Some("o · w5: recipe")), hit(2, false, 100, 3, Some("o · w5: recipe")), hit(2, false, 100, 4, Some("o · w5x"))];
        let r = rank_hits(v);
        assert_eq!(r.iter().map(|h| h.line_no).collect::<Vec<_>>(), vec![4, 3, 2]);
    }

    #[test]
    fn a_chat_is_the_account_and_title_without_the_state_tag() {
        assert_eq!(chat_key("- 19:07 Other · AS W5: importer [review ready]: waiting on x: y").as_deref(), Some("other · as w5: importer"));
        assert_eq!(chat_key("- 20:27 Other · AS W5: importer [working]: next").as_deref(), Some("other · as w5: importer"));
        assert_eq!(chat_key("- 08:01 Team · Fix login: all green").as_deref(), Some("team · fix login"));
        assert_eq!(chat_key("# heading"), None);
        assert_eq!(chat_key("- (this month's digest is full)"), None);
    }

    /// A scratch vault under the profile (not Temp), removed by the caller.
    fn scratch_vault(tag: &str) -> PathBuf {
        PathBuf::from(std::env::var("USERPROFILE").unwrap()).join(format!(".hive-dig-{tag}-{}", std::process::id()))
    }

    /// 2025-10-09 about 11:00 UTC, so a few minutes stay inside one local day.
    const T0: u64 = 1_760_000_000 + 7_200;

    fn item(at: u64, text: &str, key: &str) -> Line {
        ("p".into(), at, text.into(), key.into())
    }

    fn digest_text(v: &Path, at: u64) -> String {
        let st = crate::time::local(at);
        std::fs::read_to_string(v.join("projects").join("p").join("digest").join(format!("{:04}-{:02}.md", st.wYear, st.wMonth))).unwrap_or_default()
    }

    fn chat_lines(text: &str) -> Vec<&str> {
        text.lines().filter(|l| l.starts_with("- ")).collect()
    }

    #[test]
    fn ten_ticks_of_one_chat_keep_first_state_changes_and_one_latest_line() {
        let v = scratch_vault("ticks");
        let states = ["", "", "", "", " [review ready]", " [review ready]", " [review ready]", " [blocked]", " [blocked]", " [blocked]"];
        // One harvest per tick, as the 30-minute harvests would run.
        for (i, st) in states.iter().enumerate() {
            let at = T0 + 60 * i as u64;
            write_digests(&v, &[item(at, &format!("Acct · W5: job{st}: tick {i}"), &format!("k{i}"))]);
        }
        let text = digest_text(&v, T0);
        let l = chat_lines(&text);
        // first (0), then tick 3 is the latest of "none"... states: 0 first; 4 changes; 7 changes; 9 latest.
        assert_eq!(l.len(), 4, "{text}");
        assert!(l[0].contains("tick 0") && l[1].contains("tick 4") && l[2].contains("tick 7") && l[3].contains("tick 9"), "{text}");
        assert_eq!(text.matches("## ").count(), 1);
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn a_stable_chat_is_two_lines_a_day_and_the_latest_is_replaced_not_appended() {
        let v = scratch_vault("stable");
        for i in 0..8u64 {
            write_digests(&v, &[item(T0 + 60 * i, &format!("Acct · W5 [working]: tick {i}"), &format!("k{i}"))]);
        }
        let text = digest_text(&v, T0);
        let l = chat_lines(&text);
        assert_eq!(l.len(), 2, "{text}");
        assert!(l[0].contains("tick 0") && l[1].contains("tick 7"), "{text}");
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn one_harvest_with_many_lines_folds_the_same_way_and_chats_stay_apart() {
        let v = scratch_vault("batch");
        let mut items = Vec::new();
        for i in 0..6u64 {
            items.push(item(T0 + 60 * i, &format!("Acct · A [working]: a{i}"), &format!("a{i}")));
            items.push(item(T0 + 60 * i + 30, &format!("Acct · B [working]: b{i}"), &format!("b{i}")));
        }
        items.push(item(T0 + 600, "chat summary: not a chat", "s1"));
        items.push(item(T0 + 601, "chat summary: not a chat either", "s2"));
        let kept = write_digests(&v, &items);
        assert_eq!(kept.len(), items.len());
        let text = digest_text(&v, T0);
        let l = chat_lines(&text);
        assert_eq!(l.len(), 2 + 2 + 2, "{text}"); // A: first+latest, B: first+latest, 2 non-chat lines kept
        assert!(text.contains("a5") && text.contains("b5") && text.contains("a0") && text.contains("b0") && !text.contains("a3"), "{text}");
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn an_untagged_line_belongs_to_the_tagged_chat_it_names() {
        assert!(same_chat("Other · AS W10: HD terrain [review ready]: x", "Other · AS W10: recipes validated"));
        assert!(!same_chat("Other · AS W1 [working]: x", "Other · AS W10: y"));
        assert!(!same_chat("Codex: a", "Codex: b"));
        assert_eq!(state_of("Other · W [review ready]: x"), "review ready");
        assert_eq!(state_of("Other · W: x"), "");
    }

    #[test]
    fn a_new_day_starts_fresh_with_its_own_first_line() {
        let v = scratch_vault("days");
        for i in 0..4u64 {
            write_digests(&v, &[item(T0 + 60 * i, &format!("Acct · W5 [working]: d1 tick {i}"), &format!("x{i}"))]);
        }
        let next = T0 + 86_400;
        for i in 0..4u64 {
            write_digests(&v, &[item(next + 60 * i, &format!("Acct · W5 [working]: d2 tick {i}"), &format!("y{i}"))]);
        }
        let text = digest_text(&v, T0);
        let l = chat_lines(&text);
        assert_eq!(l.len(), 4, "{text}");
        assert!(l[0].contains("d1 tick 0") && l[1].contains("d1 tick 3") && l[2].contains("d2 tick 0") && l[3].contains("d2 tick 3"), "{text}");
        assert_eq!(text.matches("\n## ").count(), 2);
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn the_same_lines_written_twice_do_not_duplicate_or_lose_anything() {
        let v = scratch_vault("twice");
        let items = [item(T0, "Acct · W5 [working]: one", "k1"), item(T0 + 60, "Acct · W5 [working]: two", "k2")];
        write_digests(&v, &items[..1]);
        write_digests(&v, &items[1..]);
        let one = digest_text(&v, T0);
        // The harvest's seen-list stops a second run from re-adding k1 and k2.
        let st: HashSet<String> = ["k1".to_string(), "k2".to_string()].into();
        let rest: Vec<Line> = items.iter().filter(|l| !st.contains(&l.3)).cloned().collect();
        write_digests(&v, &rest);
        assert_eq!(digest_text(&v, T0), one);
        assert_eq!(chat_lines(&one).len(), 2);
        // No temp file is left behind.
        let dir = v.join("projects").join("p").join("digest");
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| e.file_name().to_string_lossy().ends_with(".md")));
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn a_second_harvest_cannot_take_the_lock_while_the_first_holds_it() {
        let v = scratch_vault("lock");
        std::fs::create_dir_all(&v).unwrap();
        let lock = v.join("harvest.lock");
        assert!(take_lock(&lock));
        assert!(!take_lock(&lock));
        let _ = std::fs::remove_file(&lock);
        assert!(take_lock(&lock));
        let _ = std::fs::remove_dir_all(&v);
    }

    /// AC2 replay (run by hand): `HIVE_REPLAY_SRC` is a real digest file, `HIVE_REPLAY_DAY` a
    /// `yyyy-mm-dd` day in it (must be the day before today), `HIVE_REPLAY_OUT` a scratch vault.
    #[test]
    #[ignore]
    fn replay_a_real_day_into_a_scratch_vault() {
        let (src, day, out) = (std::env::var("HIVE_REPLAY_SRC").unwrap(), std::env::var("HIVE_REPLAY_DAY").unwrap(), PathBuf::from(std::env::var("HIVE_REPLAY_OUT").unwrap()));
        let now = crate::time::now();
        let t = crate::time::local(now);
        let midnight_today = now - (t.wHour as u64 * 3600 + t.wMinute as u64 * 60 + t.wSecond as u64);
        assert_eq!(ymd(midnight_today - 43_200), day, "the replay day must be yesterday");
        let midnight = midnight_today - 86_400;
        let text = std::fs::read_to_string(src).unwrap();
        let mut in_day = false;
        let mut items: Vec<Line> = Vec::new();
        for (i, l) in text.lines().enumerate() {
            if l.starts_with("## ") {
                in_day = l == format!("## {day}");
            } else if in_day && l.starts_with("- ") {
                let (clock, rest) = l[2..].split_once(' ').unwrap();
                let (h, m) = clock.split_once(':').unwrap();
                let at = midnight + h.parse::<u64>().unwrap() * 3600 + m.parse::<u64>().unwrap() * 60;
                items.push(("replay".into(), at, rest.to_string(), format!("k{i}")));
            }
        }
        // One harvest per half hour, as the tray runs them.
        let mut kept = 0;
        for chunk in items.chunk_by(|a, b| (a.1 - midnight) / 1800 == (b.1 - midnight) / 1800) {
            kept += write_digests(&out, chunk).len();
        }
        let after = std::fs::read_to_string(out.join("projects").join("replay").join("digest").join(format!("{}.md", &day[..7]))).unwrap();
        let count = |t: &str| {
            let mut m: std::collections::BTreeMap<String, usize> = Default::default();
            for l in t.lines().filter(|l| l.starts_with("- ")) {
                *m.entry(chat_key(l).unwrap_or_else(|| "(no chat)".into())).or_default() += 1;
            }
            m
        };
        let before = count(&items.iter().map(|i| format!("- 00:00 {}\n", i.2)).collect::<String>());
        let after_n = count(&after);
        let mut report = format!("lines before {} after {} (kept {kept})\n", items.len(), after_n.values().sum::<usize>());
        for (k, b) in &before {
            report.push_str(&format!("{b:>3} -> {:>3}  {k}\n", after_n.get(k).copied().unwrap_or(0)));
        }
        std::fs::write(out.join("replay-report.txt"), report).unwrap();
    }

    #[test]
    fn digests_stop_at_their_cap_with_one_closing_line() {
        let v = scratch_vault("cap");
        let file = v.join("projects").join("p").join("digest");
        write_digests(&v, &[item(T0, "first", "k0")]);
        let path = std::fs::read_dir(&file).unwrap().flatten().next().unwrap().path();
        let pad = "x".repeat((DIGEST_FILE_CAP - 400) as usize);
        let mut t = std::fs::read_to_string(&path).unwrap();
        t.push_str(&format!("<!-- {pad} -->\n"));
        std::fs::write(&path, t).unwrap();
        let big = "y".repeat(LINE_CHARS);
        let items: Vec<Line> = (1..8u64).map(|i| item(T0 + i, &format!("{big}{i}"), &format!("k{i}"))).collect();
        let kept = write_digests(&v, &items);
        assert_eq!(kept.len(), 7);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("digest is full").count(), 1, "full marker once");
        assert!(text.contains("first") && !text.contains(&format!("{big}7")));
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn projects_are_named_after_their_folder_and_throwaway_folders_share_general() {
        // Not under Temp or AppData: those have rules of their own.
        let dir = PathBuf::from(std::env::var("USERPROFILE").unwrap()).join(format!(".hive-mem-test-{}", std::process::id()));
        let repo = dir.join("Honey Glade");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert_eq!(project_of(&repo.join("src").to_string_lossy()).as_deref(), Some("honey-glade"));
        let scratch = dir.join("scratch-2026-10-16-abc123");
        std::fs::create_dir_all(&scratch).unwrap();
        assert_eq!(project_of(&scratch.to_string_lossy()).as_deref(), Some("general"));
        let dated = dir.join("2026-10-14").join("think-about-it");
        std::fs::create_dir_all(&dated).unwrap();
        assert_eq!(project_of(&dated.to_string_lossy()).as_deref(), Some("general"));
        assert_eq!(project_of("C:\\Users\\x\\Private Notes\\notes"), None, "private folders are never used");
        assert_eq!(project_of(""), None);
        let home = std::env::var("USERPROFILE").unwrap();
        // A projects folder: its first folder names the project, however deep the chat is.
        assert_eq!(project_of(&format!(r"{home}\dev\zz-hive-test\art\concepts")).as_deref(), Some("zz-hive-test"));
        assert_eq!(project_of(&format!(r"{home}\dev\zz-hive-test-wt\branch")).as_deref(), Some("zz-hive-test"));
        assert_eq!(project_of(&format!(r"{home}\dev")).as_deref(), Some("general"));
        assert_eq!(project_of(&format!(r"{home}\Documents\Codex\2026-10-14\a-task")).as_deref(), Some("general"));
        assert_eq!(project_of(&format!(r"{home}\.claude\tools\my-app\src")).as_deref(), Some("my-app"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_index_is_rebuilt_from_descriptions_newest_first_and_capped() {
        let dir = std::env::temp_dir().join(format!("hive-idx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, d) in ["2026-10-01", "2026-10-03", "2026-10-02"].iter().enumerate() {
            std::fs::write(dir.join(format!("n{i}.md")), format!("---\ndescription: note {i}\nupdated: {d}\n---\nbody")).unwrap();
        }
        std::fs::write(dir.join("no-front.md"), "just text").unwrap();
        // A date in another script must not crash the start-of-chat hook.
        std::fs::write(dir.join("odd-date.md"), "---
description: odd
updated: 2026年10月16日
---
").unwrap();
        let lines = rebuild_index(&dir, 2);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains("2 older notes"), "{lines:?}");
        // The unreadable date falls back to the file's own time (today), so it sorts first.
        assert!(lines[0].contains("[[odd-date]]"), "{lines:?}");
        assert!(lines[1].contains("[[n1]]") && lines[1].contains("note 1"), "{lines:?}");
        assert!(std::fs::read_to_string(dir.join("INDEX.md")).unwrap().contains("Do not edit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_are_read_once_each_and_never_from_code() {
        let text = "See [[Alpha]] and [[beta|the beta note]], [[gamma#part]], [[sub/Delta.md]].\n\
                    Again [[alpha]]. In code `[[not-a-link]]` stays out.\n```\n[[fenced]]\n```\n[[open";
        assert_eq!(links_in(text), ["alpha", "beta", "gamma", "delta"]);
    }

    #[test]
    fn paths_are_found_and_only_real_missing_ones_count() {
        let home = std::env::var("USERPROFILE").unwrap();
        let text = format!(
            "Source: `{home}\\.claude\\settings.json` and `{home}\\nowhere-hive-test\\data.txt --flag`.\n\
             Run `{home}\\.claude --memory start`. Pattern `C:\\Users\\<name>\\x`, `~\\.claude\\skills\\tool-*` and C:\\some\\*.md skip.\n\
             The old file C:\\gone-hive-test\\a.txt was removed.\n~\\nowhere-hive-test2 end."
        );
        let found = paths_in(&text);
        assert!(found.iter().any(|p| p.ends_with("settings.json")), "{found:?}");
        assert!(!found.iter().any(|p| p.contains('<') || p.contains('*') || p.contains("gone-hive-test")), "{found:?}");
        let missing: Vec<&String> = found.iter().filter(|p| path_missing(p)).collect();
        // The settings file and the folder with arguments after it exist; the two made-up ones don't.
        assert_eq!(missing.len(), 2, "{missing:?}");
        assert!(missing.iter().all(|p| p.contains("nowhere-hive-test")), "{missing:?}");
        // Outside backticks a path may hold spaces: it is tried word by word.
        let spaced = std::env::temp_dir().join(format!("hive spaced {}", std::process::id()));
        std::fs::create_dir_all(&spaced).unwrap();
        let prose = format!("The folder {} holds it, and C:\\hive-nowhere-test\\a b is absent. Skills ~\\.claude\\skills\\tool-<name> vary.", spaced.display());
        let found = paths_in(&prose);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(!path_missing(&found[0]) && path_missing(&found[1]), "{found:?}");
        let _ = std::fs::remove_dir_all(&spaced);
        // A drive that isn't there says nothing.
        assert!(!path_missing("Q:\\not\\connected.txt") || Path::new("Q:\\").exists());
    }

    #[test]
    fn prose_placeholders_locks_and_examples_are_not_dead_paths() {
        let text = "Moved A:\\hive-nowhere\\Archive\\same names to it.\n\
                    C:\\hive-msys64 does not exist here.\n\
                    Slots A:/hive-nowhere/slots/sN/tool/override.cfg and A:/hive-nowhere/slots/leases/s1.lock.\n\
                    Copy to F:/hive-nowhere/NAME and F:\\hive-nowhere\\X - the rest.\n\
                    Held by F:/hive-nowhere/.heavy.lock and C:\\hive-nowhere\\run.tmp.\n\
                    Try it e.g. C:\\hive-nowhere\\example-dir for a test.\n\
                    The tool at C:\\hive-nowhere\\tool.dll is Sample and C:\\hive-nowhere\\tools\\Open sample tool.bat opens it.\n\
                    Real: C:\\hive-nowhere\\real-dir reached 650 GB.";
        let found = paths_in(text);
        assert_eq!(found, ["C:\\hive-nowhere\\tool.dll", "C:\\hive-nowhere\\tools\\Open sample tool.bat", "C:\\hive-nowhere\\real-dir"], "{found:?}");
        // A run-on stops at the path, and a path the prose runs into is cut where the prose starts.
        assert_eq!(paths_in("See C:\\hive-nowhere\\Archive\\same names today."), ["C:\\hive-nowhere\\Archive\\same"]);
        assert_eq!(paths_in("Run `C:\\hive-nowhere\\tool.exe --flag x`."), ["C:\\hive-nowhere\\tool.exe"]);
        // Multi-byte text next to a path is no reason to panic.
        assert_eq!(paths_in("See C:\\hive-nowhere\\Pok\u{e9}mon Violet \u{2026} more and C:\\hive-nowhere\\b\u{2026}"), ["C:\\hive-nowhere\\Pok\u{e9}mon Violet"]);
        // Real dead paths stay listed when a skip word is only near them, not about them.
        assert_eq!(paths_in("See C:\\hive-nowhere\\examples\\run-a here."), ["C:\\hive-nowhere\\examples\\run-a"]);
        assert_eq!(paths_in("Focused to C:\\hive-nowhere\\b-dir and was attached to C:\\hive-nowhere\\c-dir."), ["C:\\hive-nowhere\\b-dir", "C:\\hive-nowhere\\c-dir"]);
        assert_eq!(paths_in("C:\\hive-nowhere\\gone-dir was removed, C:\\hive-nowhere\\live-dir is the new home."), ["C:\\hive-nowhere\\live-dir"]);
        assert!(paths_in("Plan was in C:\\hive-nowhere\\PLAN.md, notes there (removed with the folder, 2026-10-29).").is_empty());
        assert_eq!(paths_in("Repo at C:\\hive-nowhere\\dead-repo
old config was deleted."), ["C:\\hive-nowhere\\dead-repo"]);
        assert_eq!(paths_in("Files C:\\hive-nowhere\\Warlock, C:\\hive-nowhere\\block and C:\\hive-nowhere\\FILE\\USER\\IN\\x1 are real."), ["C:\\hive-nowhere\\Warlock", "C:\\hive-nowhere\\block", "C:\\hive-nowhere\\FILE\\USER\\IN\\x1"]);
        assert_eq!(paths_in("Under K:\\D\\work\\a-dir only."), ["K:\\D\\work\\a-dir"]);
        // A sentence wrapped onto the next line is one clause: the "deleted" there covers the path here.
        assert!(paths_in("Copied C:\\hive-nowhere\\snap-tex to A:,\nthen deleted it.\nKept C:\\hive-nowhere\\kept-dir.\nNext line.").iter().eq(["C:\\hive-nowhere\\kept-dir"].iter()));
        assert_eq!(paths_in("Game C:\\hive-nowhere\\Call of the Wild The Angler, build 1."), ["C:\\hive-nowhere\\Call of the Wild The Angler"]);
        // Names with small words stay whole; a colon, a drive written twice (`F:/D`) and a pause flag are not paths.
        assert_eq!(paths_in("In E:\\hive-nowhere\\World of Warcraft and K:/hive-nowhere/Call Of The Wild now."), ["E:\\hive-nowhere\\World of Warcraft", "K:/hive-nowhere/Call Of The Wild"]);
        assert_eq!(paths_in("List G:\\hive-nowhere\\Anime: SAO and D:/F: copies, C:\\hive-nowhere\\work\\PAUSE."), ["G:\\hive-nowhere\\Anime"]);
    }

    #[test]
    fn a_missing_path_on_one_drive_that_exists_on_another_is_a_move() {
        // Needs a second connected drive: put a folder in the temp dir's drive, then ask for it on a made-up one.
        let tmp = std::env::temp_dir().join(format!("hive-moved-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let t = tmp.to_string_lossy().to_string();
        let drive = t.chars().next().unwrap();
        let other = "ZYXWVUTSRQPONMLKJIHGFEDCBA".chars().find(|d| !Path::new(&format!("{d}:\\")).exists());
        if let Some(o) = other {
            // `moved_to` only looks at connected drives for the answer, so a missing source drive still finds it.
            let from = format!("{o}{}", &t[1..]);
            assert_eq!(moved_to(&from).map(|m| m.to_lowercase()), Some(t.to_lowercase()));
        }
        // A path that exists nowhere has no hint, and a non-drive path has none.
        assert_eq!(moved_to(&format!("{drive}:\\hive-nowhere-at-all\\x")), None);
        assert_eq!(moved_to("~\\x\\y"), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn near_same_names_are_told_from_different_ones() {
        assert!(near_names("deploy-notes", "Deploy_Notes"));
        assert!(near_names("codex-alerts", "codex-alert"));
        assert!(near_names("memory-system-design", "memory-systm-desgn"));
        assert!(!near_names("deploy-notes", "deploy-plans"));
        assert!(!near_names("ui", "uk"));
        assert_eq!(edits("kitten", "sitting"), 3);
    }

    #[test]
    fn the_check_and_links_read_a_fixture_vault() {
        let v = std::env::temp_dir().join(format!("hive-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&v);
        for d in ["projects\\app", "projects\\other", "shared", "working\\app"] {
            std::fs::create_dir_all(v.join(d)).unwrap();
        }
        let note = |rel: &str, desc: &str, updated: &str, body: &str| {
            std::fs::write(v.join(rel), format!("---\ndescription: {desc}\nupdated: {updated}\n---\n{body}\n")).unwrap();
        };
        let home = std::env::var("USERPROFILE").unwrap();
        note("projects\\app\\deploy-notes.md", "How the app deploys", "2026-10-15", "See [[build-steps]] and [[ghost]] and [[todo]].");
        note("projects\\app\\build-steps.md", "Build steps", "2026-10-15", "Back to [[deploy-notes]].");
        note("projects\\app\\deploy-note.md", "A second deploy note", "2026-10-15", &format!("Log at `{home}\\nowhere-hive-check\\log.txt`."));
        note("projects\\other\\build-steps.md", "Other project's build", "2026-10-15", "");
        note("shared\\old-fact.md", "How the app deploys", "2026-01-02", "");
        std::fs::write(v.join("working\\app\\todo.md"), "- a to-do list is a link target too\n").unwrap();
        let now = 1_792_497_600; // 2026-10-20, noon UTC
        assert_eq!(ymd(now), "2026-10-20");
        let c = check_at(&v, 60, now);
        assert_eq!(c.notes, 5);
        assert_eq!(c.old, [("shared\\old-fact.md".to_string(), "2026-01-02".to_string())]);
        assert_eq!(c.broken, [("projects\\app\\deploy-notes.md".to_string(), vec!["ghost".to_string()])]);
        assert_eq!(c.missing.len(), 1, "{:?}", c.missing);
        // Same description across folders, and near-same names in one folder; the two
        // build-steps notes live in different projects, so they are not duplicates.
        assert_eq!(c.dupes.len(), 2, "{:?}", c.dupes);
        assert!(c.dupes.iter().any(|d| d.2 == "same description") && c.dupes.iter().any(|d| d.2 == "near-same name"));
        assert_eq!(c.count(), 3);
        let text = check_text(&c, 60, false);
        assert!(text.contains("3 of 5 notes") && text.contains("[[ghost]]") && text.contains("nothing was changed"), "{text}");
        // A link resolves to the note in its own folder first.
        let links = links_text(&v, "[[build-steps]]");
        assert!(links.contains("projects\\app\\build-steps.md") && links.contains("projects\\other\\build-steps.md"), "{links}");
        let app = links.split("[[build-steps]] projects\\other").next().unwrap();
        assert!(app.contains("Linked from (1):\n- [[deploy-notes]] How the app deploys"), "{links}");
        let deploy = links_text(&v, "deploy-notes");
        assert!(deploy.contains("Links to (3):") && deploy.contains("[[ghost]] (no such note)") && deploy.contains("- [[build-steps]] Build steps"), "{deploy}");
        assert!(links_text(&v, "nope").starts_with("No note named"));
        let _ = std::fs::remove_dir_all(&v);
    }

    fn put_note(v: &Path, rel: &str, extra: &str, updated: &str, body: &str) {
        let f = v.join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, format!("---\ndescription: d\n{extra}updated: {updated}\n---\n{body}\n")).unwrap();
    }

    #[test]
    fn an_edited_added_or_deleted_note_shows_in_the_very_next_search() {
        let v = scratch_vault("fresh");
        // Enough files that every worker thread gets some.
        for i in 0..40 {
            put_note(&v, &format!("projects\\app\\filler-{i}.md"), "", "2026-10-20", "nothing to see in this filler note");
        }
        put_note(&v, "projects\\app\\target.md", "", "2026-10-20", "the quokka lives here");
        let (out, _) = search_at(&v, "quokka", None);
        assert!(out.contains("projects\\app\\target.md:") && out.contains("quokka lives"), "{out}");
        put_note(&v, "projects\\app\\target.md", "", "2026-10-21", "the wombat lives here now");
        let (out, _) = search_at(&v, "quokka", None);
        assert!(out.starts_with("No matches"), "stale text served: {out}");
        let (out, _) = search_at(&v, "wombat", None);
        assert!(out.contains("wombat lives"), "{out}");
        put_note(&v, "projects\\app\\added.md", "", "2026-10-21", "a wombat was added");
        let (out, _) = search_at(&v, "wombat", None);
        assert!(out.contains("added.md:") && out.contains("target.md:"), "{out}");
        std::fs::remove_file(v.join("projects\\app\\target.md")).unwrap();
        let (out, _) = search_at(&v, "wombat", None);
        assert!(out.contains("added.md:") && !out.contains("target.md"), "{out}");
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn a_dead_end_note_outranks_a_plain_note_and_says_so() {
        let v = scratch_vault("dead");
        put_note(&v, "projects\\app\\plain.md", "", "2026-10-20", "retry the flaky upload with a bigger buffer");
        put_note(&v, "projects\\app\\failed-upload.md", "type: dead-end\n", "2026-10-01", "retry the flaky upload with a bigger buffer failed on 2026-10-01");
        // Same words, same folder, same file time: only the dead-end rule orders them.
        let (out, shown) = search_at(&v, "flaky upload buffer", Some("app"));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("- Dead end: projects\\app\\failed-upload.md:"), "{out}");
        assert!(lines[1].starts_with("- projects\\app\\plain.md:") && !lines[1].contains("Dead end"), "{out}");
        assert_eq!(shown.len(), 2);
        assert!(is_dead_end("---\ntype: \"Dead-End\"\n---\nx") && !is_dead_end("---\ndescription: type: dead-end\n---\n") && !is_dead_end("type: dead-end\n"));
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn a_search_records_the_notes_it_returned_and_not_the_words() {
        let v = scratch_vault("hits");
        put_note(&v, "projects\\app\\a.md", "", "2026-10-20", "zebra crossing rules");
        put_note(&v, "projects\\app\\b.md", "", "2026-10-20", "nothing relevant");
        let (_, shown) = search_at(&v, "zebra crossing", None);
        record_hits(&v, &shown, 1_792_497_600);
        record_hits(&v, &shown, 1_792_497_700);
        let raw = std::fs::read_to_string(v.join(".hive").join("search-hits.json")).unwrap();
        assert!(!raw.contains("zebra"), "{raw}");
        let h: SearchHits = serde_json::from_str(&raw).unwrap();
        assert_eq!(h.since, 1_792_497_600);
        assert_eq!(h.notes.len(), 1);
        let e = h.notes["projects\\app\\a.md"];
        assert_eq!((e.count, e.last), (2, 1_792_497_700));
        // A note deleted since is dropped on the next write; nothing outside the vault is kept.
        std::fs::remove_file(v.join("projects\\app\\a.md")).unwrap();
        // Within a day of the last prune the deleted note stays; a day later it is dropped.
        record_hits(&v, &["projects\\app\\b.md".to_string()], 1_792_497_800);
        let h: SearchHits = serde_json::from_str(&std::fs::read_to_string(v.join(".hive").join("search-hits.json")).unwrap()).unwrap();
        assert_eq!(h.notes.len(), 2);
        record_hits(&v, &["projects\\app\\b.md".to_string()], 1_792_497_600 + crate::time::DAY + 1);
        let h: SearchHits = serde_json::from_str(&std::fs::read_to_string(v.join(".hive").join("search-hits.json")).unwrap()).unwrap();
        assert_eq!(h.notes.keys().collect::<Vec<_>>(), ["projects\\app\\b.md"]);
        assert!(!v.join(".hive").join("search-hits.lock").exists());
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn searches_at_once_do_not_lose_or_corrupt_hits() {
        let v = scratch_vault("race");
        put_note(&v, "projects\\app\\a.md", "", "2026-10-20", "x");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let v = v.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        record_hits(&v, &["projects\\app\\a.md".to_string()], 1_792_497_600);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let h: SearchHits = serde_json::from_str(&std::fs::read_to_string(v.join(".hive").join("search-hits.json")).unwrap()).unwrap();
        assert_eq!(h.notes["projects\\app\\a.md"].count, 80);
        let left: Vec<_> = std::fs::read_dir(v.join(".hive")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left, ["search-hits.json"], "{left:?}");
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn a_hits_file_held_open_during_the_write_still_records_the_hit() {
        let v = scratch_vault("held");
        put_note(&v, "projects\\app\\a.md", "", "2026-10-20", "x");
        record_hits(&v, &["projects\\app\\a.md".to_string()], 1_792_497_600);
        let file = v.join(".hive").join("search-hits.json");
        let held = std::fs::File::open(&file).unwrap();
        let v2 = v.clone();
        let t = std::thread::spawn({
            let v = v.clone();
            move || record_hits(&v, &["projects\\app\\a.md".to_string()], 1_792_497_600)
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(held);
        t.join().unwrap();
        let h: SearchHits = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(h.notes["projects\\app\\a.md"].count, 2);
        let left: Vec<_> = std::fs::read_dir(v2.join(".hive")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left, ["search-hits.json"], "{left:?}");
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    #[ignore]
    fn stress_16_searches_at_once_do_not_lose_hits() {
        let v = scratch_vault("race16");
        put_note(&v, "projects\\app\\a.md", "", "2026-10-20", "x");
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let v = v.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        record_hits(&v, &["projects\\app\\a.md".to_string()], 1_792_497_600);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let h: SearchHits = serde_json::from_str(&std::fs::read_to_string(v.join(".hive").join("search-hits.json")).unwrap()).unwrap();
        assert_eq!(h.notes["projects\\app\\a.md"].count, 160);
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn check_lists_old_notes_no_search_returned_once_30_days_of_data_exist() {
        let v = scratch_vault("unused");
        let now = 1_792_497_600; // 2026-10-20
        put_note(&v, "projects\\app\\hit.md", "", "2026-08-01", "x");
        put_note(&v, "projects\\app\\stale-hit.md", "", "2026-08-01", "x");
        put_note(&v, "projects\\app\\never.md", "", "2026-08-01", "x");
        put_note(&v, "projects\\app\\young.md", "", "2026-10-15", "x");
        // Before any search: says tracking has not started.
        assert!(check_text(&check_at(&v, 60, now), 60, false).contains("tracking starts with the next search"));
        // Ten days of data: no list, a date.
        record_hits(&v, &["projects\\app\\hit.md".to_string()], now - 10 * crate::time::DAY);
        let early = check_text(&check_at(&v, 60, now), 60, false);
        assert!(early.contains("tracking since 2026-10-10, not enough data yet") && !early.contains("never.md: last"), "{early}");
        // 40 days of data: stale-hit was returned 35 days ago, hit 1 day ago.
        std::fs::remove_dir_all(v.join(".hive")).unwrap();
        record_hits(&v, &["projects\\app\\stale-hit.md".to_string()], now - 40 * crate::time::DAY);
        record_hits(&v, &["projects\\app\\hit.md".to_string()], now - crate::time::DAY);
        let c = check_at(&v, 60, now);
        let names: Vec<&str> = c.unused.iter().map(|u| u.0.as_str()).collect();
        assert_eq!(names, ["projects\\app\\stale-hit.md", "projects\\app\\never.md"], "{:?}", c.unused);
        let text = check_text(&c, 60, false);
        assert!(text.contains("Notes no search returned in 30 days (2, tracking since 2026-09-10)") && text.contains("never.md: last returned never"), "{text}");
        let section = text.split("Notes no search returned").nth(1).unwrap();
        assert!(!section.contains("projects\\app\\hit.md") && !section.contains("young.md"), "{text}");
        let _ = std::fs::remove_dir_all(&v);
    }

    #[test]
    fn obsidian_opens_only_a_folder_it_already_knows() {
        let v = Path::new("C:\\Users\\Someone\\Documents\\Hive Memory");
        let known = r#"{"vaults":{"a":{"path":"C:\\Users\\Someone\\Documents\\Obsidian Vault"},"b":{"path":"c:/users/someone/documents/hive memory/"}}}"#;
        assert!(obsidian_knows(known, v));
        assert!(!obsidian_knows(r#"{"vaults":{"a":{"path":"C:\\Other"}}}"#, v));
        assert!(!obsidian_knows("not json", v));
        assert_eq!(url_encode("C:\\A b"), "C%3A%5CA%20b");
    }
}
