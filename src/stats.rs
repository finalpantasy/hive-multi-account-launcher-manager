//! Token and cost statistics for the dashboard, read from Claude Code's own
//! transcripts (~/.claude/projects/<folder>/<session>.jsonl, subagents included).
//!
//! Each assistant reply carries its usage: input, cache writes (5-minute and 1-hour),
//! cache reads, output, the model, and whether it ran in fast mode. A reply is written
//! several times while it streams (up to 25 copies with the same message id and
//! request id), so each counts once, as its last (largest) copy. Every account shares
//! the one transcripts folder: a transcript's session id is matched to an account
//! through that account's chat records (their cliSessionId); the rest are chats run in
//! a terminal or elsewhere.
//!
//! The first read covers every file (19 GB on 2026-10-15) on worker threads; after
//! that only files whose size or time changed are read again. Cost is the API price
//! of the same tokens, for comparison: a subscription doesn't bill per token.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---- prices -----------------------------------------------------------------

/// API prices per million tokens: (input, output, cache read). Claude cache writes cost
/// 1.25x input (5-minute) and 2x input (1-hour). From the claude-api reference,
/// 2026-10-06; the 4.1-and-older Opus rate is from memory. OpenAI rows are in `OPENAI`.
fn price(model: &str) -> Option<(f64, f64, f64)> {
    // OpenAI (Codex) standard tier, short context, from developers.openai.com/api/docs/pricing
    // (platform.openai.com/docs/pricing redirects there), fetched 2026-11-24: (input, output,
    // cached input). gpt-daybreak-blue-latest is not on that page, so it stays unpriced.
    const OPENAI: [(&str, (f64, f64, f64)); 7] = [
        ("gpt-6.2-astra", (8.0, 40.0, 0.40)),
        ("gpt-6.2-sol", (2.0, 10.0, 0.10)),
        ("gpt-6.1-sol", (2.0, 10.0, 0.10)),
        ("gpt-6.1-terra", (1.0, 5.0, 0.05)),
        ("gpt-6-astra", (10.0, 50.0, 1.0)),
        ("gpt-6-sol", (2.0, 10.0, 0.20)),
        ("gpt-6-luna", (0.10, 0.50, 0.01)),
    ];
    const TABLE: [(&str, (f64, f64, f64)); 18] = [
        ("claude-fable-5", (10.0, 50.0, 0.25)),
        ("claude-mythos-5", (10.0, 50.0, 0.25)),
        ("claude-opus-5-5", (4.0, 20.0, 0.20)),
        ("claude-opus-5", (5.0, 25.0, 0.50)),
        ("claude-opus-4-8", (5.0, 25.0, 0.50)),
        ("claude-opus-4-7", (5.0, 25.0, 0.50)),
        ("claude-opus-4-6", (5.0, 25.0, 0.50)),
        ("claude-opus-4-5", (5.0, 25.0, 0.50)),
        ("claude-opus-4-1", (15.0, 75.0, 1.50)),
        ("claude-opus-4", (15.0, 75.0, 1.50)),
        ("claude-sonnet-5-5", (2.0, 10.0, 0.20)),
        ("claude-sonnet-5", (2.0, 10.0, 0.20)),
        ("claude-sonnet-4-6", (3.0, 15.0, 0.30)),
        ("claude-sonnet-4-5", (3.0, 15.0, 0.30)),
        ("claude-sonnet-4", (3.0, 15.0, 0.30)),
        ("claude-haiku-5-5", (0.10, 0.50, 0.01)),
        ("claude-haiku-4-5", (1.0, 5.0, 0.10)),
        ("claude-3-5-haiku", (0.80, 4.0, 0.08)),
    ];
    // Longest matching prefix: "claude-opus-5-5" before "claude-opus-5".
    TABLE.iter().chain(OPENAI.iter()).filter(|(p, _)| model.starts_with(p)).max_by_key(|(p, _)| p.len()).map(|(_, v)| *v)
}

/// Dollars for these tokens on this model; None when the model has no known price.
pub fn cost(model: &str, fast: bool, t: &Tokens) -> Option<f64> {
    let (i, o, r) = price(model)?;
    let m = 1e-6 * if fast { 2.0 } else { 1.0 };
    // OpenAI has no cache-write surcharge: a write is plain input.
    let (w5, w1h) = if model.starts_with("gpt-") { (1.0, 1.0) } else { (1.25, 2.0) };
    Some(m * (t.input as f64 * i + t.w5 as f64 * i * w5 + t.w1h as f64 * i * w1h + t.read as f64 * r + t.output as f64 * o))
}

/// A friendlier model name: "claude-opus-5-5" becomes "Opus 5.5".
pub fn model_label(model: &str) -> String {
    // OpenAI's: "gpt-6.1-sol" becomes "GPT-6.1 Sol"; others like "codex-auto-review" read as words.
    let cap = |w: &str| {
        let mut c = w.chars();
        c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
    };
    if let Some(rest) = model.strip_prefix("gpt-") {
        let mut parts = rest.split('-');
        let ver = parts.next().unwrap_or("");
        let tail: Vec<String> = parts.map(cap).collect();
        return if tail.is_empty() { format!("GPT-{ver}") } else { format!("GPT-{ver} {}", tail.join(" ")) };
    }
    if !model.starts_with("claude-") {
        return cap(&model.replace('-', " "));
    }
    let m = model.strip_prefix("claude-").unwrap_or(model);
    let mut parts = m.split('-').filter(|p| !(p.len() == 8 && p.chars().all(|c| c.is_ascii_digit())));
    let Some(family) = parts.next() else { return model.to_string() };
    let ver: Vec<&str> = parts.collect();
    let mut name = family.to_string();
    if let Some(f) = name.get_mut(0..1) {
        f.make_ascii_uppercase();
    }
    if ver.is_empty() { name } else { format!("{name} {}", ver.join(".")) }
}

// ---- the index ----------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug, PartialEq)]
pub struct Tokens {
    pub input: u64,
    /// Cache writes kept 5 minutes and 1 hour.
    pub w5: u64,
    pub w1h: u64,
    /// Cache reads.
    pub read: u64,
    pub output: u64,
    pub replies: u64,
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.w5 += o.w5;
        self.w1h += o.w1h;
        self.read += o.read;
        self.output += o.output;
        self.replies += o.replies;
    }
    pub fn total(&self) -> u64 {
        self.input + self.w5 + self.w1h + self.read + self.output
    }
}

/// One group of replies in one file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    /// Local date as yyyymmdd, and the hour (0-23).
    pub day: u32,
    pub hour: u8,
    pub session: String,
    pub model: String,
    /// The working folder's name.
    pub project: String,
    pub fast: bool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct FileEntry {
    size: u64,
    mtime: u64,
    groups: Vec<(Key, Tokens)>,
}

/// 3: a resumed chat's copied lines are no longer counted a second time.
const INDEX_VERSION: u32 = 3;

/// Where a transcript comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// Claude Code: the account is found later from the session id.
    Claude,
    /// Codex rollouts of this sign-in ("Codex" or an extra sign-in's name).
    Codex(String),
}

/// Sessions written as this prefix plus an account name belong to that account directly.
pub const DIRECT: char = '\u{1}';

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Index {
    version: u32,
    files: HashMap<String, FileEntry>,
}

impl Index {
    pub fn load(path: &Path) -> Index {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Index>(&b).ok())
            .filter(|i| i.version == INDEX_VERSION)
            .unwrap_or(Index { version: INDEX_VERSION, files: HashMap::new() })
    }

    pub fn save(&self, path: &Path) {
        if let Ok(text) = serde_json::to_vec(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// Read the files that are new or changed under `root` (and drop the ones that are
    /// gone). Returns how many files were read. `progress(done, of)` is called as it goes.
    pub fn update(&mut self, roots: &[(PathBuf, Source)], progress: &(dyn Fn(usize, usize) + Sync)) -> usize {
        let mut all = Vec::new();
        for (root, src) in roots {
            let mut found = Vec::new();
            walk(root, &mut found);
            all.extend(found.into_iter().map(|(p, size, mtime)| (p, size, mtime, src.clone())));
        }
        let seen: std::collections::HashSet<String> = all.iter().map(|(p, ..)| p.to_string_lossy().to_string()).collect();
        // Gone files go, but only under a folder that could be read this time: a folder
        // that is missing for a moment (or a source switched off) keeps what it had.
        let readable: Vec<String> = roots.iter().filter(|(r, _)| r.is_dir()).map(|(r, _)| r.to_string_lossy().to_string()).collect();
        self.files.retain(|k, _| seen.contains(k) || !readable.iter().any(|r| k.starts_with(r.as_str())));
        let mut todo: Vec<(PathBuf, u64, u64, Source)> = all
            .into_iter()
            .filter(|(p, size, mtime, _)| self.files.get(&*p.to_string_lossy()).map_or(true, |e| e.size != *size || e.mtime != *mtime))
            .collect();
        // Biggest first, so one huge file does not leave a single thread working at the end.
        todo.sort_by(|a, b| b.1.cmp(&a.1));
        let n = todo.len();
        if n == 0 {
            return 0;
        }
        let done = std::sync::atomic::AtomicUsize::new(0);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map_or(2, |c| c.get()).clamp(1, 6);
        let results: Vec<Vec<(String, FileEntry)>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let mut out = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((p, size, mtime, src)) = todo.get(i) else { break };
                            let groups = match src {
                                Source::Claude => read_file(p),
                                Source::Codex(account) => read_codex(p, account),
                            };
                            out.push((p.to_string_lossy().to_string(), FileEntry { size: *size, mtime: *mtime, groups }));
                            let d = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if d % 25 == 0 || d == n {
                                progress(d, n);
                            }
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        for (k, v) in results.into_iter().flatten() {
            self.files.insert(k, v);
        }
        n
    }

    pub fn groups(&self) -> impl Iterator<Item = &(Key, Tokens)> {
        self.files.values().flat_map(|f| f.groups.iter())
    }
}

/// Every transcript under `dir`: (path, size, modified seconds).
fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64, u64)>) {
    walk_checked(dir, out, &mut Vec::new());
}

/// `walk`, also listing the folders that could not be read.
fn walk_checked(dir: &Path, out: &mut Vec<(PathBuf, u64, u64)>, failed: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        failed.push(dir.to_path_buf());
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk_checked(&p, out, failed);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            if let Ok(m) = e.metadata() {
                let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                out.push((p, m.len(), mtime));
            }
        }
    }
}

/// `walk` with the project folders split over a few threads (thousands of files to list).
struct Walked {
    found: Vec<(PathBuf, u64, u64)>,
    /// Folders whose listing failed (what is under them is unknown, not gone).
    failed: Vec<PathBuf>,
    /// The root itself could not be listed: nothing is known.
    root_failed: bool,
}

fn walk_parallel(root: &Path) -> Walked {
    let mut found = Vec::new();
    let mut dirs = Vec::new();
    let mut root_failed = false;
    let listing = std::fs::read_dir(root);
    if listing.is_err() {
        root_failed = true;
    }
    if let Ok(rd) = listing {
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(m) = e.metadata() {
                    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                    found.push((p, m.len(), mtime));
                }
            }
        }
    }
    let threads = 4;
    type Part = (Vec<(PathBuf, u64, u64)>, Vec<PathBuf>);
    let parts: Vec<Part> = std::thread::scope(|s| {
        let dirs = &dirs;
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    let _bg = low_priority();
                    let mut out = Vec::new();
                    let mut failed = Vec::new();
                    for d in dirs.iter().skip(t).step_by(threads) {
                        walk_checked(d, &mut out, &mut failed);
                    }
                    (out, failed)
                })
            })
            .collect();
        // A thread that died leaves its folders unknown: treat the whole listing as failed.
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| (Vec::new(), vec![root.to_path_buf()]))).collect()
    });
    let mut failed = Vec::new();
    for (f, bad) in parts {
        found.extend(f);
        failed.extend(bad);
    }
    Walked { found, failed, root_failed }
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "isSidechain")]
    sidechain: Option<bool>,
    message: Option<Msg>,
}

#[derive(Deserialize)]
struct Msg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_creation: Option<CacheWrites>,
    speed: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CacheWrites {
    ephemeral_5m_input_tokens: u64,
    ephemeral_1h_input_tokens: u64,
}

/// One transcript's replies, each counted once, grouped.
fn read_file(path: &Path) -> Vec<(Key, Tokens)> {
    let Ok(f) = std::fs::File::open(path) else { return Vec::new() };
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut buf = Vec::new();
    // (message id, request id) -> (key, tokens); the copy with the most output wins.
    let mut replies: HashMap<(String, String), (Key, Tokens)> = HashMap::new();
    // A resumed or forked chat starts with a copy of its parent's lines, still under the
    // parent's session id; the parent's own file counts those. (A chat's file is named
    // after its session; helpers' files under "subagents" are not.)
    let own = path.parent().and_then(|d| d.file_name()).map_or(true, |n| n != "subagents");
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let has = |b: &[u8], k: &[u8]| b.windows(k.len()).any(|w| w == k);
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !has(&buf, b"\"usage\"") || !has(&buf, b"\"assistant\"") {
            continue;
        }
        let Some((id, k, t)) = reply(&buf) else { continue };
        if own && !k.session.is_empty() && k.session != stem {
            continue;
        }
        let e = replies.entry(id).or_insert_with(|| (k.clone(), t));
        if t.output >= e.1.output {
            *e = (k, t);
        }
    }
    let mut groups: BTreeMap<Key, Tokens> = BTreeMap::new();
    for (_, (k, t)) in replies {
        groups.entry(k).or_default().add(&t);
    }
    groups.into_iter().collect()
}

/// One assistant reply line: its identity, group and tokens.
fn reply(line: &[u8]) -> Option<((String, String), Key, Tokens)> {
    let l: Line = serde_json::from_slice(line).ok()?;
    if l.kind.as_deref() != Some("assistant") {
        return None;
    }
    let m = l.message?;
    let u = m.usage?;
    let model = m.model.unwrap_or_default();
    if model.is_empty() || model == "<synthetic>" {
        return None;
    }
    let (day, hour) = local_day(l.timestamp.as_deref()?)?;
    let (w5, w1h) = match &u.cache_creation {
        Some(c) if c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens > 0 => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
        _ => (u.cache_creation_input_tokens, 0),
    };
    let project = l
        .cwd
        .as_deref()
        .and_then(|c| Path::new(c.trim_end_matches(['\\', '/'])).file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "(no folder)".into());
    let key = Key { day, hour, session: l.session_id.unwrap_or_default(), model, project, fast: u.speed.as_deref() == Some("fast") };
    let t = Tokens { input: u.input_tokens, w5, w1h, read: u.cache_read_input_tokens, output: u.output_tokens, replies: 1 };
    Some(((m.id.unwrap_or_default(), l.request_id.unwrap_or_default()), key, t))
}

/// "2026-10-15T16:12:07.369Z" as the local date (yyyymmdd) and hour.
fn local_day(ts: &str) -> Option<(u32, u8)> {
    let secs = utc_secs(ts)?;
    let st = crate::time::local(secs);
    Some((st.wYear as u32 * 10000 + st.wMonth as u32 * 100 + st.wDay as u32, st.wHour as u8))
}

#[derive(Deserialize)]
struct CodexLine {
    timestamp: Option<String>,
    ordinal: Option<u64>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<serde_json::Value>,
}

/// One Codex rollout: each response's tokens once (by response id), with the model and
/// folder of its turn. A forked chat starts with a copy of its parent's history (the
/// records before `forked_from_ordinal_exclusive`): those count in the parent only.
/// OpenAI's input count includes the cached part, so cached input is moved to `read`.
fn read_codex(path: &Path, account: &str) -> Vec<(Key, Tokens)> {
    let Ok(f) = std::fs::File::open(path) else { return Vec::new() };
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut buf = Vec::new();
    let has = |b: &[u8], k: &[u8]| b.windows(k.len()).any(|w| w == k);
    let (mut model, mut project) = (String::from("codex"), String::from("(no folder)"));
    let mut copied_until = 0u64;
    let mut seen = std::collections::HashSet::new();
    let mut groups: BTreeMap<Key, Tokens> = BTreeMap::new();
    let session = format!("{DIRECT}{account}");
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Only the short records matter; skip the big ones before parsing.
        let head = &buf[..buf.len().min(160)];
        let kind = if has(head, b"\"token_usage_record\"") {
            2
        } else if has(head, b"\"turn_context\"") {
            1
        } else if has(head, b"\"session_meta\"") {
            0
        } else {
            continue;
        };
        let Ok(l) = serde_json::from_slice::<CodexLine>(&buf) else { continue };
        let Some(p) = l.payload else { continue };
        match kind {
            0 if l.kind.as_deref() == Some("session_meta") => {
                copied_until = p.get("forked_from_ordinal_exclusive").and_then(|v| v.as_u64()).unwrap_or(0);
            }
            1 if l.kind.as_deref() == Some("turn_context") => {
                if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                    model = m.to_string();
                }
                if let Some(c) = p.get("cwd").and_then(|v| v.as_str()) {
                    project = Path::new(c.trim_end_matches(['\\', '/'])).file_name().map_or_else(|| c.to_string(), |n| n.to_string_lossy().to_string());
                }
            }
            2 if l.kind.as_deref() == Some("token_usage_record") => {
                if l.ordinal.is_some_and(|o| o < copied_until) {
                    continue;
                }
                let id = p.get("response_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if !id.is_empty() && !seen.insert(id) {
                    continue;
                }
                let Some(u) = p.get("usage") else { continue };
                let n = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let Some((day, hour)) = l.timestamp.as_deref().and_then(local_day) else { continue };
                let cached = n("cached_input_tokens");
                let t = Tokens { input: n("input_tokens").saturating_sub(cached), w5: n("cache_write_input_tokens"), w1h: 0, read: cached, output: n("output_tokens"), replies: 1 };
                let key = Key { day, hour, session: session.clone(), model: model.clone(), project: project.clone(), fast: false };
                groups.entry(key).or_default().add(&t);
            }
            _ => {}
        }
    }
    groups.into_iter().collect()
}

pub fn utc_secs(ts: &str) -> Option<u64> {
    let n = |a: usize, b: usize| ts.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + h * 3600 + mi * 60 + s).ok()
}

// ---- top chats this week ----------------------------------------------------------
//
// A second, byte-offset index just for "which chats burned the most in the last 7 days
// and which are past 300k". Per transcript file it keeps the size, time, how far it has
// been read, hourly token totals by model family and the last turn's context size, so a
// refresh reads only the bytes appended since. Lines older than the window are skipped
// before parsing, and files untouched for longer than the window are never read.

/// Model families shown in a chat's mix.
pub const FAMILIES: [&str; 5] = ["Opus", "Sonnet", "Haiku", "Fable", "Other"];

/// Context size past which a chat should be handed off.
pub const HANDOFF_AT: u64 = 300_000;

/// Days the "top chats" window covers.
pub const WINDOW_DAYS: u64 = 7;

/// 0 Opus, 1 Sonnet, 2 Haiku, 3 Fable (and Mythos), 4 anything else.
pub fn family(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        0
    } else if m.contains("sonnet") {
        1
    } else if m.contains("haiku") {
        2
    } else if m.contains("fable") || m.contains("mythos") {
        3
    } else {
        4
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct Recent {
    id: String,
    hour: u32,
    fam: u8,
    total: u64,
    output: u64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct ChatFile {
    size: u64,
    mtime: u64,
    /// Bytes read so far (always the end of a complete line).
    offset: u64,
    /// Hash of the file's first bytes; 0 when not known yet. A mismatch means the file was replaced.
    head: u64,
    /// The chat this file belongs to (a helper's file belongs to its parent's chat).
    session: String,
    /// A chat's own file, not a helper's.
    main: bool,
    /// UTC hour (seconds / 3600) -> tokens by family.
    hours: BTreeMap<u32, [u64; 5]>,
    /// The latest copies of recent replies, so a streamed reply counts once.
    recent: Vec<Recent>,
    /// Context of the last turn (input + cache read + cache write) and when it was.
    ctx: u64,
    ctx_at: u64,
}

const CHAT_VERSION: u32 = 1;
const RECENT_KEEP: usize = 32;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ChatIndex {
    version: u32,
    files: HashMap<String, ChatFile>,
    /// Entries were added, changed or dropped since the last `take_dirty`.
    #[serde(skip)]
    dirty: bool,
}

/// Which entries survive a listing: seen ones, ones outside the root, and ones under a
/// folder that could not be listed. Nothing is dropped when the root itself failed.
fn keep_entry(key: &str, seen: &std::collections::HashSet<String>, root: &Path, failed: &[PathBuf], root_failed: bool) -> bool {
    if root_failed || seen.contains(key) {
        return true;
    }
    let k = Path::new(key);
    !k.starts_with(root) || failed.iter().any(|d| k.starts_with(d))
}

/// One chat's week.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TopChat {
    pub session: String,
    /// Tokens in the window (input, output, cache read and cache write).
    pub tokens: u64,
    /// The same by family (see `FAMILIES`).
    pub mix: [u64; 5],
    /// Current context size: the last turn's input plus cache.
    pub context: u64,
    /// When that last turn ran (unix seconds).
    pub last: u64,
}

impl TopChat {
    pub fn past_handoff(&self) -> bool {
        self.context > HANDOFF_AT
    }
}

fn fnv(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in b {
        h = (h ^ *x as u64).wrapping_mul(0x100_0000_01b3);
    }
    h | 1
}

fn head_hash(path: &Path) -> u64 {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return 0 };
    let mut b = [0u8; 256];
    let mut n = 0;
    while n < b.len() {
        match f.read(&mut b[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    // A file shorter than the sample is still growing: its start is not yet a fingerprint.
    if n < b.len() { 0 } else { fnv(&b) }
}

/// Drops back to normal when it goes out of scope.
pub struct Background(bool);

impl Drop for Background {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.0 {
            unsafe {
                use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END};
                SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
            }
        }
    }
}

/// Background mode for this thread (lower CPU and disk I/O priority) until the guard drops;
/// below-normal CPU priority if that mode is refused.
pub fn low_priority() -> Background {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_BELOW_NORMAL};
        if SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) != 0 {
            return Background(true);
        }
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    Background(false)
}

/// The chat a transcript belongs to, and whether it is the chat's own file.
fn chat_of(path: &Path) -> (String, bool) {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let parent = path.parent();
    if parent.and_then(|d| d.file_name()).is_some_and(|n| n == "subagents") {
        let s = parent.and_then(|d| d.parent()).and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or(stem);
        (s, false)
    } else {
        (stem, true)
    }
}

impl ChatFile {
    /// Read from `self.offset` to the last complete line, keeping lines newer than `cutoff` (seconds).
    fn read_more(&mut self, path: &Path, cutoff: u64) {
        use std::io::{Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(path) else { return };
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut r = BufReader::with_capacity(1 << 20, f);
        let mut buf = Vec::new();
        let has = |b: &[u8], k: &[u8]| b.windows(k.len()).any(|w| w == k);
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.last() != Some(&b'\n') {
                break; // a line still being written: read it next time
            }
            self.offset += buf.len() as u64;
            if !has(&buf, b"\"usage\"") || !has(&buf, b"\"assistant\"") {
                continue;
            }
            // Skip old lines without parsing them.
            const TS: &[u8] = b"\"timestamp\":\"";
            if let Some(i) = buf.windows(TS.len()).position(|w| w == TS) {
                let s = i + TS.len();
                if let Some(secs) = buf.get(s..s + 19).and_then(|b| std::str::from_utf8(b).ok()).and_then(utc_secs) {
                    if secs < cutoff {
                        continue;
                    }
                }
            }
            self.take(&buf);
        }
    }

    fn take(&mut self, line: &[u8]) {
        let Ok(l) = serde_json::from_slice::<Line>(line) else { return };
        if l.kind.as_deref() != Some("assistant") {
            return;
        }
        let Some(m) = l.message else { return };
        let Some(u) = m.usage else { return };
        let model = m.model.unwrap_or_default();
        if model.is_empty() || model == "<synthetic>" {
            return;
        }
        let Some(secs) = l.timestamp.as_deref().and_then(utc_secs) else { return };
        // A resumed chat starts with a copy of its parent's lines under the parent's id.
        if self.main && l.session_id.as_deref().is_some_and(|s| !s.is_empty() && s != self.session) {
            return;
        }
        let writes = match &u.cache_creation {
            Some(c) if c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens > 0 => c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens,
            _ => u.cache_creation_input_tokens,
        };
        let total = u.input_tokens + u.output_tokens + u.cache_read_input_tokens + writes;
        let (hour, fam) = ((secs / 3600) as u32, family(&model) as u8);
        let id = format!("{}\u{1}{}", m.id.unwrap_or_default(), l.request_id.unwrap_or_default());
        let sidechain = l.sidechain.unwrap_or(false);
        if self.main && !sidechain && secs >= self.ctx_at {
            self.ctx = u.input_tokens + u.cache_read_input_tokens + writes;
            self.ctx_at = secs;
        }
        let keyed = id.len() > 1;
        if keyed {
            if let Some(old) = self.recent.iter_mut().find(|r| r.id == id) {
                if u.output_tokens >= old.output {
                    if let Some(h) = self.hours.get_mut(&old.hour) {
                        h[old.fam as usize] = h[old.fam as usize].saturating_sub(old.total);
                    }
                    self.hours.entry(hour).or_default()[fam as usize] += total;
                    *old = Recent { id, hour, fam, total, output: u.output_tokens };
                }
                return;
            }
        }
        self.hours.entry(hour).or_default()[fam as usize] += total;
        if keyed {
            self.recent.push(Recent { id, hour, fam, total, output: u.output_tokens });
            if self.recent.len() > RECENT_KEEP {
                self.recent.remove(0);
            }
        }
    }

    fn prune(&mut self, cutoff: u64) {
        let keep = (cutoff / 3600) as u32;
        self.hours.retain(|h, _| *h >= keep);
        self.recent.retain(|r| r.hour >= keep);
    }
}

impl ChatIndex {
    pub fn load(path: &Path) -> ChatIndex {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<ChatIndex>(&b).ok())
            .filter(|i| i.version == CHAT_VERSION)
            .unwrap_or(ChatIndex { version: CHAT_VERSION, files: HashMap::new(), dirty: false })
    }

    pub fn save(&self, path: &Path) {
        if let Ok(text) = serde_json::to_vec(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// True once if the index changed (entries read, added or dropped) since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Bring the index up to date for the transcripts under `root`. Stops starting new
    /// files after `budget`. Returns (files read, files still waiting).
    pub fn update(&mut self, root: &Path, now: u64, budget: std::time::Duration) -> (usize, usize) {
        let window = WINDOW_DAYS * 86_400;
        // One day of slack either side of the window.
        let cutoff = now.saturating_sub(window + 86_400);
        let walked = walk_parallel(root);
        let found = walked.found;
        if self.version != CHAT_VERSION {
            self.version = CHAT_VERSION;
        }
        let seen: std::collections::HashSet<String> = found.iter().map(|(p, ..)| p.to_string_lossy().to_string()).collect();
        // Gone files go, but not under a folder that could not be listed this time.
        let before = self.files.len();
        self.files.retain(|k, _| keep_entry(k, &seen, root, &walked.failed, walked.root_failed));
        if self.files.len() != before {
            self.dirty = true;
        }
        // (path, key, size, mtime, entry to continue)
        let mut todo: Vec<(PathBuf, String, u64, u64, ChatFile)> = Vec::new();
        for (p, size, mtime) in found {
            let key = p.to_string_lossy().to_string();
            match self.files.get_mut(&key) {
                Some(e) if e.size == size && e.mtime == mtime => {}
                Some(e) => {
                    let head = if e.head == 0 { 0 } else { head_hash(&p) };
                    if size < e.offset || (e.head != 0 && head != e.head) {
                        // Truncated or replaced: start again from the top.
                        let (session, main) = chat_of(&p);
                        todo.push((p, key, size, mtime, ChatFile { session, main, ..ChatFile::default() }));
                    } else {
                        todo.push((p, key, size, mtime, e.clone()));
                    }
                }
                None if mtime < cutoff => {
                    // Too old to matter: remember it as read, so only later appends are read.
                    let (session, main) = chat_of(&p);
                    let head = head_hash(&p);
                    self.files.insert(key, ChatFile { size, mtime, offset: size, head, session, main, ..ChatFile::default() });
                    self.dirty = true;
                }
                None => {
                    let (session, main) = chat_of(&p);
                    todo.push((p, key, size, mtime, ChatFile { session, main, ..ChatFile::default() }));
                }
            }
        }
        // Biggest unread part first.
        todo.sort_by(|a, b| (b.2.saturating_sub(b.4.offset)).cmp(&a.2.saturating_sub(a.4.offset)));
        let n = todo.len();
        if n == 0 {
            return (0, 0);
        }
        let started = std::time::Instant::now();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map_or(2, |c| c.get()).clamp(1, 3);
        let results: Vec<Vec<(String, ChatFile)>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let _bg = low_priority();
                        let mut out = Vec::new();
                        loop {
                            if started.elapsed() > budget {
                                break;
                            }
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((p, key, size, mtime, entry)) = todo.get(i) else { break };
                            let mut e = entry.clone();
                            if e.head == 0 {
                                e.head = head_hash(p);
                            }
                            e.read_more(p, cutoff);
                            e.size = *size;
                            e.mtime = *mtime;
                            e.prune(cutoff);
                            out.push((key.clone(), e));
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        let mut read = 0;
        for (k, v) in results.into_iter().flatten() {
            self.files.insert(k, v);
            read += 1;
        }
        if read > 0 {
            self.dirty = true;
        }
        (read, n.saturating_sub(read))
    }

    /// The `n` chats with the most tokens in the last 7 days.
    pub fn top(&self, now: u64, n: usize) -> Vec<TopChat> {
        let from = (now.saturating_sub(WINDOW_DAYS * 86_400) / 3600) as u32;
        let mut by: HashMap<&str, TopChat> = HashMap::new();
        for f in self.files.values() {
            let mut mix = [0u64; 5];
            for (_, h) in f.hours.range(from..) {
                for i in 0..5 {
                    mix[i] += h[i];
                }
            }
            let tokens: u64 = mix.iter().sum();
            if tokens == 0 && !(f.main && f.ctx_at >= now.saturating_sub(WINDOW_DAYS * 86_400)) {
                continue;
            }
            let e = by.entry(f.session.as_str()).or_insert_with(|| TopChat { session: f.session.clone(), ..TopChat::default() });
            e.tokens += tokens;
            for i in 0..5 {
                e.mix[i] += mix[i];
            }
            if f.main && f.ctx_at >= e.last {
                e.context = f.ctx;
                e.last = f.ctx_at;
            }
        }
        let mut v: Vec<TopChat> = by.into_values().filter(|c| c.tokens > 0).collect();
        v.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.session.cmp(&b.session)));
        v.truncate(n);
        v
    }
}

// ---- totals for the dashboard ---------------------------------------------------

#[derive(Clone, Copy, Default, Debug)]
pub struct Agg {
    pub tokens: Tokens,
    pub cost: f64,
}

impl Agg {
    fn add(&mut self, t: &Tokens, c: f64) {
        self.tokens.add(t);
        self.cost += c;
    }
}

#[derive(Debug)]
pub struct Summary {
    pub total: Agg,
    /// Day (yyyymmdd) -> account -> totals.
    pub by_day: BTreeMap<u32, BTreeMap<String, Agg>>,
    pub by_account: BTreeMap<String, Agg>,
    pub by_model: BTreeMap<String, Agg>,
    pub by_project: BTreeMap<String, Agg>,
    /// Day (yyyymmdd) -> Claude tokens by model family (see `FAMILIES`); Codex (direct) sessions are left out.
    pub by_day_family: BTreeMap<u32, [u64; 5]>,
    /// Tokens on models with no known price (left out of cost).
    pub unpriced: u64,
    /// Weekday (0 = Sunday) x hour.
    pub by_hour: [[Agg; 24]; 7],
}

impl Default for Summary {
    fn default() -> Summary {
        Summary {
            total: Agg::default(),
            by_day: BTreeMap::new(),
            by_account: BTreeMap::new(),
            by_model: BTreeMap::new(),
            by_project: BTreeMap::new(),
            by_day_family: BTreeMap::new(),
            unpriced: 0,
            by_hour: [[Agg::default(); 24]; 7],
        }
    }
}

/// Name for transcripts that match no account's chats.
pub const OTHER: &str = "Terminal and other";

/// Totals from `from_day` (yyyymmdd) on, for the accounts `keep` allows.
pub fn summarize(index: &Index, account_of: &HashMap<String, String>, from_day: u32, keep: &dyn Fn(&str) -> bool) -> Summary {
    let mut s = Summary::default();
    for (k, t) in index.groups() {
        if k.day < from_day {
            continue;
        }
        let account = match k.session.strip_prefix(DIRECT) {
            Some(direct) => direct,
            None => account_of.get(&k.session).map_or(OTHER, String::as_str),
        };
        if !keep(account) {
            continue;
        }
        let c = cost(&k.model, k.fast, t);
        if c.is_none() {
            s.unpriced += t.total();
        }
        let c = c.unwrap_or(0.0);
        s.total.add(t, c);
        s.by_day.entry(k.day).or_default().entry(account.to_string()).or_default().add(t, c);
        s.by_account.entry(account.to_string()).or_default().add(t, c);
        s.by_model.entry(model_label(&k.model)).or_default().add(t, c);
        s.by_project.entry(k.project.clone()).or_default().add(t, c);
        // Direct (Codex) sessions are not Claude tokens: leave them out of the family shares.
        if !k.session.starts_with(DIRECT) {
            s.by_day_family.entry(k.day).or_default()[family(&k.model)] += t.total();
        }
        s.by_hour[weekday(k.day)][(k.hour as usize).min(23)].add(t, c);
    }
    s
}

// ---- model share and the last week ---------------------------------------------

/// Each family's share (0..1) of a day's tokens; None for a day with no tokens.
pub fn day_shares(by_day_family: &BTreeMap<u32, [u64; 5]>, day: u32) -> Option<[f64; 5]> {
    let f = by_day_family.get(&day)?;
    let total: u64 = f.iter().sum();
    if total == 0 {
        return None;
    }
    let mut out = [0.0; 5];
    for i in 0..5 {
        out[i] = f[i] as f64 / total as f64;
    }
    Some(out)
}

/// The Opus share (percent, 1 to 100) above which the model-share strip marks a day red.
/// Off unless `HIVE_OPUS_ALERT` holds a number such as `60` or `60%`.
pub fn parse_alert(v: Option<&str>) -> Option<f64> {
    let n: f64 = v?.trim().trim_end_matches('%').trim().parse().ok()?;
    (n.is_finite() && (1.0..=100.0).contains(&n)).then_some(n)
}

pub fn opus_alert() -> Option<f64> {
    parse_alert(std::env::var("HIVE_OPUS_ALERT").ok().as_deref())
}

/// True when a day's Opus share is at or over the threshold.
pub fn opus_over(shares: &[f64; 5], alert: Option<f64>) -> bool {
    alert.is_some_and(|a| shares[0] * 100.0 >= a)
}

/// Tokens by account and model family from `from_day` (yyyymmdd) on, for every account.
pub fn week_by_account(index: &Index, account_of: &HashMap<String, String>, from_day: u32) -> BTreeMap<String, [u64; 5]> {
    let mut out: BTreeMap<String, [u64; 5]> = BTreeMap::new();
    for (k, t) in index.groups() {
        if k.day < from_day {
            continue;
        }
        let account = match k.session.strip_prefix(DIRECT) {
            Some(direct) => direct,
            None => account_of.get(&k.session).map_or(OTHER, String::as_str),
        };
        let slot = match out.get_mut(account) {
            Some(s) => s,
            None => out.entry(account.to_string()).or_default(),
        };
        slot[family(&k.model)] += t.total();
    }
    out
}

/// Chats whose last turn left more than 300k of context (a missed hand-off).
pub fn missed_handoffs(chats: &[TopChat]) -> usize {
    chats.iter().filter(|c| c.past_handoff()).count()
}

/// Day of the week (0 = Sunday) of a yyyymmdd date.
pub fn weekday(day: u32) -> usize {
    let (y, m, d) = ((day / 10000) as i64, ((day / 100) % 100) as i64, (day % 100) as i64);
    let ts = format!("{y:04}-{m:02}-{d:02}T12:00:00Z");
    utc_secs(&ts).map_or(0, |s| ((s / 86_400 + 4) % 7) as usize)
}

// ---- when a limit will be reached -------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Forecast {
    /// Already at 100%.
    Reached,
    /// At this pace, 100% at this time (before the reset).
    At(u64),
    /// At this pace it lasts until the reset.
    Lasts,
    /// Not enough samples to tell.
    Unknown,
}

/// Where the use in `samples` ((seconds, percent), oldest first) is heading: the pace
/// over the newest `window` seconds, projected to 100% and compared with `reset`.
pub fn forecast(samples: &[(u64, u8)], window: u64, reset: Option<u64>) -> Forecast {
    let Some(&(t1, p1)) = samples.last() else { return Forecast::Unknown };
    if p1 >= 100 {
        return Forecast::Reached;
    }
    // From the oldest sample in the window, but not across a reset (a drop in %).
    let mut start = (t1, p1);
    for &(t, p) in samples.iter().rev().skip(1) {
        if t1.saturating_sub(t) > window || p > start.1 {
            break;
        }
        start = (t, p);
    }
    let (t0, p0) = start;
    let dt = t1.saturating_sub(t0);
    if dt < 20 * 60 {
        return Forecast::Unknown;
    }
    if p1 <= p0 {
        return Forecast::Lasts;
    }
    let per_sec = (p1 - p0) as f64 / dt as f64;
    let at = t1 + ((100 - p1) as f64 / per_sec) as u64;
    match reset {
        Some(r) if at >= r => Forecast::Lasts,
        _ => Forecast::At(at),
    }
}

// ---- limit log ---------------------------------------------------------------

/// The usage levels worth recording, in percent of a window.
pub const LEVELS: [u8; 3] = [80, 95, 100];
/// Lines older than this are dropped from limits.jsonl (at most once a day).
const LIMIT_KEEP_SECS: u64 = 90 * 86_400;
/// A sample this far below the one before it means the window reset.
const RESET_DROP: u8 = 10;

/// One account passing one level in one window: a line of limits.jsonl.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Crossing {
    /// When the sample showing it was taken (seconds).
    pub t: u64,
    pub account: String,
    /// "5h" or "week".
    pub window: String,
    /// 80, 95 or 100.
    pub level: u8,
    /// The sample's own percent.
    pub pct: u8,
    /// When the window resets (seconds), when known.
    pub reset: Option<u64>,
}

/// Where one account's window stood after the last sample read.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct WinState {
    t: u64,
    pct: u8,
    /// Highest level already logged in this window.
    lvl: u8,
    /// When this window resets, as last seen (only the newest sample of a read knows it).
    #[serde(default)]
    reset: Option<u64>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct LimitState {
    /// "<account>|5h" or "<account>|week".
    windows: BTreeMap<String, WinState>,
    /// Seconds of the last prune of limits.jsonl.
    last_prune: u64,
}

/// Two reset times this close belong to the same window (a reset time is an estimate).
const RESET_SAME: u64 = 3600;

/// Feed one sample to a window of `span` seconds; returns the levels it newly passed
/// (upward, once per window). A sample not newer than the last one read is ignored.
/// A new window starts when the reset time changed, when the sample is a whole span
/// after the last one, or (reset times not telling) when the percent fell 10+ points.
/// Equal reset times mean the same window even through a dip.
fn step(w: &mut WinState, t: u64, pct: u8, span: u64, reset: Option<u64>) -> Vec<u8> {
    if w.t > 0 && t <= w.t {
        return Vec::new();
    }
    let both = reset.zip(w.reset);
    let same = both.is_some_and(|(a, b)| a.abs_diff(b) <= RESET_SAME);
    let changed = both.is_some_and(|(a, b)| a.abs_diff(b) > RESET_SAME);
    let gap = w.t > 0 && t - w.t >= span;
    if changed || gap || (!same && pct.saturating_add(RESET_DROP) <= w.pct) {
        w.lvl = 0; // a new window
    }
    if reset.is_some() {
        w.reset = reset;
    }
    let mut out = Vec::new();
    for l in LEVELS {
        if pct >= l && l > w.lvl {
            out.push(l);
            w.lvl = l;
        }
    }
    w.t = t;
    w.pct = pct;
    out
}

/// One account's samples for [`record_limits`]: (seconds, 5-hour %, weekly %), oldest
/// first, plus the current resets.
pub struct LimitInput {
    pub account: String,
    pub samples: Vec<(u64, u8, u8)>,
    pub five_reset: Option<u64>,
    pub week_reset: Option<u64>,
}

static LIMIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Log every new crossing in `inputs` to `<dir>/limits.jsonl` and return them (oldest
/// first). Which samples were already read is kept in `<dir>/limits-state.json`, so
/// this can run as often as it likes; samples from before it first ran are read too,
/// with their own times. Also drops lines over 90 days old, at most once a day.
pub fn record_limits(dir: &Path, inputs: &[LimitInput], now: u64) -> Vec<Crossing> {
    use std::io::Write;
    let _guard = LIMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state_path = dir.join("limits-state.json");
    let log_path = dir.join("limits.jsonl");
    let loaded: Option<LimitState> = std::fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str(&t).ok());
    // No usable state (first run, or a damaged file): samples already covered by the log
    // rebuild the state but are not logged again.
    let floor: HashMap<String, u64> = if loaded.is_some() { HashMap::new() } else { newest_logged(&log_path) };
    let mut state = loaded.unwrap_or_default();
    let mut found = Vec::new();
    for inp in inputs {
        let last = inp.samples.last().map(|s| s.0);
        for (t, fh, sd) in &inp.samples {
            // Only the newest sample's reset is known to belong to it.
            let newest = Some(*t) == last;
            for (key, pct, reset, span) in [("5h", *fh, inp.five_reset, 5 * 3600), ("week", *sd, inp.week_reset, 7 * 86_400)] {
                let w = state.windows.entry(format!("{}|{key}", inp.account)).or_default();
                for level in step(w, *t, pct, span, if newest { reset } else { None }) {
                    if now.saturating_sub(*t) > LIMIT_KEEP_SECS || floor.get(&format!("{}|{key}", inp.account)).is_some_and(|f| *t <= *f) {
                        continue; // old enough to be pruned, or already in the log
                    }
                    found.push(Crossing {
                        t: *t,
                        account: inp.account.clone(),
                        window: key.into(),
                        level,
                        pct,
                        // Only the newest sample's window is the one the reset belongs to.
                        reset: if newest { reset } else { None },
                    });
                }
            }
        }
    }
    found.sort_by_key(|c| c.t);
    if !found.is_empty() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            for c in &found {
                if let Ok(line) = serde_json::to_string(c) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }
    let mut dirty = !found.is_empty() || !state_path.exists();
    if now.saturating_sub(state.last_prune) >= 86_400 {
        prune_limits(&log_path, now);
        state.last_prune = now;
        dirty = true;
    }
    if dirty {
        if let Ok(text) = serde_json::to_string(&state) {
            // Written beside and renamed over, so a crash never leaves half a file.
            let tmp = dir.join("limits-state.json.tmp");
            if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &state_path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    found
}

/// [`record_limits`], then one `log` line for each crossing from the last hour (older
/// ones, found when catching up, only go to the file). Shared by the tray and the dashboard.
pub fn record_and_log(dir: &Path, inputs: &[LimitInput], now: u64, log: &dyn Fn(&str)) -> Vec<Crossing> {
    let found = record_limits(dir, inputs, now);
    for c in found.iter().filter(|c| now.saturating_sub(c.t) <= 3600) {
        let reset = c.reset.map_or("unknown".to_string(), |r| format!("{r}"));
        log(&format!("limit: {} passed {}% of its {} window (sample {}%, resets {})", c.account, c.level, if c.window == "5h" { "5-hour" } else { "weekly" }, c.pct, reset));
    }
    found
}

/// The newest logged time per "<account>|<window>" in limits.jsonl.
fn newest_logged(path: &Path) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else { return out };
    for c in text.lines().filter_map(|l| serde_json::from_str::<Crossing>(l).ok()) {
        let e = out.entry(format!("{}|{}", c.account, c.window)).or_default();
        *e = (*e).max(c.t);
    }
    out
}

/// Rewrite the log without lines older than 90 days.
fn prune_limits(path: &Path, now: u64) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let cut = now.saturating_sub(LIMIT_KEEP_SECS);
    let kept: Vec<&str> = text.lines().filter(|l| serde_json::from_str::<Crossing>(l).map_or(false, |c| c.t >= cut)).collect();
    if kept.len() == text.lines().count() {
        return;
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let _ = std::fs::write(path, out);
}

/// Per account: how many times it passed 95% (either window) in the last 7 days, and
/// when it last did (in the whole log).
pub fn past95(dir: &Path, now: u64) -> HashMap<String, (u32, Option<u64>)> {
    let mut out: HashMap<String, (u32, Option<u64>)> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(dir.join("limits.jsonl")) else { return out };
    for c in text.lines().filter_map(|l| serde_json::from_str::<Crossing>(l).ok()).filter(|c| c.level == 95) {
        let e = out.entry(c.account).or_default();
        if now.saturating_sub(c.t) <= 7 * 86_400 {
            e.0 += 1;
        }
        e.1 = e.1.max(Some(c.t));
    }
    out
}

/// The dashboard's line for one account; `when` formats the time of the last crossing.
pub fn past95_text(count: u32, last: Option<u64>, when: &dyn Fn(u64) -> String) -> String {
    match (count, last) {
        (0, None) => "95%+: none logged".into(),
        (0, Some(t)) => format!("95%+: none in 7 days, last {}", when(t)),
        (n, Some(t)) => format!("95%+: {n} in 7 days, last {}", when(t)),
        (n, None) => format!("95%+: {n} in 7 days"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hive-limits-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn five(account: &str, pcts: &[u8], start: u64) -> LimitInput {
        LimitInput { account: account.into(), samples: pcts.iter().enumerate().map(|(i, p)| (start + i as u64 * 900, *p, 10)).collect(), five_reset: Some(start + 99_999), week_reset: None }
    }

    #[test]
    fn crossings_are_logged_once_per_window_going_up() {
        let dir = limit_dir("ac1");
        let now = 1_800_000_000;
        let got = record_limits(&dir, &[five("Personal", &[70, 82, 85, 96, 40, 81], now - 10_000)], now);
        let levels: Vec<u8> = got.iter().map(|c| c.level).collect();
        assert_eq!(levels, vec![80, 95, 80], "{got:?}");
        assert!(got.iter().all(|c| c.window == "5h" && c.account == "Personal"));
        // Reading the same samples again adds nothing; the file holds the three lines.
        assert!(record_limits(&dir, &[five("Personal", &[70, 82, 85, 96, 40, 81], now - 10_000)], now).is_empty());
        assert_eq!(std::fs::read_to_string(dir.join("limits.jsonl")).unwrap().lines().count(), 3);
        // A jump past several levels logs each; the week is tracked apart from the 5 hours.
        let mut inp = five("Team", &[50, 100], now - 5_000);
        inp.samples[1].2 = 96;
        let got = record_limits(&dir, &[inp], now);
        let seen: Vec<(String, u8)> = got.iter().map(|c| (c.window.clone(), c.level)).collect();
        assert!(seen.contains(&("5h".into(), 80)) && seen.contains(&("5h".into(), 95)) && seen.contains(&("5h".into(), 100)));
        assert!(seen.contains(&("week".into(), 80)) && seen.contains(&("week".into(), 95)) && !seen.contains(&("week".into(), 100)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_crossings_reach_the_log_callback_and_old_ones_do_not() {
        let dir = limit_dir("log");
        let now = 1_800_000_000;
        let lines = std::cell::RefCell::new(Vec::new());
        let log = |m: &str| lines.borrow_mut().push(m.to_string());
        // 96% an hour and a half ago (file only), a reset, then 97% ten minutes ago.
        let inp = LimitInput { account: "A".into(), samples: vec![(now - 5_400, 96, 1), (now - 4_000, 40, 1), (now - 600, 97, 1)], five_reset: None, week_reset: None };
        let got = record_and_log(&dir, &[inp], now, &log);
        assert_eq!(got.len(), 4, "80 and 95, twice: {got:?}");
        let l = lines.borrow();
        assert_eq!(l.len(), 2, "{l:?}");
        assert!(l.iter().all(|m| m.starts_with("limit: A passed ") && m.contains("5-hour")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_reset_or_a_long_gap_starts_a_new_window_and_a_dip_with_the_same_reset_does_not() {
        // 96, then 88 a long gap later: a new window, so 88 logs 80 again (95 is not reached).
        let dir = limit_dir("gap");
        let now = 1_800_000_000;
        let inp = LimitInput { account: "A".into(), samples: vec![(now - 40_000, 96, 1), (now - 1_000, 88, 1)], five_reset: None, week_reset: None };
        let levels: Vec<u8> = record_limits(&dir, &[inp], now).iter().map(|c| c.level).collect();
        assert_eq!(levels, vec![80, 95, 80]);
        let _ = std::fs::remove_dir_all(&dir);
        // The reset time moved between two reads, with the percent barely lower: new window.
        let dir = limit_dir("moved");
        let mk = |t: u64, pct: u8, reset: u64| LimitInput { account: "A".into(), samples: vec![(t, pct, 1)], five_reset: Some(reset), week_reset: None };
        assert_eq!(record_limits(&dir, &[mk(now - 3_000, 96, now + 2_000)], now).len(), 2);
        assert!(record_limits(&dir, &[mk(now - 2_000, 97, now + 2_100)], now).is_empty(), "same window, same reset within the tolerance");
        let got = record_limits(&dir, &[mk(now - 1_000, 90, now + 18_000)], now);
        assert_eq!(got.iter().map(|c| c.level).collect::<Vec<_>>(), vec![80], "a new reset time is a new window");
        let _ = std::fs::remove_dir_all(&dir);
        // A 12-point dip with an unchanged reset is the same window: 95 is not logged twice.
        let dir = limit_dir("dip");
        assert_eq!(record_limits(&dir, &[mk(now - 3_000, 96, now + 2_000)], now).len(), 2);
        assert!(record_limits(&dir, &[mk(now - 2_000, 84, now + 2_000)], now).is_empty());
        assert!(record_limits(&dir, &[mk(now - 1_000, 97, now + 2_000)], now).is_empty(), "no second 95 after a dip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_or_damaged_state_file_does_not_log_old_crossings_again() {
        let dir = limit_dir("lost");
        let now = 1_800_000_000;
        let inp = || five("A", &[70, 82, 96, 40, 81], now - 10_000);
        assert_eq!(record_limits(&dir, &[inp()], now).len(), 3);
        for damage in [None, Some("{\"windows\": {\"A|5")] {
            let st = dir.join("limits-state.json");
            match damage {
                None => std::fs::remove_file(&st).unwrap(),
                Some(text) => std::fs::write(&st, text).unwrap(),
            }
            assert!(record_limits(&dir, &[inp()], now).is_empty(), "old samples are not logged again");
            assert_eq!(std::fs::read_to_string(dir.join("limits.jsonl")).unwrap().lines().count(), 3);
            // The rebuilt state still catches a new crossing.
            let more = LimitInput { account: "A".into(), samples: inp().samples.into_iter().chain([(now, 97, 10)]).collect(), five_reset: None, week_reset: None };
            let got = record_limits(&dir, &[more], now);
            assert_eq!(got.iter().map(|c| c.level).collect::<Vec<_>>(), vec![95]);
            // Put the log back to three lines for the next round.
            let text = std::fs::read_to_string(dir.join("limits.jsonl")).unwrap();
            std::fs::write(dir.join("limits.jsonl"), text.lines().take(3).collect::<Vec<_>>().join("\n") + "\n").unwrap();
        }
        assert!(!dir.join("limits-state.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reset_time_goes_only_on_the_newest_sample() {
        let dir = limit_dir("reset");
        let now = 1_800_000_000;
        let got = record_limits(&dir, &[five("A", &[85, 50, 90], now - 3_000)], now);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].reset, None);
        assert_eq!(got[1].reset, Some(now - 3_000 + 99_999));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_90_day_prune_runs_once_a_day_and_keeps_newer_lines() {
        let dir = limit_dir("ac2");
        let now = 1_800_000_000;
        let day = 86_400;
        let line = |t: u64| serde_json::to_string(&Crossing { t, account: "A".into(), window: "5h".into(), level: 95, pct: 96, reset: None }).unwrap();
        let path = dir.join("limits.jsonl");
        std::fs::write(&path, format!("{}\n{}\n{}\n", line(now - 100 * day), line(now - 89 * day), line(now - 5 * day))).unwrap();
        record_limits(&dir, &[], now);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2, "the 100-day-old line goes, the 89-day-old stays");
        // Within the same day nothing is pruned, even if a line has gone stale since.
        std::fs::write(&path, format!("{}\n{}\n", line(now - 100 * day), line(now - day))).unwrap();
        record_limits(&dir, &[], now + 3_600);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2, "no second prune inside a day");
        record_limits(&dir, &[], now + day + 10);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1, "pruned again a day later");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seven_day_counts_and_their_text() {
        let dir = limit_dir("ac3");
        let now = 1_800_000_000;
        let day = 86_400;
        let line = |a: &str, t: u64, level: u8| serde_json::to_string(&Crossing { t, account: a.into(), window: "5h".into(), level, pct: level, reset: None }).unwrap();
        let text = [line("A", now - 10 * day, 95), line("A", now - 2 * day, 95), line("A", now - day, 95), line("A", now - day, 100), line("B", now - 3 * day, 80)].join("\n");
        std::fs::write(dir.join("limits.jsonl"), text + "\n").unwrap();
        let c = past95(&dir, now);
        assert_eq!(c["A"], (2, Some(now - day)));
        assert!(!c.contains_key("B"));
        let when = |t: u64| format!("t{}", now - t);
        assert_eq!(past95_text(2, Some(now - day), &when), format!("95%+: 2 in 7 days, last t{day}"));
        assert_eq!(past95_text(0, Some(now - 10 * day), &when), format!("95%+: none in 7 days, last t{}", 10 * day));
        assert_eq!(past95_text(0, None, &when), "95%+: none logged");
        let _ = std::fs::remove_dir_all(&dir);
    }


    #[test]
    fn a_resumed_chats_copied_lines_are_not_counted_again() {
        let dir = std::env::temp_dir().join(format!("sb-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let line = |session: &str, id: &str| format!(r#"{{"type":"assistant","timestamp":"2026-10-15T16:00:00.000Z","sessionId":"{session}","requestId":"r{id}","message":{{"id":"m{id}","model":"claude-opus-5-5","usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#);
        let file = dir.join("child.jsonl");
        std::fs::write(&file, format!("{}\n{}\n", line("parent", "1"), line("child", "2"))).unwrap();
        let replies: u64 = read_file(&file).iter().map(|(_, t)| t.replies).sum();
        assert_eq!(replies, 1, "the line copied from the parent is the parent's");
        let sub = dir.join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let helper = sub.join("agent-1.jsonl");
        std::fs::write(&helper, format!("{}\n", line("parent", "3"))).unwrap();
        assert_eq!(read_file(&helper).len(), 1, "a helper's own file keeps its lines");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prices_pick_the_longest_prefix() {
        assert_eq!(price("claude-opus-5-5").unwrap().0, 4.0);
        assert_eq!(price("claude-opus-5").unwrap().0, 5.0);
        assert_eq!(price("claude-sonnet-4-5-20250929").unwrap().0, 3.0);
        assert_eq!(price("claude-fable-5-1").unwrap().0, 10.0);
        assert!(price("gpt-6").is_none());
        assert!(price("gpt-daybreak-blue-latest").is_none());
        assert_eq!(price("gpt-6.2-sol").unwrap(), (2.0, 10.0, 0.10));
        assert_eq!(price("gpt-6-sol").unwrap().2, 0.20);
        assert_eq!(price("gpt-6.2-astra").unwrap().0, 8.0);
        assert_eq!(price("gpt-6-luna").unwrap().1, 0.50);
        // Codex: 1M uncached input + 1M cached + 1M output on gpt-6.2-sol = 2 + 0.1 + 10.
        let c = Tokens { input: 1_000_000, w5: 0, w1h: 0, read: 1_000_000, output: 1_000_000, replies: 1 };
        assert!((cost("gpt-6.2-sol", false, &c).unwrap() - 12.1).abs() < 1e-9);
        assert!(cost("gpt-daybreak-blue-latest", false, &c).is_none());
        let t = Tokens { input: 1_000_000, w5: 0, w1h: 1_000_000, read: 1_000_000, output: 1_000_000, replies: 1 };
        // 4 + 8 (1-hour write, 2x) + 0.2 + 20
        assert!((cost("claude-opus-5-5", false, &t).unwrap() - 32.2).abs() < 1e-9);
        assert!((cost("claude-opus-5-5", true, &t).unwrap() - 64.4).abs() < 1e-9);
    }

    #[test]
    fn model_labels_read_well() {
        assert_eq!(model_label("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_label("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_label("claude-haiku-5-5"), "Haiku 5.5");
        assert_eq!(model_label("gpt-6.1-sol"), "GPT-6.1 Sol");
        assert_eq!(model_label("codex-auto-review"), "Codex auto review");
    }

    #[test]
    fn utc_times_parse() {
        assert_eq!(utc_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(utc_secs("2026-10-15T16:12:07.369Z"), Some(1_792_080_727));
        assert_eq!(utc_secs("bad"), None);
    }

    #[test]
    fn a_streamed_reply_counts_once_as_its_last_copy() {
        let dir = std::env::temp_dir().join(format!("ca-stats-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("S.jsonl"); // a chat file is named after its session
        let line = |out: u64, id: &str| {
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"2026-10-15T16:12:07.369Z\",\"sessionId\":\"S\",\"cwd\":\"F:\\\\dev\\\\proj\",\"requestId\":\"r{id}\",\"message\":{{\"id\":\"m{id}\",\"model\":\"claude-opus-5-5\",\"content\":[{{\"type\":\"text\",\"text\":\"the word usage\"}}],\"usage\":{{\"input_tokens\":2,\"output_tokens\":{out},\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":50,\"cache_creation\":{{\"ephemeral_5m_input_tokens\":0,\"ephemeral_1h_input_tokens\":50}}}}}}}}\n"
            )
        };
        let text = [line(5, "1"), line(9, "1"), line(7, "1"), line(3, "2"), "{\"type\":\"user\",\"message\":{\"content\":\"\\\"usage\\\" assistant\"}}\n".into()].concat();
        std::fs::write(&p, text).unwrap();
        let g = read_file(&p);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(g.len(), 1);
        let (k, t) = &g[0];
        assert_eq!((k.project.as_str(), k.session.as_str(), k.model.as_str()), ("proj", "S", "claude-opus-5-5"));
        assert_eq!((t.replies, t.output, t.input, t.read, t.w1h, t.w5), (2, 12, 4, 200, 100, 0));
    }

    fn grp(day: u32, session: &str, model: &str, read: u64) -> (Key, Tokens) {
        (Key { day, hour: 10, session: session.into(), model: model.into(), project: "p".into(), fast: false }, Tokens { read, replies: 1, ..Tokens::default() })
    }

    fn share_fixture() -> (Index, HashMap<String, String>) {
        let groups = vec![
            grp(20261116, "s1", "claude-opus-5-5", 600),
            grp(20261116, "s1", "claude-sonnet-5-5", 400),
            grp(20261117, "s2", "claude-opus-5-5", 100),
            grp(20261117, "s2", "claude-haiku-5-5", 100),
            grp(20261117, "s2", "claude-fable-5-1", 200),
            grp(20261117, &format!("{DIRECT}Codex"), "gpt-6.1-sol", 600),
            grp(20261110, "s2", "claude-opus-5-5", 9_999),
        ];
        let mut files = HashMap::new();
        files.insert("f".to_string(), FileEntry { size: 1, mtime: 1, groups });
        let map: HashMap<String, String> = [("s1", "Personal"), ("s2", "Team")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        (Index { version: INDEX_VERSION, files }, map)
    }

    #[test]
    fn model_share_per_day_from_existing_index_data() {
        let (idx, map) = share_fixture();
        let s = summarize(&idx, &map, 20261111, &|_| true);
        assert!(!s.by_day_family.contains_key(&20261110), "days before the range are left out");
        let d1 = day_shares(&s.by_day_family, 20261116).unwrap();
        assert!((d1[0] - 0.6).abs() < 1e-9 && (d1[1] - 0.4).abs() < 1e-9 && d1[2] == 0.0, "{d1:?}");
        let d2 = day_shares(&s.by_day_family, 20261117).unwrap();
        // Opus 100, Haiku 100, Fable 200 of 400; the 600 Codex tokens are not Claude tokens.
        assert_eq!(d2, [0.25, 0.0, 0.25, 0.5, 0.0]);
        assert!((d2.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(day_shares(&s.by_day_family, 20261118).is_none());
        // The account filter narrows the shares like every other total.
        let only = summarize(&idx, &map, 20261111, &|a| a == "Personal");
        assert_eq!(only.by_day_family.len(), 1);
    }

    #[test]
    fn last_week_by_account_and_model_and_missed_handoffs() {
        let (idx, map) = share_fixture();
        let w = week_by_account(&idx, &map, 20261111);
        assert_eq!(w["Personal"], [600, 400, 0, 0, 0]);
        assert_eq!(w["Team"], [100, 0, 100, 200, 0]);
        assert_eq!(w["Codex"], [0, 0, 0, 0, 600], "a direct (Codex) session is its own account");
        assert_eq!(w.len(), 3);
        // Unknown sessions land in "Terminal and other".
        let mut idx2 = idx;
        idx2.files.get_mut("f").unwrap().groups.push(grp(20261117, "zz", "claude-opus-5-5", 50));
        assert_eq!(week_by_account(&idx2, &map, 20261111)[OTHER][0], 50);
        let chat = |s: &str, ctx: u64| TopChat { session: s.into(), tokens: 1, context: ctx, ..TopChat::default() };
        let chats = vec![chat("a", 400_000), chat("b", 300_000), chat("c", 300_001), chat("d", 0)];
        assert_eq!(missed_handoffs(&chats), 2, "past 300k means over, not at, the line");
    }

    #[test]
    fn opus_alert_is_off_by_default_and_marks_days_at_or_over() {
        assert_eq!(parse_alert(None), None);
        assert_eq!(parse_alert(Some("")), None);
        assert_eq!(parse_alert(Some("abc")), None);
        assert_eq!(parse_alert(Some("0")), None);
        assert_eq!(parse_alert(Some("101")), None);
        assert_eq!(parse_alert(Some("NaN")), None);
        assert_eq!(parse_alert(Some("60")), Some(60.0));
        assert_eq!(parse_alert(Some(" 62.5% ")), Some(62.5));
        let (idx, map) = share_fixture();
        let s = summarize(&idx, &map, 20261111, &|_| true);
        let red: Vec<u32> = s.by_day_family.keys().copied().filter(|d| opus_over(&day_shares(&s.by_day_family, *d).unwrap(), Some(60.0))).collect();
        assert_eq!(red, vec![20261116], "Opus 60% is at the line; 25% is not");
        // A heavy Codex day must not dilute the Opus share: Opus 700, Sonnet 300, Codex 2000.
        let mut idx2 = idx;
        let g = &mut idx2.files.get_mut("f").unwrap().groups;
        g.push(grp(20261120, "s1", "claude-opus-5-5", 700));
        g.push(grp(20261120, "s1", "claude-sonnet-5-5", 300));
        g.push(grp(20261120, &format!("{DIRECT}Codex"), "gpt-6.1-sol", 2000));
        let s2 = summarize(&idx2, &map, 20261111, &|_| true);
        let sh = day_shares(&s2.by_day_family, 20261120).unwrap();
        assert!((sh[0] - 0.7).abs() < 1e-9 && sh[4] == 0.0, "{sh:?}");
        assert!(opus_over(&sh, Some(60.0)));
        assert!(s.by_day_family.keys().all(|d| !opus_over(&day_shares(&s.by_day_family, *d).unwrap(), None)));
    }

    #[test]
    fn dashboard_refresh_over_a_large_index_stays_under_a_second() {
        let models = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"];
        let mut files = HashMap::new();
        for f in 0..2_000u32 {
            let groups = (0..100u32).map(|g| grp(20260901 + (g % 80), &format!("s{}", f % 400), models[(g % 4) as usize], 1_000 + g as u64)).collect();
            files.insert(format!("f{f}"), FileEntry { size: 1, mtime: 1, groups });
        }
        let idx = Index { version: INDEX_VERSION, files };
        let map: HashMap<String, String> = (0..400).map(|i| (format!("s{i}"), format!("A{}", i % 5))).collect();
        let started = std::time::Instant::now();
        let s = summarize(&idx, &map, 20260901, &|_| true);
        let w = week_by_account(&idx, &map, 20260915);
        let took = started.elapsed();
        assert_eq!(s.by_day_family.len(), 80);
        assert_eq!(w.len(), 5);
        assert!(took.as_millis() < 1000, "200k groups took {took:?}");
    }

    #[test]
    fn weekdays() {
        assert_eq!(weekday(20261015), 4); // a Thursday
        assert_eq!(weekday(20261011), 0);
    }

    #[test]
    fn codex_rollouts_count_each_response_once_and_skip_a_forks_copy() {
        let dir = std::env::temp_dir().join(format!("ca-codex-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("rollout.jsonl");
        let rec = |ord: u64, id: &str| format!("{{\"timestamp\":\"2026-10-15T18:48:40.581Z\",\"ordinal\":{ord},\"type\":\"token_usage_record\",\"payload\":{{\"response_id\":\"{id}\",\"usage\":{{\"input_tokens\":1000,\"cached_input_tokens\":800,\"output_tokens\":50}}}}}}\n");
        let text = [
            "{\"timestamp\":\"2026-10-15T18:00:00Z\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{\"forked_from_ordinal_exclusive\":10}}\n".to_string(),
            rec(5, "copied"),
            "{\"timestamp\":\"2026-10-15T18:00:00Z\",\"ordinal\":11,\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-6.1-sol\",\"cwd\":\"F:\\\\dev\\\\sample-game\"}}\n".into(),
            rec(12, "a"),
            rec(13, "a"),
            rec(14, "b"),
        ]
        .concat();
        std::fs::write(&p, text).unwrap();
        let g = read_codex(&p, "Codex");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(g.len(), 1);
        let (k, t) = &g[0];
        assert_eq!((k.model.as_str(), k.project.as_str(), k.session.as_str()), ("gpt-6.1-sol", "sample-game", "\u{1}Codex"));
        assert_eq!((t.replies, t.input, t.read, t.output), (2, 400, 1600, 100));
    }

    #[test]
    fn forecasts() {
        let h = 3600;
        // 10% an hour from 40%: 100% in 6 h, before a reset 10 h away.
        let s = [(0, 30), (h, 40)];
        assert_eq!(forecast(&s, 5 * h, Some(11 * h)), Forecast::At(7 * h));
        assert_eq!(forecast(&s, 5 * h, Some(4 * h)), Forecast::Lasts);
        assert_eq!(forecast(&[(0, 50), (h, 50)], 5 * h, None), Forecast::Lasts);
        assert_eq!(forecast(&[(0, 99), (h, 100)], 5 * h, None), Forecast::Reached);
        assert_eq!(forecast(&[(0, 10), (60, 20)], 5 * h, None), Forecast::Unknown, "too short to tell");
        // A reset in the window (80 then 5): the pace counts only after it.
        assert_eq!(forecast(&[(0, 80), (h, 5), (2 * h, 15)], 5 * h, None), Forecast::At(2 * h + 85 * 360));
    }

    // ---- top chats ----

    const NOW: &str = "2026-11-23T12:00:00Z";

    fn chat_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hive-top-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        dir
    }

    /// One assistant line: time, message id, model, input, output, cache read, cache write.
    fn ln(session: &str, ts: &str, id: &str, model: &str, i: u64, o: u64, r: u64, w: u64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{ts}\",\"sessionId\":\"{session}\",\"requestId\":\"req_{id}\",\"message\":{{\"id\":\"msg_{id}\",\"model\":\"{model}\",\"usage\":{{\"input_tokens\":{i},\"output_tokens\":{o},\"cache_read_input_tokens\":{r},\"cache_creation_input_tokens\":{w}}}}}}}\n"
        )
    }

    fn now() -> u64 {
        utc_secs(NOW).unwrap()
    }

    const SECS: std::time::Duration = std::time::Duration::from_secs(60);

    #[test]
    fn top_chats_count_the_window_mix_context_and_handoff_mark() {
        let dir = chat_dir("ac1");
        // Chat A: Opus twice (the second streamed in two copies), Sonnet once, one reply 10 days old.
        let mut a = String::new();
        a += &ln("aaa", "2026-11-13T10:00:00.000Z", "old", "claude-opus-5-5", 1, 1_000_000, 0, 0);
        a += &ln("aaa", "2026-11-20T10:00:00.000Z", "1", "claude-opus-5-5", 10, 90, 900, 0);
        a += &ln("aaa", "2026-11-21T10:00:00.000Z", "2", "claude-opus-5-5", 10, 5, 500, 100);
        a += &ln("aaa", "2026-11-21T10:00:01.000Z", "2", "claude-opus-5-5", 10, 50, 500, 100);
        a += &ln("aaa", "2026-11-22T10:00:00.000Z", "3", "claude-sonnet-5-5", 20, 80, 3_000, 900);
        std::fs::write(dir.join("proj/aaa.jsonl"), a).unwrap();
        // A helper's file counts toward chat A's tokens but not its context.
        std::fs::create_dir_all(dir.join("proj/aaa/subagents")).unwrap();
        std::fs::write(dir.join("proj/aaa/subagents/agent-1.jsonl"), ln("aaa", "2026-11-22T11:00:00.000Z", "h1", "claude-haiku-5-5", 100, 100, 800_000, 0)).unwrap();
        // Chat B: a smaller week but 350k of context on its last turn.
        let mut b = ln("bbb", "2026-11-22T09:00:00.000Z", "b1", "claude-opus-5-5", 5, 5, 100, 0);
        b += &ln("bbb", "2026-11-23T09:00:00.000Z", "b2", "claude-opus-5-5", 50_000, 100, 300_000, 0);
        std::fs::write(dir.join("proj/bbb.jsonl"), b).unwrap();
        // Chat C: only a copied line under another chat's id (a resumed chat) and a synthetic reply.
        let mut c = ln("zzz", "2026-11-22T09:00:00.000Z", "c1", "claude-opus-5-5", 1, 1, 1, 1);
        c += &ln("ccc", "2026-11-22T09:00:00.000Z", "c2", "<synthetic>", 1, 1, 1, 1);
        std::fs::write(dir.join("proj/ccc.jsonl"), c).unwrap();

        let mut idx = ChatIndex::default();
        let (read, left) = idx.update(&dir, now(), SECS);
        assert_eq!((read, left), (4, 0));
        let top = idx.top(now(), 10);
        assert_eq!(top.len(), 2, "{top:?}");
        let (a, b) = (&top[0], &top[1]);
        assert_eq!(a.session, "aaa", "ranked by tokens: {top:?}");
        // Opus: reply 1 plus the larger copy of reply 2 (output 50), counted once.
        let opus = (10 + 90 + 900) + (10 + 50 + 500 + 100);
        assert_eq!(a.mix, [opus, 20 + 80 + 3_000 + 900, 100 + 100 + 800_000, 0, 0], "{a:?}");
        assert_eq!(a.tokens, a.mix.iter().sum::<u64>());
        // Context is the last main-chain turn (Sonnet's), not the helper's 800k.
        assert_eq!(a.context, 20 + 3_000 + 900);
        assert!(!a.past_handoff());
        assert_eq!(b.session, "bbb");
        assert_eq!(b.tokens, (5 + 5 + 100) + (50_000 + 100 + 300_000));
        assert_eq!(b.context, 350_000);
        assert!(b.past_handoff());
        assert_eq!(b.mix, [b.tokens, 0, 0, 0, 0]);
        assert!(a.tokens > b.tokens);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_chats_window_slides_and_old_files_are_not_read() {
        let dir = chat_dir("ac1b");
        std::fs::write(dir.join("proj/new.jsonl"), ln("new", "2026-11-23T01:00:00.000Z", "1", "claude-opus-5-5", 1, 1, 1, 1)).unwrap();
        let mut idx = ChatIndex::default();
        idx.update(&dir, now(), SECS);
        assert_eq!(idx.top(now(), 10).len(), 1);
        // Eight days later nothing is in the window.
        assert!(idx.top(now() + 8 * 86_400, 10).is_empty());
        // A file last touched long before "now" is remembered as read, without being parsed.
        std::fs::write(dir.join("proj/old.jsonl"), ln("old", "2026-11-23T01:00:00.000Z", "2", "claude-opus-5-5", 1, 1, 1, 1)).unwrap();
        idx.update(&dir, now() + 40 * 86_400, SECS);
        let old = &idx.files[&dir.join("proj").join("old.jsonl").to_string_lossy().to_string()];
        assert!(old.hours.is_empty() && old.offset == old.size && old.size > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_chats_append_reads_only_new_bytes_and_a_replaced_file_starts_over() {
        let dir = chat_dir("ac2");
        let f = dir.join("proj").join("ddd.jsonl");
        let l1 = ln("ddd", "2026-11-22T10:00:00.000Z", "1", "claude-opus-5-5", 100, 100, 100, 100);
        let l2 = ln("ddd", "2026-11-22T10:01:00.000Z", "2", "claude-opus-5-5", 100, 100, 100, 100);
        std::fs::write(&f, format!("{l1}{l2}")).unwrap();
        let mut idx = ChatIndex::default();
        idx.update(&dir, now(), SECS);
        let key = f.to_string_lossy().to_string();
        assert_eq!(idx.files[&key].offset, (l1.len() + l2.len()) as u64);
        assert_eq!(idx.top(now(), 5)[0].tokens, 800);
        // Change an already-read line in place (same length, other numbers) and append a third.
        // Only the appended bytes are read, so the change is not seen.
        let l2x = l2.replace("\"output_tokens\":100", "\"output_tokens\":999");
        assert_eq!(l2x.len(), l2.len());
        let l3 = ln("ddd", "2026-11-22T10:02:00.000Z", "3", "claude-opus-5-5", 100, 100, 100, 100);
        std::fs::write(&f, format!("{l1}{l2x}{l3}")).unwrap();
        let before = idx.files[&key].offset;
        idx.update(&dir, now(), SECS);
        assert_eq!(idx.files[&key].offset, before + l3.len() as u64);
        assert_eq!(idx.top(now(), 5)[0].tokens, 1200, "appended lines add; earlier ones are not read again");
        // A half-written last line waits for its newline.
        let l4 = ln("ddd", "2026-11-22T10:03:00.000Z", "4", "claude-opus-5-5", 100, 100, 100, 100);
        let (half, rest) = l4.split_at(l4.len() / 2);
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str(half);
        std::fs::write(&f, &text).unwrap();
        idx.update(&dir, now(), SECS);
        assert_eq!(idx.top(now(), 5)[0].tokens, 1200);
        text.push_str(rest);
        std::fs::write(&f, &text).unwrap();
        idx.update(&dir, now(), SECS);
        assert_eq!(idx.top(now(), 5)[0].tokens, 1600);
        // Truncated: shorter than what was read, so it starts again from byte 0.
        std::fs::write(&f, ln("ddd", "2026-11-22T10:00:00.000Z", "9", "claude-sonnet-5-5", 1, 2, 3, 4)).unwrap();
        idx.update(&dir, now(), SECS);
        let t = &idx.top(now(), 5)[0];
        assert_eq!((t.tokens, t.mix[1]), (10, 10), "{t:?}");
        // Replaced by a longer file with other first bytes: also read from 0.
        let long = format!(
            "{}{}",
            ln("ddd", "2026-11-22T11:00:00.000Z", "a", "claude-haiku-5-5", 7, 7, 7, 7),
            ln("ddd", "2026-11-22T11:01:00.000Z", "b", "claude-haiku-5-5", 8, 8, 8, 8)
        );
        std::fs::write(&f, long.repeat(3)).unwrap();
        idx.update(&dir, now(), SECS);
        let t = &idx.top(now(), 5)[0];
        // Both ids repeat three times: a repeated id counts once (28 and 32).
        assert_eq!((t.tokens, t.mix[2]), (60, 60), "{t:?}");
        // Saved and loaded, it carries on.
        let saved = dir.join("chats.json");
        idx.save(&saved);
        assert_eq!(ChatIndex::load(&saved).top(now(), 5), idx.top(now(), 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_chats_work_per_refresh_is_capped() {
        let dir = chat_dir("cap");
        for i in 0..5 {
            std::fs::write(dir.join(format!("proj/s{i}.jsonl")), ln(&format!("s{i}"), "2026-11-22T10:00:00.000Z", "1", "claude-opus-5-5", 1, 1, 1, 1)).unwrap();
        }
        let mut idx = ChatIndex::default();
        let (read, left) = idx.update(&dir, now(), std::time::Duration::ZERO);
        assert_eq!((read, left), (0, 5));
        let (read, left) = idx.update(&dir, now(), SECS);
        assert_eq!((read, left), (5, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }


    // ---- fix round ----

    #[test]
    fn top_chats_keep_entries_under_unlistable_folders_and_a_missing_root() {
        let seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let root = Path::new("C:\\data\\projects");
        let under_bad = "C:\\data\\projects\\bad\\a.jsonl";
        let under_good = "C:\\data\\projects\\good\\b.jsonl";
        let elsewhere = "C:\\other\\c.jsonl";
        let failed = vec![PathBuf::from("C:\\data\\projects\\bad")];
        assert!(keep_entry(under_bad, &seen, root, &failed, false), "a folder that failed to list keeps its entries");
        assert!(!keep_entry(under_good, &seen, root, &failed, false), "a listed folder drops entries that are gone");
        assert!(keep_entry(elsewhere, &seen, root, &failed, false));
        assert!(keep_entry(under_good, &seen, root, &[], true), "a failed root drops nothing");
        // End to end: the root vanishes for a moment, the index keeps everything.
        let dir = chat_dir("gone");
        std::fs::write(dir.join("proj").join("x.jsonl"), ln("x", "2026-11-22T10:00:00.000Z", "1", "claude-opus-5-5", 1, 1, 1, 1)).unwrap();
        let mut idx = ChatIndex::default();
        idx.update(&dir, now(), SECS);
        assert!(idx.take_dirty());
        assert!(!idx.take_dirty());
        let moved = dir.with_extension("away");
        std::fs::rename(&dir, &moved).unwrap();
        let (read, left) = idx.update(&dir, now(), SECS);
        assert_eq!((read, left), (0, 0));
        assert_eq!(idx.top(now(), 5).len(), 1, "a missing root must not empty the index");
        assert!(!idx.take_dirty());
        // The root is back but the file was deleted: now it goes, and that counts as a change.
        std::fs::rename(&moved, &dir).unwrap();
        std::fs::remove_file(dir.join("proj").join("x.jsonl")).unwrap();
        idx.update(&dir, now(), SECS);
        assert!(idx.top(now(), 5).is_empty());
        assert!(idx.take_dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_old(path: &Path) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(utc_secs("2026-10-01T00:00:00Z").unwrap())).unwrap();
    }

    #[test]
    fn top_chats_a_replaced_old_file_is_detected_by_its_stored_head() {
        let dir = chat_dir("oldhead");
        let f = dir.join("proj").join("eee.jsonl");
        let old = ln("eee", "2026-09-01T10:00:00.000Z", "o", "claude-opus-5-5", 1, 1, 1, 1);
        std::fs::write(&f, old.repeat(2)).unwrap();
        set_old(&f);
        let mut idx = ChatIndex::default();
        idx.update(&dir, now(), SECS);
        let key = f.to_string_lossy().to_string();
        assert!(idx.files[&key].head != 0 && idx.files[&key].hours.is_empty(), "stored without being parsed");
        // Replaced by another, longer file whose first lines are recent: read from 0.
        let r1 = ln("eee", "2026-11-22T10:00:00.000Z", "r1", "claude-opus-5-5", 10, 10, 10, 10);
        let r2 = ln("eee", "2026-11-22T10:01:00.000Z", "r2", "claude-opus-5-5", 20, 20, 20, 20);
        std::fs::write(&f, format!("{r1}{r2}{r1}")).unwrap();
        set_old(&f);
        assert!(std::fs::metadata(&f).unwrap().len() > idx.files[&key].offset);
        idx.update(&dir, now(), SECS);
        assert_eq!(idx.top(now(), 5)[0].tokens, 40 + 80, "both lines counted: the file was read from byte 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_chats_a_small_growing_file_is_not_taken_for_a_replaced_one() {
        let dir = chat_dir("small");
        let f = dir.join("proj").join("fff.jsonl");
        let mini = |id: &str, out: u32| {
            format!("{{\"type\":\"assistant\",\"timestamp\":\"2026-11-22T10:00:00.000Z\",\"message\":{{\"id\":\"{id}\",\"model\":\"m\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":{out}}}}}}}\n")
        };
        let (l1, l2) = (mini("a", 1), mini("b", 1));
        assert!(l1.len() < 256);
        std::fs::write(&f, &l1).unwrap();
        let mut idx = ChatIndex::default();
        idx.update(&dir, now(), SECS);
        let key = f.to_string_lossy().to_string();
        assert_eq!(idx.files[&key].head, 0, "under 256 bytes: no fingerprint yet");
        assert_eq!(idx.top(now(), 5)[0].tokens, 2);
        // Grow past 256 bytes; the first line is also changed in place (same length), which a
        // re-read from byte 0 would show.
        let l1x = mini("a", 9);
        assert_eq!(l1x.len(), l1.len());
        std::fs::write(&f, format!("{l1x}{l2}{}", mini("c", 1))).unwrap();
        idx.update(&dir, now(), SECS);
        assert!(idx.files[&key].head != 0);
        assert_eq!(idx.top(now(), 5)[0].tokens, 2 + 2 + 2, "only the appended lines were read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real transcripts, read only: `cargo test --release real_top -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_top_chats_timing() {
        let root = crate::claude::projects_dir();
        let cache = std::env::temp_dir().join(format!("hive-top-real-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&cache);
        let t = std::time::Instant::now();
        let mut idx = ChatIndex::load(&cache);
        let (read, left) = idx.update(&root, crate::time::now(), std::time::Duration::from_secs(480));
        let first = t.elapsed();
        idx.save(&cache);
        let size = std::fs::metadata(&cache).map_or(0, |m| m.len());
        let t = std::time::Instant::now();
        let mut again = ChatIndex::load(&cache);
        let loaded = t.elapsed();
        let tw = std::time::Instant::now();
        let all = walk_parallel(&root).found;
        let walked = (tw.elapsed(), all.len());
        let t2 = std::time::Instant::now();
        let (read2, left2) = again.update(&root, crate::time::now(), std::time::Duration::from_secs(480));
        let second = t2.elapsed();
        println!("TIMING first: {first:?} read {read} left {left}; cache {size} bytes, load {loaded:?}; second: {second:?} read {read2} left {left2}; walk alone {:?} over {} files", walked.0, walked.1);
        for (i, c) in again.top(crate::time::now(), 10).iter().enumerate() {
            println!("TOP {} tokens {} context {} mix {:?} past300k {}", i + 1, c.tokens, c.context, c.mix, c.past_handoff());
        }
        let _ = std::fs::remove_file(&cache);
    }
}
