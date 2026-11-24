//! Demo mode (`--demo`): the real interface filled with made-up accounts,
//! usage and chats, for screenshots that show nobody's real data. Nothing
//! else changes: the settings screen still shows the real settings, except
//! the Drive folder, which is replaced with a sample path.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::time;
use crate::ui::{ChatItem, Kind, Opts, Row, Snapshot, Tone};

static ON: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ON.store(true, Ordering::SeqCst);
}

pub fn on() -> bool {
    ON.load(Ordering::SeqCst)
}

const DAY: u64 = 86_400;

fn row(name: &str, plan: &str, gb: &str, week: u8, five: Option<u8>, reset_in: u64, approx: bool, trend: &[u8]) -> Row {
    let at = time::now() + reset_in;
    let tone = if week >= 100 { Tone::Bad } else if week >= 80 { Tone::Warn } else { Tone::Normal };
    let unc = if approx { 3 * 3600 } else { 0 };
    Row {
        kind: Kind::Claude,
        name: name.into(),
        status: format!("{plan}  \u{00B7}  {gb}"),
        running: true,
        signed_out: false,
        chrome: name == "Work",
        bars: vec![("5h".into(), five), ("Week".into(), Some(week))],
        note: Some((format!("Resets {} (in {})", time::when(at, unc), time::span(reset_in)), tone)),
        best: false,
        trend: trend.to_vec(),
    }
}

/// Sample accounts. "Work" resets soonest while under the line, so it is "Use next".
pub fn snapshot(mut opts: Opts, pinned: bool, pinned_at: Option<[i32; 2]>) -> Snapshot {
    let mut rows = vec![
        row("Work", "Acme Studio (Team)", "3.1 GB", 64, Some(12), DAY + 7 * 3600, false, &[10, 18, 25, 33, 41, 55, 64]),
        row("Personal", "Max plan", "2.4 GB", 81, None, 3 * DAY + 5 * 3600, true, &[22, 30, 44, 52, 60, 73, 81]),
        row("Side project", "Pro plan", "1.2 GB", 100, None, 5 * DAY + 2 * 3600, false, &[40, 58, 70, 82, 91, 97, 100]),
        row("Freelance", "Max plan", "0.9 GB", 9, Some(3), 6 * DAY, false, &[0, 0, 2, 4, 4, 7, 9]),
    ];
    rows[0].best = true;
    let codex_reset = 4 * DAY + 9 * 3600;
    rows.push(Row {
        kind: Kind::Codex,
        name: "Codex".into(),
        status: "Pro plan  \u{00B7}  48,200 credits".into(),
        running: true,
        signed_out: false,
        chrome: false,
        bars: vec![("Week".into(), Some(37))],
        note: Some((format!("Week resets {} (in {})", time::when(time::now() + codex_reset, 0), time::span(codex_reset)), Tone::Normal)),
        best: false,
        trend: vec![],
    });
    opts.drive_path = "G:\\My Drive\\Projects".into();
    let names = ["Work", "Personal", "Side project", "Freelance"];
    Snapshot {
        rows,
        waiting: None,
        drive: Some(true),
        best: Some("Work".into()),
        pinned,
        pinned_at,
        palette: crate::ui::avatar_palette(names.into_iter()),
        opts,
    }
}

/// Sample chats for the search screen.
pub fn chats() -> Vec<ChatItem> {
    let now = time::now();
    let c = |account: &str, title: &str, ago: u64| ChatItem {
        account: account.into(),
        id: format!("local_demo{}", title.len()),
        title: title.into(),
        last: now.saturating_sub(ago),
        archived: false,
        cli: String::new(),
    };
    vec![
        c("Work", "Fix flaky checkout test on Windows", 600),
        c("Personal", "Plan the garden irrigation timer", 3_600),
        c("Work", "Write release notes for 2.4", 7_200),
        c("Side project", "Add dark mode to the settings page", 20_000),
        c("Freelance", "Migrate invoices to the new API", 50_000),
        c("Work", "Speed up the image upload path", 90_000),
        c("Personal", "Summarize the lease renewal terms", 200_000),
        c("Side project", "Fix the release build on CI", 400_000),
        c("Freelance", "Draft the onboarding email sequence", 600_000),
    ]
}
