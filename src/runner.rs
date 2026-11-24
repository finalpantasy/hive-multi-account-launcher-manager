//! Queue runner (`Hive.exe --queue run [--lane claude] [--once] [--dry]`): hands each todo job to the
//! Claude account with the most headroom by opening a NEW Code chat there with the job's card path as the
//! prompt (the claude:// new-chat link the Tidy up flow already uses: prompt filled in, never sent).
//! The runner is prefill-only: the job is held while the draft waits, and an unsent claim goes stale after
//! 30 minutes, after which the next run (or `next`) offers it again.
//! Usage is read from the same local sources `--status` uses; nothing calls claude.ai or reads sign-in data.

use crate::queue::{self, Job, QErr};
use std::path::{Path, PathBuf};

const USAGE: &str = "Hive --queue run [--lane claude] [--once] [--dry]";

/// One account that could take a job: how much of its tighter limit (5-hour or week) is still free.
#[derive(Clone, Debug, PartialEq)]
pub struct Cand {
    pub name: String,
    pub headroom: u8,
    /// Minutes since the usage reading (None for fake usage).
    pub age_min: Option<u64>,
}

/// A usage reading older than this says nothing reliable about the account now (Claude samples every 15 minutes).
pub const MAX_READING_SECS: u64 = 60 * 60;

pub fn reading_fresh(as_of: u64, now: u64) -> bool {
    now.saturating_sub(as_of) <= MAX_READING_SECS
}

pub fn headroom(week: u8, five: u8) -> u8 {
    100 - week.max(five).min(100)
}

/// The account with the most headroom. An account at or past the "use next" line (`below`, 0 = off) is out;
/// a tie goes to the earlier one in the list.
pub fn pick(c: &[Cand], below: u8) -> Option<&Cand> {
    let mut best: Option<&Cand> = None;
    for x in c.iter().filter(|x| x.headroom > 0 && (below == 0 || 100 - x.headroom < below)) {
        if best.map_or(true, |b| x.headroom > b.headroom) {
            best = Some(x);
        }
    }
    best
}

/// Claude accounts with a usage reading, read-only (the same org and usage reads as `Use next`).
/// Work accounts (Team) never get Hive's own jobs.
fn live_candidates(cfg: &crate::config::Config) -> Vec<Cand> {
    use crate::claude;
    cfg.accounts
        .iter()
        .filter(|a| !crate::chatmove::is_work(a))
        .filter_map(|a| {
            let org = match claude::org(cfg, a) {
                claude::Org::Id(id) => Some(id),
                claude::Org::SignedOut => return None,
                claude::Org::Unknown => None,
            };
            let u = claude::usage(cfg, a, org.as_deref())?;
            let now = crate::time::now();
            if !reading_fresh(u.as_of, now) {
                println!("skipping {}: usage reading is {} min old", a.name, now.saturating_sub(u.as_of) / 60);
                return None;
            }
            // The reading is fresh, so a missing five-hour number means that window has ended: 0% used.
            Some(Cand {
                name: a.name.clone(),
                headroom: headroom(u.week, u.five_hour.unwrap_or(0)),
                age_min: Some(now.saturating_sub(u.as_of) / 60),
            })
        })
        .collect()
}

/// `Name=week/five,Name2=week/five` (percent used): fake usage for dry runs without touching any account.
pub fn parse_fake(s: &str) -> Vec<Cand> {
    s.split(',')
        .filter_map(|p| {
            let (name, v) = p.split_once('=')?;
            let (w, f) = v.split_once('/')?;
            Some(Cand { name: name.trim().to_string(), headroom: headroom(w.trim().parse().ok()?, f.trim().parse().ok()?), age_min: None })
        })
        .filter(|c| !c.name.is_empty())
        .collect()
}

/// The queue name a started chat holds the job under (it passes the same name to beat, done, release).
pub fn by_name(account: &str) -> String {
    let slug: String = account.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    format!("runner-{}", slug.trim_matches('-'))
}

/// `<id>-<slug>.md` (or `<id>.md`) in the cards folder; files starting with `_` are templates.
pub fn card_for(cards: &Path, id: &str) -> Option<PathBuf> {
    let (dash, exact) = (format!("{id}-"), format!("{id}.md"));
    let mut hits: Vec<PathBuf> = std::fs::read_dir(cards)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
            !n.starts_with('_') && n.ends_with(".md") && (n == exact || n.starts_with(&dash))
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// One line of text for the new chat: the card path first, then how to report back under the held name.
pub fn prompt(exe: &str, job: &Job, card: Option<&Path>, by: &str) -> String {
    let id = &job.id;
    let title: String = job.title.chars().filter(|c| !c.is_control()).take(120).collect();
    let lead = match card {
        Some(c) => format!("Read the Hive card at {} and do the work it describes.", c.display()),
        None => format!("Hive queue job {id}: {title}. No card file was found for it, so do the work the title describes."),
    };
    format!(
        "{lead} This is queue job {id}, held for you as {by}. Keep it alive while you work with \"{exe}\" --queue beat {id} --by {by} (at least every 20 minutes). When finished run \"{exe}\" --queue done {id} --by {by} --note \"<one line>\". To give it back run \"{exe}\" --queue release {id} --by {by}."
    )
}

pub struct Opts {
    pub once: bool,
    pub dry: bool,
}

pub struct Outcome {
    pub lines: Vec<String>,
    /// 0 ok, 1 error, 4 no todo job (or every one skipped), 5 no account has headroom.
    pub code: i32,
}

/// Everything the loop needs from outside, so tests run it with fake usage and a fake chat start.
pub struct Ctx<'a> {
    pub dir: &'a Path,
    pub cards: &'a Path,
    pub exe: &'a str,
    pub below: u8,
    pub now: u64,
    pub cands: &'a dyn Fn() -> Vec<Cand>,
    /// Opens a new chat in the named account with the prompt (the real one launches Claude Desktop).
    pub start: &'a mut dyn FnMut(&str, &str) -> Result<(), String>,
    /// Called between two real starts, so the window settles before the next link arrives.
    pub pause: &'a dyn Fn(),
}

pub fn run_jobs(ctx: Ctx, o: &Opts) -> Outcome {
    let Ctx { dir, cards, exe, below, now, cands, start, pause } = ctx;
    let mut out = Outcome { lines: Vec::new(), code: 0 };
    // Todo jobs plus claims gone stale (a draft that was never sent): those are offered again.
    let todo = queue::claimable(dir, Some("claude"), now);
    if todo.is_empty() {
        out.lines.push("no todo job".into());
        out.code = 4;
        return out;
    }
    let (mut handled, mut skipped, mut errors) = (0, 0, 0);
    let mut first_err: Option<String> = None;
    for (i, j) in todo.iter().enumerate() {
        // --once means one job actually handled, not "look at the first one only".
        if o.once && handled >= 1 {
            break;
        }
        let cs = cands();
        let Some(c) = pick(&cs, below) else {
            out.lines.push("no account has headroom".into());
            out.code = if handled == 0 { 5 } else { 0 };
            return out;
        };
        let by = by_name(&c.name);
        let p = prompt(exe, j, card_for(cards, &j.id).as_deref(), &by);
        let seen = c.age_min.map(|m| format!(", reading {m} min old")).unwrap_or_default();
        out.lines.push(format!("job {} -> account {} ({}%{seen})", j.id, c.name, c.headroom));
        if o.dry {
            out.lines.push(format!("  plan: claim {} as {by}", j.id));
            out.lines.push(format!("  plan: open a new Claude Code chat in {} (claude://code/new, prompt filled in, not sent)", c.name));
            out.lines.push(format!("  plan: prompt: {p}"));
            out.lines.push("  plan: dry run, nothing claimed or opened".into());
            handled += 1;
            continue;
        }
        match queue::claim(dir, &j.id, &by, now) {
            Ok(took_over) => {
                if took_over {
                    out.lines.push(format!("  took over a stale claim on {}", j.id));
                }
            }
            Err(QErr::Held(h)) => {
                out.lines.push(format!("  skipped: {} is held by {h}", j.id));
                skipped += 1;
                continue;
            }
            Err(e) if queue::is_gone(&e, &j.id) => {
                out.lines.push(format!("  skipped: {} ({})", j.id, describe(&e)));
                skipped += 1;
                continue;
            }
            Err(e) => {
                // A genuine problem with this one job (broken lock, disk error): note it, carry on with the rest.
                out.lines.push(format!("  claim failed on {}: {}", j.id, describe(&e)));
                first_err.get_or_insert(describe(&e));
                errors += 1;
                continue;
            }
        }
        match start(&c.name, &p) {
            Ok(()) => {
                out.lines.push(format!(
                    "  draft opened: new chat in {} with the prompt filled in; press send there (until then the job is only held, and an unsent claim goes stale after 30 minutes and is offered again)",
                    c.name
                ));
                handled += 1;
                if i + 1 < todo.len() && !(o.once && handled >= 1) {
                    pause();
                }
            }
            Err(e) => {
                let _ = queue::release(dir, &j.id, &by, now);
                out.lines.push(format!("  could not open a chat in {}: {e} (job released)", c.name));
                out.code = 1;
                return out;
            }
        }
    }
    if let Some(e) = first_err {
        out.lines.push(format!("{errors} job(s) could not be claimed (first error: {e}); {handled} handled, {skipped} skipped"));
        out.code = 1;
    } else if handled == 0 {
        out.lines.push(format!("nothing started: all {skipped} todo job(s) were skipped"));
        out.code = 4;
    }
    out
}

fn describe(e: &QErr) -> String {
    match e {
        QErr::Other(m) => m.clone(),
        QErr::Held(h) => format!("held by {h}"),
        QErr::Empty => "nothing to claim".into(),
        QErr::Usage => "bad arguments".into(),
    }
}

fn parse(args: &[String]) -> Option<Opts> {
    let mut o = Opts { once: false, dry: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--once" => o.once = true,
            "--dry" => o.dry = true,
            // Only the Claude lane can be started from here (Codex has no new-chat link).
            "--lane" if it.next().is_some_and(|l| l == "claude") => {}
            _ => return None,
        }
    }
    Some(o)
}

fn cards_dir() -> PathBuf {
    match std::env::var("HIVE_CARDS_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => match queue::dir().parent() {
            Some(p) if std::env::var("HIVE_QUEUE_DIR").is_ok() => p.join("cards"),
            _ => crate::exe_dir().join("hive-control").join("cards"),
        },
    }
}

pub fn cli(args: &[String]) {
    crate::console();
    let Some(o) = parse(args) else {
        println!("{USAGE}");
        std::process::exit(2);
    };
    let cfg = match crate::config::Config::load(&crate::config_path()) {
        Ok(c) => c,
        Err(e) => {
            println!("settings error: {e}");
            std::process::exit(1);
        }
    };
    let fake = std::env::var("HIVE_RUNNER_FAKE_USAGE").ok().filter(|s| !s.is_empty());
    let cands = || match &fake {
        Some(f) => parse_fake(f),
        None => live_candidates(&cfg),
    };
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "Hive.exe".into());
    let mut start = |account: &str, prompt: &str| -> Result<(), String> {
        let acc = cfg.accounts.iter().find(|a| a.name == account).ok_or_else(|| format!("no account {account}"))?;
        crate::claude::launch_with(&cfg, acc, &[crate::claude::new_chat_link(prompt)])
    };
    let delay = std::time::Duration::from_secs(cfg.launch_delay_seconds.max(5) as u64);
    let pause = move || std::thread::sleep(delay);
    let cards = cards_dir();
    let out = run_jobs(
        Ctx {
            dir: &queue::dir(),
            cards: &cards,
            exe: &exe,
            below: cfg.use_next_below,
            now: crate::time::now(),
            cands: &cands,
            start: &mut start,
            pause: &pause,
        },
        &o,
    );
    for l in &out.lines {
        println!("{l}");
    }
    std::process::exit(out.code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hive-runner-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn cand(n: &str, week: u8, five: u8) -> Cand {
        Cand { name: n.into(), headroom: headroom(week, five), age_min: None }
    }

    /// Runs the loop with fake usage; returns the outcome and the (account, prompt) pairs that were started.
    fn go(d: &Path, cards: &Path, cs: Vec<Cand>, below: u8, o: Opts, fail: bool) -> (Outcome, Vec<(String, String)>) {
        go_at(d, cards, cs, below, o, fail, 100, &|| {})
    }

    /// Same, with the clock set and a hook that runs each time usage is read (to stage a race before the claim).
    #[allow(clippy::too_many_arguments)]
    fn go_at(
        d: &Path,
        cards: &Path,
        cs: Vec<Cand>,
        below: u8,
        o: Opts,
        fail: bool,
        now: u64,
        on_read: &dyn Fn(),
    ) -> (Outcome, Vec<(String, String)>) {
        let mut started: Vec<(String, String)> = Vec::new();
        let mut start = |a: &str, p: &str| -> Result<(), String> {
            if fail {
                return Err("no window".into());
            }
            started.push((a.into(), p.into()));
            Ok(())
        };
        let f = move || {
            on_read();
            cs.clone()
        };
        let out = run_jobs(Ctx { dir: d, cards, exe: "Hive.exe", below, now, cands: &f, start: &mut start, pause: &|| {} }, &o);
        (out, started)
    }

    #[test]
    fn picks_the_most_headroom_and_skips_past_the_line() {
        let cs = vec![cand("A", 40, 10), cand("B", 20, 55), cand("C", 70, 0), cand("D", 96, 0)];
        // Headroom: A 60, B 45, C 30, D 4.
        assert_eq!(pick(&cs, 95).unwrap().name, "A");
        assert_eq!(pick(&cs[1..], 95).unwrap().name, "B");
        // D is past a 90% line, C is not.
        assert_eq!(pick(&cs[2..], 90).unwrap().name, "C");
        assert!(pick(&cs[3..], 90).is_none());
        // Tie: the earlier account wins. Line off (0) allows anything with room.
        assert_eq!(pick(&[cand("X", 50, 0), cand("Y", 0, 50)], 95).unwrap().name, "X");
        assert_eq!(pick(&cs[3..], 0).unwrap().name, "D");
        assert!(pick(&[cand("Z", 100, 0)], 0).is_none());
        assert!(pick(&[], 95).is_none());
    }

    #[test]
    fn fake_usage_parses() {
        let c = parse_fake("Personal=20/10, Spare=60/75,junk,=1/2,Bad=x/1");
        assert_eq!(c, vec![Cand { name: "Personal".into(), headroom: 80, age_min: None }, Cand { name: "Spare".into(), headroom: 25, age_min: None }]);
        assert_eq!(by_name("Spare 2!"), "runner-spare-2");
    }

    #[test]
    fn dry_run_prints_the_plan_and_changes_nothing() {
        let d = tdir("dry");
        let cards = d.join("cards");
        fs::create_dir_all(&cards).unwrap();
        fs::write(cards.join("7-thing.md"), "x").unwrap();
        queue::add(&d, "7", "Do the thing", "claude", 1).unwrap();
        queue::add(&d, "8", "No card", "claude", 2).unwrap();
        queue::add(&d, "9", "Codex job", "codex", 3).unwrap();
        let cs = vec![cand("Personal", 30, 10), cand("Spare", 10, 5)];
        let (out, started) = go(&d, &cards, cs.clone(), 95, Opts { once: true, dry: true }, false);
        assert_eq!(out.code, 0);
        assert_eq!(out.lines[0], "job 7 -> account Spare (90%)");
        assert!(out.lines.iter().any(|l| l.contains("plan: claim 7 as runner-spare")));
        assert!(out.lines.iter().any(|l| l.contains("plan: prompt: Read the Hive card at") && l.contains("7-thing.md")));
        assert!(started.is_empty());
        // Nothing claimed: still todo, no claim file.
        assert!(queue::list(&d, false, 100)[0].contains("todo"));
        assert!(!d.join("7.claim").exists());
        // Without --once every claude todo job gets a line, in order; the codex lane is left alone.
        let (out, _) = go(&d, &cards, cs, 95, Opts { once: false, dry: true }, false);
        let jobs: Vec<&String> = out.lines.iter().filter(|l| l.starts_with("job ")).collect();
        assert_eq!(jobs.len(), 2);
        assert!(jobs[1].starts_with("job 8 -> account Spare"));
        assert!(out.lines.iter().any(|l| l.contains("No card file was found")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn real_run_claims_then_starts_the_oldest_job_only_with_once() {
        let d = tdir("once");
        let cards = d.join("cards");
        queue::add(&d, "b", "Second", "claude", 2).unwrap();
        queue::add(&d, "a", "First", "claude", 1).unwrap();
        let (out, started) = go(&d, &cards, vec![cand("Personal", 30, 10)], 95, Opts { once: true, dry: false }, false);
        assert_eq!(out.code, 0, "{:?}", out.lines);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, "Personal");
        assert!(started[0].1.contains("queue job a") && started[0].1.contains("--by runner-personal"));
        let l = queue::list(&d, false, 100);
        assert!(l[0].starts_with("a claimed runner-personal"), "{l:?}");
        assert!(l[1].starts_with("b todo"), "{l:?}");
        // A second run takes the next job; the first stays held.
        let (_, started) = go(&d, &cards, vec![cand("Personal", 30, 10)], 95, Opts { once: true, dry: false }, false);
        assert_eq!(started.len(), 1);
        assert!(started[0].1.contains("queue job b"));
        let (out, started) = go(&d, &cards, vec![cand("Personal", 30, 10)], 95, Opts { once: true, dry: false }, false);
        assert_eq!((out.code, started.len()), (4, 0));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn without_once_every_todo_job_is_started() {
        let d = tdir("all");
        let cards = d.join("cards");
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            queue::add(&d, id, id, "claude", i as u64).unwrap();
        }
        let (out, started) = go(&d, &cards, vec![cand("Personal", 30, 10)], 95, Opts { once: false, dry: false }, false);
        assert_eq!((out.code, started.len()), (0, 3));
        assert_eq!(queue::list(&d, false, 100).iter().filter(|l| l.contains("claimed")).count(), 3);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_start_releases_the_job() {
        let d = tdir("fail");
        queue::add(&d, "a", "A", "claude", 1).unwrap();
        let (out, started) = go(&d, &d.join("cards"), vec![cand("Personal", 30, 10)], 95, Opts { once: true, dry: false }, true);
        assert_eq!((out.code, started.len()), (1, 0));
        assert!(out.lines.iter().any(|l| l.contains("job released")));
        assert!(queue::list(&d, false, 100)[0].starts_with("a todo"));
        // Free to claim again.
        assert_eq!(queue::claim(&d, "a", "other", 101), Ok(false));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_account_with_headroom_starts_nothing() {
        let d = tdir("full");
        queue::add(&d, "a", "A", "claude", 1).unwrap();
        let (out, started) = go(&d, &d.join("cards"), vec![cand("Personal", 97, 0)], 95, Opts { once: true, dry: false }, false);
        assert_eq!((out.code, started.len()), (5, 0));
        assert_eq!(out.lines, vec!["no account has headroom".to_string()]);
        assert!(queue::list(&d, false, 100)[0].starts_with("a todo"));
        let (out, _) = go(&d, &d.join("cards"), vec![], 95, Opts { once: true, dry: false }, false);
        assert_eq!(out.code, 5);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_queue_and_held_jobs() {
        let d = tdir("empty");
        let cs = vec![cand("Personal", 30, 10)];
        assert_eq!(go(&d, &d.join("cards"), cs.clone(), 95, Opts { once: false, dry: false }, false).0.code, 4);
        queue::add(&d, "a", "A", "claude", 1).unwrap();
        queue::claim(&d, "a", "someone", crate::time::now()).unwrap();
        // Claimed jobs are not todo, so the runner never touches them.
        assert_eq!(go(&d, &d.join("cards"), cs, 95, Opts { once: false, dry: false }, false).0.code, 4);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_claim_is_offered_again_but_a_live_one_is_not() {
        let d = tdir("stale");
        let cards = d.join("cards");
        let cs = vec![cand("Personal", 30, 10)];
        queue::add(&d, "a", "A", "claude", 1).unwrap();
        // The draft was opened at t=0 and never sent.
        queue::claim(&d, "a", "runner-personal", 0).unwrap();
        // 10 minutes later: still held, nothing to do.
        assert_eq!(go_at(&d, &cards, cs.clone(), 95, Opts { once: true, dry: false }, false, 600, &|| {}).0.code, 4);
        // Past 30 minutes: taken over and started again.
        let (out, started) = go_at(&d, &cards, cs, 95, Opts { once: true, dry: false }, false, 2000, &|| {});
        assert_eq!((out.code, started.len()), (0, 1), "{:?}", out.lines);
        assert!(out.lines.iter().any(|l| l.contains("took over a stale claim")));
        assert!(queue::list(&d, false, 2000)[0].starts_with("a claimed runner-personal"));
        // `next` sees a stale claim too.
        let d2 = tdir("stale-next");
        queue::add(&d2, "a", "A", "claude", 1).unwrap();
        queue::claim(&d2, "a", "old", 0).unwrap();
        assert_eq!(queue::next(&d2, "new", Some("claude"), 100), Err(QErr::Empty));
        assert_eq!(queue::next(&d2, "new", Some("claude"), 2000), Ok("a".to_string()));
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn once_counts_handled_jobs_not_the_first_index() {
        let d = tdir("race");
        let cards = d.join("cards");
        queue::add(&d, "a", "A", "claude", 1).unwrap();
        queue::add(&d, "b", "B", "claude", 2).unwrap();
        // Another process takes `a` after the runner listed the jobs but before it claims.
        let dd = d.clone();
        let steal = move || {
            let _ = queue::claim(&dd, "a", "other", 100);
        };
        let (out, started) = go_at(&d, &cards, vec![cand("P", 30, 10)], 95, Opts { once: true, dry: false }, false, 100, &steal);
        assert_eq!(out.code, 0, "{:?}", out.lines);
        assert_eq!(started.len(), 1);
        assert!(started[0].1.contains("queue job b"));
        assert!(out.lines.iter().any(|l| l.contains("skipped: a is held by other")));
        // Only held jobs: nothing started is a non-zero exit.
        let d2 = tdir("race2");
        queue::add(&d2, "a", "A", "claude", 1).unwrap();
        let dd = d2.clone();
        let steal = move || {
            let _ = queue::claim(&dd, "a", "other", 100);
        };
        let (out, started) = go_at(&d2, &cards, vec![cand("P", 30, 10)], 95, Opts { once: true, dry: false }, false, 100, &steal);
        assert_eq!((out.code, started.len()), (4, 0), "{:?}", out.lines);
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn a_job_that_finished_meanwhile_does_not_stop_the_run() {
        let d = tdir("gone");
        let cards = d.join("cards");
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            queue::add(&d, id, id, "claude", i as u64).unwrap();
        }
        let dd = d.clone();
        let finish = move || {
            if queue::claim(&dd, "a", "other", 100).is_ok() {
                let _ = queue::done(&dd, "a", "other", "x", 100);
            }
        };
        let (out, started) = go_at(&d, &cards, vec![cand("P", 30, 10)], 95, Opts { once: false, dry: false }, false, 100, &finish);
        assert_eq!((out.code, started.len()), (0, 2), "{:?}", out.lines);
        assert!(out.lines.iter().any(|l| l.contains("skipped: a (job a is done)")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn one_broken_job_does_not_block_the_others() {
        let d = tdir("broken");
        let cards = d.join("cards");
        queue::add(&d, "a", "A", "claude", 1).unwrap();
        queue::add(&d, "b", "B", "claude", 2).unwrap();
        // A stray directory where the lock file should be: every claim on `a` fails.
        fs::create_dir_all(d.join("a.lock")).unwrap();
        let (out, started) = go(&d, &cards, vec![cand("P", 30, 10)], 95, Opts { once: false, dry: false }, false);
        assert_eq!(started.len(), 1, "{:?}", out.lines);
        assert!(started[0].1.contains("queue job b"));
        assert_eq!(out.code, 1);
        assert!(out.lines.iter().any(|l| l.contains("claim failed on a")));
        assert!(out.lines.last().unwrap().contains("1 job(s) could not be claimed"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn old_usage_readings_do_not_count() {
        assert!(reading_fresh(1000, 1000 + MAX_READING_SECS));
        assert!(!reading_fresh(1000, 1001 + MAX_READING_SECS));
        assert!(reading_fresh(5000, 100));
        let mut c = cand("Old", 20, 0);
        c.age_min = Some(7);
        let d = tdir("age");
        queue::add(&d, "a", "A", "claude", 1).unwrap();
        let (out, _) = go(&d, &d.join("cards"), vec![c], 95, Opts { once: true, dry: true }, false);
        assert_eq!(out.lines[0], "job a -> account Old (80%, reading 7 min old)");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn args_and_card_lookup() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(parse(&s(&[])).is_some());
        let o = parse(&s(&["--lane", "claude", "--once", "--dry"])).unwrap();
        assert!(o.once && o.dry);
        for bad in [&["--lane", "codex"][..], &["--lane"], &["--bogus"], &["extra"]] {
            assert!(parse(&s(bad)).is_none(), "{bad:?}");
        }
        let d = tdir("cards");
        fs::write(d.join("12-alpha.md"), "").unwrap();
        fs::write(d.join("125-queue-runner.md"), "").unwrap();
        fs::write(d.join("_template.md"), "").unwrap();
        assert!(card_for(&d, "12").unwrap().ends_with("12-alpha.md"));
        assert!(card_for(&d, "125").unwrap().ends_with("125-queue-runner.md"));
        assert!(card_for(&d, "1").is_none());
        assert!(card_for(&d, "_template").is_none());
        assert!(card_for(&d.join("none"), "12").is_none());
        let _ = fs::remove_dir_all(&d);
    }
}
