//! Setup check: problems that are easy to miss. Read only; runs when the tray
//! starts, from Settings > Check setup, and in --status.

use crate::claude::{self, Org};
use crate::config::{Account, Config};

/// The Claude login a profile last used (its account id), from its config.json. No secrets are read.
fn login_of(cfg: &Config, a: &Account) -> Option<String> {
    let text = std::fs::read_to_string(claude::data_dir(cfg, a).join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    v.get("lastKnownAccountUuid")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// One line per problem; empty when all is well.
pub fn problems(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if claude::claude_exe(cfg).is_none() {
        out.push("Claude Desktop wasn't found. Install or repair it from the Microsoft Store.".to_string());
    }
    // Same login AND same org: the two windows are really one account (one set of limits).
    // Same login in different orgs (a personal plan and a team seat) is normal.
    let ids: Vec<(&str, Option<String>, Option<String>)> = cfg
        .accounts
        .iter()
        .map(|a| {
            let org = match claude::org(cfg, a) {
                Org::Id(id) => Some(id),
                Org::SignedOut => {
                    out.push(format!("{} is signed out. Open it and sign in.", a.name));
                    None
                }
                Org::Unknown => None,
            };
            (a.name.as_str(), login_of(cfg, a), org)
        })
        .collect();
    for (i, (a, la, oa)) in ids.iter().enumerate() {
        for (b, lb, ob) in &ids[i + 1..] {
            if la.is_some() && la == lb && oa.is_some() && oa == ob {
                out.push(format!(
                    "{a} and {b} are signed in to the same Claude login and org, so they share one set of usage limits. Sign one of them out and in to its own login."
                ));
            }
        }
    }
    for a in &cfg.accounts {
        let org = match claude::org(cfg, a) {
            Org::Id(id) => Some(id),
            _ => None,
        };
        if let Some(u) = claude::usage(cfg, a, org.as_deref()) {
            let age = crate::time::now().saturating_sub(u.as_of);
            if age > crate::time::DAY {
                out.push(format!(
                    "{}'s usage numbers are {} old: Claude paused its usage check. Close {} and open it from this app to start it again.",
                    a.name,
                    crate::time::span(age),
                    a.name
                ));
            }
        }
    }
    if let Some(helper) = crate::codex::missing_helper(cfg) {
        out.push(format!(
            "Codex alerts pass Codex's signal on to {helper}, which no longer exists (Codex probably moved it in an update). Turn Codex alerts off and on in Settings to repair."
        ));
    }
    if !cfg.chrome_account.is_empty() && cfg.find(&cfg.chrome_account).is_none() {
        out.push(format!("Claude in Chrome is set to hand back to \"{}\", which isn't in the list.", cfg.chrome_account));
    }
    out
}

/// The problems plus the memory check's line (reading every note takes about half a
/// second, so the check when the tray starts leaves it out and never raises a card for it).
pub fn all(cfg: &Config) -> Vec<String> {
    let mut out = problems(cfg);
    out.extend(crate::memory::check_problem());
    out
}

// ---- Hive.exe --doctor: Codex alerts -------------------------------------------------

/// Codex turns that got no card, from hive.log lines at or after `cutoff` ("YYYY-MM-DD HH:MM:SS",
/// local): the count and the reasons, most common first. A project's first no-card turn each hour
/// is one line; the next hour's summary line carries the hour's total, so it adds total - 1.
/// A summary lists its reasons without counts, so the extra turns go to its first reason.
pub fn no_card_from_log(text: &str, cutoff: &str) -> (u32, Vec<(String, u32)>) {
    let mut total = 0u32;
    let mut reasons: Vec<(String, u32)> = Vec::new();
    let mut add = |why: &str, n: u32| {
        total += n;
        match reasons.iter_mut().find(|(r, _)| r == why) {
            Some(e) => e.1 += n,
            None => reasons.push((why.to_string(), n)),
        }
    };
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}');
        let (Some(at), Some(rest)) = (line.get(..19), line.get(20..)) else { continue };
        if at < cutoff {
            continue;
        }
        let Some(rest) = rest.strip_prefix("codex: ") else { continue };
        if rest.starts_with("no card for a background turn") {
            add("background turn", 1);
        } else if let Some(r) = rest.strip_prefix("no card for a turn in ") {
            let r = r.strip_suffix("); further turns this hour are counted, not logged").or_else(|| r.strip_suffix(')'));
            if let Some(why) = r.and_then(|r| r.rsplit_once(" (")).map(|(_, w)| w) {
                add(why, 1);
            }
        } else if let Some((n, r)) = rest.split_once(" turns with no card in ") {
            let (Ok(n), Some((_, list))) = (n.parse::<u32>(), r.strip_suffix(')').and_then(|r| r.rsplit_once(" ("))) else { continue };
            let first = list.split("; ").next().unwrap_or(list);
            add(first, n.saturating_sub(1));
        }
    }
    reasons.retain(|(_, n)| *n > 0);
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    (total, reasons)
}

/// Turns counted in this hour's `hive-codex-nocard.json` but not logged yet (all but each project's
/// first), with each project's first reason.
fn pending_no_card(text: &str, now: u64) -> Vec<(String, u32)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let Some(map) = v.as_object() else { return Vec::new() };
    map.values()
        .filter(|e| now.saturating_sub(e["since"].as_u64().unwrap_or(0)) < crate::time::DAY)
        .filter_map(|e| {
            let n = e["count"].as_u64()?.saturating_sub(1) as u32;
            let why = e["reasons"].get(0)?.as_str()?.to_string();
            (n > 0).then_some((why, n))
        })
        .collect()
}

fn stamp(secs: u64) -> String {
    let t = crate::time::local(secs);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// The Codex section as lines of text: read only.
pub fn codex_report(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let home = crate::codex::home(cfg);
    let me = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let file = crate::codex::config_toml(cfg);
    out.push(format!("Codex settings:    {}", file.display()));
    let (status, fixable) = match crate::codex::notify_check(cfg) {
        None => ("not found (open Codex once first)".to_string(), false),
        Some(crate::codex::NotifyStatus::Ok) => (format!("notify points at this Hive ({me})"), false),
        Some(crate::codex::NotifyStatus::Old(p)) => (format!("notify names an old exe ({p}); this one is {me}"), true),
        Some(crate::codex::NotifyStatus::Missing) => ("notify does not call Hive, so Codex turns give no alerts".to_string(), true),
        Some(crate::codex::NotifyStatus::Unreadable) => ("notify is written in a form Hive won't edit; change it by hand".to_string(), false),
    };
    out.push(format!("Codex notify:      {status}"));
    let hooks = home.join("hooks.json");
    out.push(format!("Codex hooks file:  {}", if hooks.exists() { format!("found ({})", hooks.display()) } else { format!("missing ({})", hooks.display()) }));
    let events = crate::events::read_recent();
    let last = events.iter().filter(|e| e.account == "Codex" || cfg.find_codex(&e.account).is_some()).map(|e| e.at).max();
    out.push(format!("Last Codex event:  {}", last.map_or("none in the kept events".to_string(), |at| format!("{} ({})", crate::time::ago(at), stamp(at)))));
    let now = crate::time::now();
    let log = crate::log_path();
    let mut text = std::fs::read_to_string(log.with_extension("log.old")).unwrap_or_default();
    text.push('\n');
    text.push_str(&std::fs::read_to_string(&log).unwrap_or_default());
    let cutoff = stamp(now.saturating_sub(crate::time::DAY));
    let window = match oldest_stamp(&text) {
        Some(first) if first > cutoff.as_str() => format!("since {first}"),
        _ => "in the last 24 h".to_string(),
    };
    let (mut n, mut reasons) = no_card_from_log(&text, &cutoff);
    for (why, k) in pending_no_card(&std::fs::read_to_string(crate::data_file("hive-codex-nocard.json")).unwrap_or_default(), now) {
        n += k;
        match reasons.iter_mut().find(|(r, _)| *r == why) {
            Some(e) => e.1 += k,
            None => reasons.push((why, k)),
        }
    }
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if n == 0 {
        out.push(format!("No-card turns:     none {window}"));
    } else {
        let top: Vec<String> = reasons.iter().take(3).map(|(r, k)| format!("{r}: {k}")).collect();
        out.push(format!("No-card turns:     {n} {window}. Top reasons: {}", top.join("; ")));
    }
    if fixable {
        out.push("To fix, add this line near the top of that file, or run: Hive.exe --doctor --fix-codex".to_string());
        out.push(format!("  {}", crate::codex::notify_line_to_add(cfg)));
    }
    out
}

/// `Hive.exe --doctor [--fix-codex]`.
pub fn cli(args: &[String], cfg: &Config) -> bool {
    for l in codex_report(cfg) {
        println!("{l}");
    }
    if args.iter().any(|a| a.eq_ignore_ascii_case("--fix-codex")) {
        match crate::codex::fix_notify(cfg) {
            Ok(Some(b)) => println!("Fixed: notify now calls this Hive. Backup: {}", b.display()),
            Ok(None) => println!("Nothing to fix: notify already points at this Hive."),
            Err(e) => {
                println!("Couldn't fix it: {e}");
                return false;
            }
        }
    }
    true
}

/// The first timestamp in the log text ("YYYY-MM-DD HH:MM:SS"), i.e. how far back it reaches.
fn oldest_stamp(text: &str) -> Option<&str> {
    text.lines().map(|l| l.trim_start_matches('\u{feff}')).filter_map(|l| l.get(..19)).find(|t| {
        t.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            10 => b == b' ',
            13 | 16 => b == b':',
            _ => b.is_ascii_digit(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_card_lines_are_counted_with_top_reasons() {
        let log = "\u{feff}2026-11-22 01:00:00 codex: no card for a turn in E:\\a (b) (subagent, client x); further turns this hour are counted, not logged\n\
2026-11-23 10:00:00 codex: no card for a turn in E:\\a (subagent, client x); further turns this hour are counted, not logged\n\
2026-11-23 11:00:00 codex: 5 turns with no card in E:\\a in the last hour (subagent, client x; exec)\n\
2026-11-23 11:30:00 codex: no card for a background turn in E:\\a (client c, asked \"q\")\n\
2026-11-23 12:00:00 something else\n";
        let (n, r) = no_card_from_log(log, "2026-11-22 12:00:00");
        assert_eq!(n, 6, "1 logged + 4 extra + 1 background; the older line is out");
        assert_eq!(r[0], ("subagent, client x".to_string(), 5));
        assert_eq!(r[1], ("background turn".to_string(), 1));
        let (all, _) = no_card_from_log(log, "2026-11-01 00:00:00");
        assert_eq!(all, 7);
    }

    #[test]
    fn the_oldest_log_line_gives_the_window() {
        assert_eq!(oldest_stamp("\u{feff}2026-11-23 01:02:03 a\n2026-11-23 05:00:00 b"), Some("2026-11-23 01:02:03"));
        assert_eq!(oldest_stamp("junk\n"), None);
    }

    #[test]
    fn pending_counts_skip_each_first_turn_and_old_hours() {
        let now = crate::time::now();
        let t = format!(
            "{{\"a\":{{\"since\":{now},\"count\":4,\"reasons\":[\"subagent\"]}},\"o\":{{\"since\":1,\"count\":9,\"reasons\":[\"x\"]}},\"one\":{{\"since\":{now},\"count\":1,\"reasons\":[\"y\"]}}}}"
        );
        assert_eq!(pending_no_card(&t, now), vec![("subagent".to_string(), 3)]);
        assert!(pending_no_card("junk", now).is_empty());
    }

    #[test]
    fn report_on_a_temp_codex_home_names_the_problem_and_the_fix_line() {
        let d = std::env::temp_dir().join(format!("hive-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("config.toml"), "model = \"m\"\n").unwrap();
        let mut cfg = Config::default();
        cfg.codex.home = d.display().to_string();
        let r = codex_report(&cfg).join("\n");
        assert!(r.contains("does not call Hive"), "{r}");
        assert!(r.contains("hooks file:  missing"), "{r}");
        assert!(r.contains("--fix-codex") && r.contains("notify = ["), "{r}");
        assert_eq!(std::fs::read_to_string(d.join("config.toml")).unwrap(), "model = \"m\"\n", "report never writes");
        let _ = std::fs::remove_dir_all(&d);
    }
}
