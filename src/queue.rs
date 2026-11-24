//! Shared job queue (`Hive.exe --queue ...`): chats and accounts claim jobs so no two do the same work.
//! Files: `<id>.json` (the job), `<id>.claim` (created with CreateNew, the atomic lock, holds the
//! holder and heartbeat). No network, no account data.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const STALE_SECS: u64 = 30 * 60;
const MAX_LIST: usize = 30;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Job {
    pub id: String,
    pub title: String,
    pub lane: String,
    pub status: String,
    pub created: u64,
    #[serde(default)]
    pub claimed_by: String,
    #[serde(default)]
    pub heartbeat: u64,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Claim {
    by: String,
    heartbeat: u64,
}

#[derive(Debug, PartialEq)]
pub enum QErr {
    Usage,
    /// Already claimed and alive: holder name.
    Held(String),
    /// `next` found nothing.
    Empty,
    Other(String),
}

pub fn dir() -> PathBuf {
    match std::env::var("HIVE_QUEUE_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => crate::exe_dir().join("hive-control").join("queue"),
    }
}

/// Lowercase id, or None when it has odd characters or is a Windows reserved device name.
fn norm_id(id: &str) -> Option<String> {
    let id = id.to_ascii_lowercase();
    let ok = !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let stem = id.split('.').next().unwrap_or("");
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt")) && stem.len() == 4 && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if ok && !reserved {
        Some(id)
    } else {
        None
    }
}

fn other<E: std::fmt::Display>(e: E) -> QErr {
    QErr::Other(e.to_string())
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_path(d: &Path, name: &str) -> PathBuf {
    d.join(format!("{name}.{}.{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)))
}

/// Temp file then rename, retried: a reader can hold the target for a moment on Windows.
fn write_atomic(path: &Path, text: &str) -> Result<(), QErr> {
    let d = path.parent().unwrap_or(Path::new("."));
    let tmp = tmp_path(d, path.file_name().and_then(|s| s.to_str()).unwrap_or("q"));
    fs::write(&tmp, text).map_err(|e| QErr::Other(format!("write tmp: {e}")))?;
    let mut last = String::new();
    for _ in 0..50 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(QErr::Other(format!("rename: {last}")))
}

fn read_retry(path: &Path) -> Result<String, QErr> {
    let mut last = String::new();
    for _ in 0..50 {
        match fs::read_to_string(path) {
            Ok(s) => return Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(QErr::Other(format!("not found: {}", path.display())))
            }
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    Err(QErr::Other(last))
}

fn job_path(d: &Path, id: &str) -> PathBuf {
    d.join(format!("{id}.json"))
}
fn claim_path(d: &Path, id: &str) -> PathBuf {
    d.join(format!("{id}.claim"))
}

fn load(d: &Path, id: &str) -> Result<Job, QErr> {
    let s = read_retry(&job_path(d, id))?;
    serde_json::from_str(&s).map_err(other)
}

fn save(d: &Path, j: &Job) -> Result<(), QErr> {
    write_atomic(&job_path(d, &j.id), &serde_json::to_string_pretty(j).map_err(other)?)
}

pub fn add(d: &Path, id: &str, title: &str, lane: &str, now: u64) -> Result<(), QErr> {
    let id = norm_id(id).ok_or(QErr::Usage)?;
    if title.is_empty() || (lane != "claude" && lane != "codex") {
        return Err(QErr::Usage);
    }
    fs::create_dir_all(d).map_err(other)?;
    let j = Job { id: id.clone(), title: title.into(), lane: lane.into(), status: "todo".into(), created: now, ..Default::default() };
    // Temp file, then MoveFileExW without REPLACE_EXISTING (atomic, fails if the id exists): readers never
    // see a partial job, a crash leaves no broken id, and a second add never replaces the job.
    let tmp = tmp_path(d, &format!("{id}.add"));
    fs::write(&tmp, serde_json::to_string_pretty(&j).map_err(other)?).map_err(other)?;
    let r = move_new(&tmp, &job_path(d, &id));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || fs::metadata(job_path(d, &id)).is_ok() => {
            Err(QErr::Other(format!("job {id} exists")))
        }
        Err(e) => Err(other(e)),
    }
}

fn move_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let w = |p: &Path| p.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (a, b) = (w(from), w(to));
    // SAFETY: both buffers are NUL-terminated and live for the call.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(a.as_ptr(), b.as_ptr(), 0) };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Per-job mutex: `<id>.lock` opened with share mode 0 (no sharing). Whoever holds the open handle owns the
/// lock; the OS refuses every other open, delete and rename until the handle closes, and closes it itself if
/// the holder crashes. So there is no token, no stale-lock breaking and no race between breakers. The empty
/// file stays behind between uses on purpose (deleting it would reopen a create/delete race).
struct Guard {
    _f: fs::File,
}

/// How long a failed lock open may keep retrying, by Windows error code: 32 (sharing violation) and 33
/// (lock violation) mean another caller holds the lock right now; 5 (access denied) is what a lock file
/// that is delete-pending reports for a moment. Anything else (bad path, full disk, a directory in the way)
/// will not clear by waiting, so it fails at once.
fn lock_retry_for(e: &std::io::Error) -> Option<std::time::Duration> {
    match e.raw_os_error() {
        Some(32) | Some(33) => Some(std::time::Duration::from_secs(10)),
        Some(5) => Some(std::time::Duration::from_secs(1)),
        _ => None,
    }
}

fn lock(d: &Path, id: &str) -> Result<Guard, QErr> {
    use std::os::windows::fs::OpenOptionsExt;
    let path = d.join(format!("{id}.lock"));
    let start = std::time::Instant::now();
    loop {
        match OpenOptions::new().write(true).create(true).share_mode(0).open(&path) {
            Ok(f) => return Ok(Guard { _f: f }),
            Err(e) => match lock_retry_for(&e) {
                Some(limit) if start.elapsed() <= limit => std::thread::sleep(std::time::Duration::from_millis(2)),
                Some(_) => return Err(QErr::Other(format!("job {id} is busy"))),
                None => return Err(QErr::Other(format!("lock {id}: {e}"))),
            },
        }
    }
}

fn done_err(id: &str) -> QErr {
    QErr::Other(format!("job {id} is done"))
}

/// Fails before any lock file exists when the job is not there (a typo id must not leave `<id>.lock`).
fn exists(d: &Path, id: &str) -> Result<(), QErr> {
    if job_path(d, id).is_file() {
        Ok(())
    } else {
        Err(QErr::Other(format!("not found: {}", job_path(d, id).display())))
    }
}

/// `--by` must be a real name: empty is a usage error, and "unknown" is what an unreadable claim file
/// reports as its holder, so nobody may claim under it.
fn check_by(by: &str) -> Result<(), QErr> {
    if by.is_empty() {
        Err(QErr::Usage)
    } else if by.eq_ignore_ascii_case("unknown") {
        Err(QErr::Other("--by \"unknown\" is reserved".into()))
    } else {
        Ok(())
    }
}

/// Retry an operation that can fail for a moment while a scanner holds a file.
fn retry<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut last = None;
    for _ in 0..100 {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(e),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
    Err(last.unwrap())
}

/// Holder and heartbeat from the claim file. A claim that does not parse (a crash between CreateNew and the
/// write) uses the file's modified time as its heartbeat, so it goes stale like any other.
fn read_claim(d: &Path, id: &str) -> Option<Claim> {
    let p = claim_path(d, id);
    let s = match fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => String::new(),
    };
    if let Ok(c) = serde_json::from_str::<Claim>(&s) {
        return Some(c);
    }
    let hb = fs::metadata(&p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |t| t.as_secs());
    Some(Claim { by: "unknown".into(), heartbeat: hb })
}

/// Create the claim file (caller holds the job lock). A failed write removes the file again.
fn create_claim(d: &Path, id: &str, by: &str, now: u64) -> Result<(), QErr> {
    let p = claim_path(d, id);
    let mut f = retry(|| OpenOptions::new().write(true).create_new(true).open(&p)).map_err(|e| QErr::Other(format!("create claim: {e}")))?;
    let c = Claim { by: by.into(), heartbeat: now };
    let r = f.write_all(serde_json::to_string(&c).map_err(other)?.as_bytes());
    drop(f);
    if let Err(e) = r {
        let _ = retry(|| fs::remove_file(&p));
        return Err(other(e));
    }
    Ok(())
}

/// Claim under the job lock. Returns Ok(true) when a stale claim was taken over.
pub fn claim(d: &Path, id: &str, by: &str, now: u64) -> Result<bool, QErr> {
    let id = norm_id(id).ok_or(QErr::Usage)?;
    check_by(by)?;
    exists(d, &id)?;
    let _g = lock(d, &id)?;
    let mut j = load(d, &id)?; // loaded after the lock, so a stale copy cannot resurrect a done job
    if j.status == "done" {
        return Err(done_err(&id));
    }
    let mut took_over = false;
    if let Some(c) = read_claim(d, &id) {
        if now.saturating_sub(c.heartbeat) <= STALE_SECS {
            return Err(QErr::Held(c.by));
        }
        retry(|| fs::remove_file(claim_path(d, &id))).map_err(|e| QErr::Other(format!("remove claim: {e}")))?;
        took_over = true;
    }
    create_claim(d, &id, by, now)?;
    j.status = "claimed".into();
    j.claimed_by = by.into();
    j.heartbeat = now;
    if let Err(e) = save(d, &j) {
        let _ = retry(|| fs::remove_file(claim_path(d, &id)));
        return Err(e);
    }
    Ok(took_over)
}

fn holder_ok(d: &Path, id: &str, by: &str) -> Result<(), QErr> {
    check_by(by)?;
    match read_claim(d, id) {
        Some(c) if c.by == by => Ok(()),
        Some(c) => Err(QErr::Held(c.by)),
        None => Err(QErr::Other(format!("job {id} is not claimed"))),
    }
}

pub fn beat(d: &Path, id: &str, by: &str, now: u64) -> Result<(), QErr> {
    let id = norm_id(id).ok_or(QErr::Usage)?;
    check_by(by)?;
    exists(d, &id)?;
    let _g = lock(d, &id)?;
    holder_ok(d, &id, by)?;
    let c = Claim { by: by.into(), heartbeat: now };
    write_atomic(&claim_path(d, &id), &serde_json::to_string(&c).map_err(other)?)?;
    let mut j = load(d, &id)?;
    j.heartbeat = now;
    save(d, &j)
}

pub fn done(d: &Path, id: &str, by: &str, note: &str, now: u64) -> Result<(), QErr> {
    let id = norm_id(id).ok_or(QErr::Usage)?;
    check_by(by)?;
    exists(d, &id)?;
    let _g = lock(d, &id)?;
    holder_ok(d, &id, by)?;
    let mut j = load(d, &id)?;
    j.status = "done".into();
    j.heartbeat = now;
    j.note = note.into();
    save(d, &j) // the claim file stays so a done job can never be claimed again
}

pub fn release(d: &Path, id: &str, by: &str, _now: u64) -> Result<(), QErr> {
    let id = norm_id(id).ok_or(QErr::Usage)?;
    check_by(by)?;
    exists(d, &id)?;
    let _g = lock(d, &id)?;
    holder_ok(d, &id, by)?;
    let mut j = load(d, &id)?;
    if j.status == "done" {
        return Err(done_err(&id));
    }
    // Claim first, then todo: a crash between them leaves a claimed job with no claim file, never a todo
    // job that still looks held.
    retry(|| fs::remove_file(claim_path(d, &id))).map_err(other)?;
    j.status = "todo".into();
    j.claimed_by.clear();
    save(d, &j)
}

fn jobs(d: &Path) -> Vec<Job> {
    let mut v: Vec<Job> = fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .filter(|e| e.path().file_stem().and_then(|s| s.to_str()).map_or(false, |s| norm_id(s).as_deref() == Some(s)))
        .filter_map(|e| read_retry(&e.path()).ok())
        .filter_map(|s| serde_json::from_str::<Job>(&s).ok())
        .filter(|j| norm_id(&j.id).as_deref() == Some(j.id.as_str()))
        .collect();
    v.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
    v
}

/// Jobs the runner or `next` may claim now, oldest first: todo jobs, plus claimed jobs whose heartbeat is
/// older than the stale limit (a chat that was never sent or died). `claim` makes the final call under the lock.
pub fn claimable(d: &Path, lane: Option<&str>, now: u64) -> Vec<Job> {
    jobs(d)
        .into_iter()
        .filter(|j| lane.map_or(true, |l| l == j.lane))
        .filter(|j| j.status == "todo" || (j.status == "claimed" && now.saturating_sub(j.heartbeat) > STALE_SECS))
        .collect()
}

/// True for the benign races where the job finished or its file went away between listing and claiming.
pub fn is_gone(e: &QErr, id: &str) -> bool {
    *e == done_err(id) || matches!(e, QErr::Other(m) if m.starts_with("not found:"))
}

fn age(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs / 60)
    }
}

/// One line per job, at most 30 lines (a final "+N more" line counts inside the cap).
pub fn list(d: &Path, all: bool, now: u64) -> Vec<String> {
    let rows: Vec<Job> = jobs(d).into_iter().filter(|j| all || j.status != "done").collect();
    let mut out = Vec::new();
    for (i, j) in rows.iter().enumerate() {
        if i == MAX_LIST - 1 && rows.len() > MAX_LIST {
            out.push(format!("+{} more", rows.len() - i));
            break;
        }
        let (holder, since) = if j.status == "claimed" { (j.claimed_by.as_str(), j.heartbeat) } else { ("-", j.created) };
        out.push(format!("{} {} {} {} {}", j.id, j.status, holder, j.lane, age(now.saturating_sub(since))));
    }
    out
}

/// Claim the oldest todo job in the lane (None = any lane); returns its id.
pub fn next(d: &Path, by: &str, lane: Option<&str>, now: u64) -> Result<String, QErr> {
    check_by(by)?;
    let mut first_err: Option<QErr> = None;
    for j in claimable(d, lane, now) {
        match claim(d, &j.id, by, now) {
            Ok(_) => return Ok(j.id),
            // Only a lost race (someone alive holds it) or a job finished behind our back is a normal skip.
            Err(QErr::Held(_)) => {}
            Err(e) if is_gone(&e, &j.id) => {}
            // Anything else (busy lock, bad file, disk error) is remembered and reported if nothing is claimed.
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.unwrap_or(QErr::Empty))
}

const USAGE: &str = "Hive --queue run [--lane claude] [--once] [--dry] | add <id> <title> [--lane claude|codex] | claim <id> --by <name> | beat <id> --by <name> | done <id> --by <name> [--note <text>] | release <id> --by <name> | list [--all] | next --by <name> [--lane claude|codex]";

struct Parsed {
    pos: Vec<String>,
    by: Option<String>,
    lane: Option<String>,
    note: Option<String>,
    all: bool,
}

fn parse(args: &[String]) -> Option<Parsed> {
    let mut p = Parsed { pos: vec![], by: None, lane: None, note: None, all: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--by" => p.by = Some(it.next()?.clone()),
            "--lane" => p.lane = Some(it.next()?.clone()),
            "--note" => p.note = Some(it.next()?.clone()),
            "--all" => p.all = true,
            s if s.starts_with("--") => return None,
            _ => p.pos.push(a.clone()),
        }
    }
    Some(p)
}

/// Runs one command against a folder; Ok carries the lines to print.
pub fn run(d: &Path, args: &[String], now: u64) -> Result<Vec<String>, QErr> {
    let sub = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let p = parse(args.get(1..).unwrap_or(&[])).ok_or(QErr::Usage)?;
    let by = p.by.clone().unwrap_or_default();
    if p.lane.as_deref().map_or(false, |l| l != "claude" && l != "codex") {
        return Err(QErr::Usage);
    }
    let one_id = || if p.pos.len() == 1 { Ok(p.pos[0].as_str()) } else { Err(QErr::Usage) };
    match sub.as_str() {
        "add" => {
            if p.pos.len() < 2 || p.by.is_some() {
                return Err(QErr::Usage);
            }
            add(d, &p.pos[0], &p.pos[1..].join(" "), p.lane.as_deref().unwrap_or("claude"), now)?;
            Ok(vec![format!("added {}", p.pos[0])])
        }
        "claim" => {
            let id = one_id()?;
            let took = claim(d, id, &by, now)?;
            Ok(vec![if took { format!("took over stale claim on {id}") } else { format!("claimed {id}") }])
        }
        "beat" => {
            beat(d, one_id()?, &by, now)?;
            Ok(vec![])
        }
        "done" => {
            done(d, one_id()?, &by, p.note.as_deref().unwrap_or(""), now)?;
            Ok(vec![])
        }
        "release" => {
            release(d, one_id()?, &by, now)?;
            Ok(vec![])
        }
        "list" => {
            if !p.pos.is_empty() {
                return Err(QErr::Usage);
            }
            Ok(list(d, p.all, now))
        }
        "next" => {
            if !p.pos.is_empty() {
                return Err(QErr::Usage);
            }
            Ok(vec![next(d, &by, p.lane.as_deref(), now)?])
        }
        _ => Err(QErr::Usage),
    }
}

pub fn cli(args: &[String]) {
    // `--queue run` starts jobs on accounts (see runner.rs); the rest are the plain queue commands.
    if args.first().is_some_and(|a| a.eq_ignore_ascii_case("run")) {
        return crate::runner::cli(&args[1..]);
    }
    crate::console();
    let code = match run(&dir(), args, crate::time::now()) {
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
            0
        }
        Err(QErr::Usage) => {
            println!("{USAGE}");
            2
        }
        Err(QErr::Held(h)) => {
            println!("already claimed by {h}");
            3
        }
        Err(QErr::Empty) => {
            println!("no todo job");
            4
        }
        Err(QErr::Other(e)) => {
            println!("{e}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hive-queue-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn basic_flow() {
        let d = tdir("basic");
        add(&d, "a1", "First", "claude", 100).unwrap();
        assert!(add(&d, "a1", "again", "claude", 100).is_err());
        assert_eq!(claim(&d, "a1", "w1", 200), Ok(false));
        assert_eq!(claim(&d, "a1", "w2", 210), Err(QErr::Held("w1".into())));
        assert_eq!(done(&d, "a1", "w2", "", 220), Err(QErr::Held("w1".into())));
        let l = list(&d, false, 300);
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("claimed") && l[0].contains("w1"), "{l:?}");
        done(&d, "a1", "w1", "ok", 300).unwrap();
        assert!(list(&d, false, 300).is_empty());
        assert_eq!(list(&d, true, 300).len(), 1);
        assert!(claim(&d, "a1", "w2", 400).is_err());
        add(&d, "b2", "Second", "codex", 110).unwrap();
        claim(&d, "b2", "w1", 200).unwrap();
        release(&d, "b2", "w1", 210).unwrap();
        assert_eq!(claim(&d, "b2", "w3", 220), Ok(false));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn race_claim_one_winner() {
        let d = Arc::new(tdir("race1"));
        add(&d, "r", "Race", "claude", 1).unwrap();
        let hs: Vec<_> = (0..16)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || claim(&d, "r", &format!("t{i}"), 50).is_ok())
            })
            .collect();
        let wins = hs.into_iter().map(|h| h.join().unwrap()).filter(|w| *w).count();
        assert_eq!(wins, 1);
        let _ = fs::remove_dir_all(&*d);
    }

    #[test]
    fn race_next_each_once() {
        let d = Arc::new(tdir("race2"));
        for i in 0..8 {
            add(&d, &format!("j{i}"), "x", "claude", i).unwrap();
        }
        let hs: Vec<_> = (0..16)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || next(&d, &format!("t{i}"), Some("claude"), 50).ok())
            })
            .collect();
        let mut got: Vec<String> = hs.into_iter().filter_map(|h| h.join().unwrap()).collect();
        assert_eq!(got.len(), 8);
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 8);
        assert_eq!(next(&d, "late", None, 60), Err(QErr::Empty));
        let _ = fs::remove_dir_all(&*d);
    }

    #[test]
    fn stale_takeover() {
        let d = tdir("stale");
        add(&d, "s", "Stale", "claude", 0).unwrap();
        let t0 = 10_000;
        claim(&d, "s", "old", t0).unwrap();
        assert_eq!(claim(&d, "s", "new", t0 + 29 * 60), Err(QErr::Held("old".into())));
        beat(&d, "s", "old", t0 + 29 * 60).unwrap();
        assert_eq!(claim(&d, "s", "new", t0 + 29 * 60 + 29 * 60), Err(QErr::Held("old".into())));
        assert_eq!(claim(&d, "s", "new", t0 + 29 * 60 + 31 * 60), Ok(true));
        assert!(list(&d, false, t0 + 7200)[0].contains("new"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn stale_takeover_race_one_winner() {
        let d = Arc::new(tdir("stale-race"));
        add(&d, "s", "Stale", "claude", 0).unwrap();
        claim(&d, "s", "old", 1000).unwrap();
        let hs: Vec<_> = (0..16)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || claim(&d, "s", &format!("t{i}"), 1000 + 31 * 60).is_ok())
            })
            .collect();
        assert_eq!(hs.into_iter().map(|h| h.join().unwrap()).filter(|w| *w).count(), 1);
        let _ = fs::remove_dir_all(&*d);
    }

    #[test]
    fn bad_args_and_list_cap() {
        let d = tdir("args");
        for bad in [
            s(&[]),
            s(&["bogus"]),
            s(&["add"]),
            s(&["add", "x"]),
            s(&["claim", "x"]),
            s(&["claim", "--by"]),
            s(&["next"]),
            s(&["add", "x", "t", "--lane", "gpu"]),
            s(&["list", "extra"]),
            s(&["add", "bad id", "t"]),
            s(&["claim", "../x", "--by", "w"]),
        ] {
            assert_eq!(run(&d, &bad, 1), Err(QErr::Usage), "{bad:?}");
        }
        for i in 0..45 {
            run(&d, &s(&["add", &format!("n{i:02}"), "title", "words"]), i).unwrap();
        }
        let out = run(&d, &s(&["list", "--all"]), 100).unwrap();
        assert_eq!(out.len(), 30);
        assert!(out[29].starts_with('+'));
        let _ = fs::remove_dir_all(&d);
    }

    fn age_file(p: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn stale_takeover_with_stale_lock_one_winner() {
        let d = Arc::new(tdir("stale-lock"));
        add(&d, "s", "Stale", "claude", 0).unwrap();
        claim(&d, "s", "old", 1000).unwrap();
        // A crashed taker left its lock file behind, too (not held open by anyone).
        fs::write(d.join("s.lock"), "dead-token").unwrap();
        age_file(&d.join("s.lock"), 120);
        let hs: Vec<_> = (0..16)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || claim(&d, "s", &format!("t{i}"), 1000 + 31 * 60).is_ok())
            })
            .collect();
        assert_eq!(hs.into_iter().map(|h| h.join().unwrap()).filter(|w| *w).count(), 1);
        let _ = fs::remove_dir_all(&*d);
    }

    #[test]
    fn taken_over_holder_cannot_write() {
        let d = tdir("late");
        add(&d, "l", "Late", "claude", 0).unwrap();
        claim(&d, "l", "old", 1000).unwrap();
        assert_eq!(claim(&d, "l", "new", 1000 + 31 * 60), Ok(true));
        assert_eq!(beat(&d, "l", "old", 5000), Err(QErr::Held("new".into())));
        assert_eq!(done(&d, "l", "old", "x", 5000), Err(QErr::Held("new".into())));
        assert_eq!(release(&d, "l", "old", 5000), Err(QErr::Held("new".into())));
        let l = list(&d, false, 5000);
        assert!(l[0].contains("claimed") && l[0].contains("new"), "{l:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unparseable_claim_goes_stale_by_mtime() {
        let d = tdir("halfclaim");
        add(&d, "h", "Half", "claude", 0).unwrap();
        fs::write(d.join("h.claim"), "").unwrap();
        let now = crate::time::now();
        assert_eq!(claim(&d, "h", "w", now), Err(QErr::Held("unknown".into())));
        age_file(&d.join("h.claim"), 31 * 60);
        assert_eq!(claim(&d, "h", "w", now), Ok(true));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn reserved_and_uppercase_ids() {
        let d = tdir("ids");
        for bad in ["CON", "con", "Nul", "aux", "PRN", "com1", "LPT9", "con.txt"] {
            assert_eq!(add(&d, bad, "t", "claude", 1), Err(QErr::Usage), "{bad}");
        }
        add(&d, "com0", "ok", "claude", 1).unwrap();
        add(&d, "Card-7", "Upper", "claude", 2).unwrap();
        assert!(d.join("card-7.json").exists());
        assert_eq!(claim(&d, "CARD-7", "w", 3), Ok(false));
        assert_eq!(claim(&d, "card-7", "w2", 3), Err(QErr::Held("w".into())));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn next_skips_bad_done_and_missing_jobs() {
        let d = tdir("nextskip");
        add(&d, "a", "A", "claude", 1).unwrap();
        add(&d, "b", "B", "claude", 2).unwrap();
        add(&d, "c", "C", "claude", 3).unwrap();
        // a is done behind next's back (claim file present), b is held by someone alive.
        claim(&d, "a", "x", 10).unwrap();
        done(&d, "a", "x", "", 11).unwrap();
        claim(&d, "b", "y", 10).unwrap();
        // A job file with an invalid id (a reserved name cannot even be created as a file here) must be ignored, and one claimed by another is skipped.
        fs::write(d.join("bad id.json"), "{\"id\":\"bad id\",\"title\":\"t\",\"lane\":\"claude\",\"status\":\"todo\",\"created\":0}").unwrap();
        assert_eq!(next(&d, "me", None, 20), Ok("c".into()));
        assert_eq!(next(&d, "me", None, 20), Err(QErr::Empty));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn mixed_ops_race_never_two_holders() {
        let d = Arc::new(tdir("mixed"));
        add(&d, "m", "Mixed", "claude", 1).unwrap();
        claim(&d, "m", "old", 1000).unwrap();
        let hs: Vec<_> = (0..16)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || {
                    let me = format!("t{i}");
                    if i % 4 == 0 {
                        let _ = beat(&d, "m", "old", 1000 + 31 * 60);
                        false
                    } else {
                        claim(&d, "m", &me, 1000 + 31 * 60).is_ok()
                    }
                })
            })
            .collect();
        let wins = hs.into_iter().map(|h| h.join().unwrap()).filter(|w| *w).count();
        // The old holder's beat may land first (then it is fresh and nobody wins) or lose to a taker.
        assert!(wins <= 1, "{wins}");
        let _ = fs::remove_dir_all(&*d);
    }

    #[test]
    fn lock_retries_only_sharing_and_access_errors() {
        use std::io::Error;
        assert_eq!(lock_retry_for(&Error::from_raw_os_error(32)), Some(std::time::Duration::from_secs(10)));
        assert_eq!(lock_retry_for(&Error::from_raw_os_error(33)), Some(std::time::Duration::from_secs(10)));
        assert_eq!(lock_retry_for(&Error::from_raw_os_error(5)), Some(std::time::Duration::from_secs(1)));
        for code in [2, 3, 15, 53, 87, 112, 123, 206, 1224] {
            assert_eq!(lock_retry_for(&Error::from_raw_os_error(code)), None, "{code}");
        }
        assert_eq!(lock_retry_for(&Error::new(std::io::ErrorKind::Other, "x")), None);
    }

    #[test]
    fn lock_waits_for_a_holder_then_gets_it() {
        let d = Arc::new(tdir("lockwait"));
        add(&d, "w", "W", "claude", 1).unwrap();
        let g = lock(&d, "w").unwrap();
        let d2 = d.clone();
        let t = std::thread::spawn(move || claim(&d2, "w", "waiter", 5).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(g);
        assert!(t.join().unwrap());
        let _ = fs::remove_dir_all(&*d);
    }

    #[test]
    fn lock_access_denied_gives_up_after_about_a_second() {
        let d = tdir("lockdir");
        add(&d, "x", "X", "claude", 1).unwrap();
        // A directory where the lock file should be: the open reports access denied and never clears.
        fs::create_dir(d.join("x.lock")).unwrap();
        let t0 = std::time::Instant::now();
        let r = claim(&d, "x", "w", 5);
        let took = t0.elapsed();
        assert_eq!(r, Err(QErr::Other("job x is busy".into())));
        assert!(took >= std::time::Duration::from_millis(900) && took < std::time::Duration::from_secs(5), "{took:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn next_skips_only_held_and_done_and_reports_the_first_other_error() {
        let d = tdir("nexterr");
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            add(&d, id, id, "claude", i as u64).unwrap();
        }
        // a: broken lock (other error), b: held by someone alive, c: nothing in the way once a is skipped.
        fs::create_dir(d.join("a.lock")).unwrap();
        claim(&d, "b", "y", 10).unwrap();
        assert_eq!(next(&d, "me", None, 20), Ok("c".into()));
        // Now only a (broken) and b (held) are left: the error from a comes back, not Empty.
        let e = next(&d, "me", None, 20);
        assert!(matches!(&e, Err(QErr::Other(m)) if m.contains("job a is busy")), "{e:?}");
        // Held and done alone still mean "nothing to claim".
        fs::remove_dir(d.join("a.lock")).unwrap();
        claim(&d, "a", "z", 10).unwrap();
        done(&d, "a", "z", "", 11).unwrap();
        assert_eq!(next(&d, "me", None, 20), Err(QErr::Empty));
        // A real run maps the other error to exit 1 (Other), never exit 4 (Empty).
        assert_ne!(e, Err(QErr::Empty));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn next_returns_an_unreadable_job_error_not_empty() {
        let d = tdir("nextbad");
        add(&d, "k", "K", "claude", 1).unwrap();
        // The job is listed as todo, then its file turns unreadable between the listing and the claim.
        fs::create_dir(d.join("k.lock")).unwrap();
        assert!(matches!(next(&d, "me", None, 20), Err(QErr::Other(_))));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_job_leaves_no_lock_file() {
        let d = tdir("nolock");
        assert!(matches!(claim(&d, "typo", "w", 1), Err(QErr::Other(m)) if m.contains("not found")));
        assert!(matches!(beat(&d, "typo", "w", 1), Err(QErr::Other(m)) if m.contains("not found")));
        assert!(matches!(done(&d, "typo", "w", "", 1), Err(QErr::Other(m)) if m.contains("not found")));
        assert!(matches!(release(&d, "typo", "w", 1), Err(QErr::Other(m)) if m.contains("not found")));
        let left: Vec<_> = fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.is_empty(), "{left:?}");
        // The folder itself missing is the same error, and is not created by a claim.
        let gone = d.join("nope");
        assert!(claim(&gone, "typo", "w", 1).is_err());
        assert!(!gone.exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn by_unknown_is_rejected_everywhere() {
        let d = tdir("unknown");
        add(&d, "u", "U", "claude", 1).unwrap();
        for who in ["unknown", "Unknown", "UNKNOWN"] {
            for r in [
                claim(&d, "u", who, 5).map(|_| ()),
                beat(&d, "u", who, 5),
                done(&d, "u", who, "", 5),
                release(&d, "u", who, 5),
                next(&d, who, None, 5).map(|_| ()),
            ] {
                assert!(matches!(&r, Err(QErr::Other(m)) if m.contains("reserved")), "{who}: {r:?}");
            }
        }
        assert_eq!(run(&d, &s(&["claim", "u", "--by", "unknown"]), 5), Err(QErr::Other("--by \"unknown\" is reserved".into())));
        // Nothing was claimed, and a real name still works.
        assert_eq!(claim(&d, "u", "real", 6), Ok(false));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn failed_save_removes_the_claim_file_and_leaves_the_job_claimable() {
        let d = tdir("savefail");
        add(&d, "f", "F", "claude", 1).unwrap();
        let jp = d.join("f.json");
        let mut perm = fs::metadata(&jp).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&jp, perm.clone()).unwrap();
        let r = claim(&d, "f", "w", 5);
        assert!(r.is_err(), "{r:?}");
        assert!(!d.join("f.claim").exists());
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        fs::set_permissions(&jp, perm).unwrap();
        assert_eq!(claim(&d, "f", "w", 6), Ok(false));
        let _ = fs::remove_dir_all(&d);
    }
}
