//! The flyout's "Waiting" list: every chat that needs you or finished and was not opened yet,
//! newest first, with worker chats ("card 121: ...") grouped under their coordinator.
//!
//! The list is built from what Hive already records (the hook events file, the snooze file and
//! the chats' own records), through a cache that reloads at most every 5 s. Painting only reads
//! the cached `Vec<Item>`; it does no file I/O.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::events::{self, Event};

/// Rows shown at most (group headers count); the rest become "N more".
pub const MAX_ROWS: usize = 12;
/// The cache reloads at most this often.
pub const REFRESH: Duration = Duration::from_secs(5);
/// A cached chat title is read again after this long (the chat may be renamed).
const TITLE_TTL: Duration = Duration::from_secs(60);
/// A finished chat stays listed this long if it is never opened (a question or needs-you: `events::REPLAY_SECS`).
const DONE_SECS: u64 = 3 * 3600;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Why {
    /// A finished turn ended on a question.
    Ask,
    /// Claude needs you (permission, input).
    Needs,
    /// A finished turn.
    Done,
}

impl Why {
    pub fn label(self) -> &'static str {
        match self {
            Why::Ask => "question",
            Why::Needs => "needs you",
            Why::Done => "finished",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Quiet {
    No,
    Snoozed,
    Muted,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub account: String,
    /// The id a chat link takes (Code tab id, else the session id); "" when it has none.
    pub chat: String,
    /// `events::chat_key`: what a dismissal is stored under.
    pub key: String,
    pub title: String,
    pub why: Why,
    /// Unix seconds of its newest event.
    pub at: u64,
    pub quiet: Quiet,
}

impl Item {
    /// Only chats with an id can be opened from here.
    pub fn openable(&self) -> bool {
        !self.chat.is_empty()
    }
}

/// "121" for a title like "card 121: Flyout waiting list".
pub fn card_id(title: &str) -> Option<&str> {
    let t = title.trim_start();
    let head = t.get(..5)?;
    if !head.eq_ignore_ascii_case("card ") {
        return None;
    }
    let rest = &t[5..];
    let (id, _) = rest.split_once(':')?;
    (!id.is_empty() && id.len() <= 8 && id.starts_with(|c: char| c.is_ascii_digit()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then_some(id)
}

/// The waiting chats from the recent events: newest event per chat, nudges ignored, newest first.
/// `opened` says you opened the chat after the event; `dismissed` is chat key to the time you cleared it.
pub fn build(
    events: &[Event],
    now: u64,
    dismissed: &HashMap<String, u64>,
    opened: &dyn Fn(&Event) -> bool,
    title: &dyn Fn(&Event) -> String,
    quiet: &dyn Fn(&str, &str) -> Quiet,
    goal_shown: &dyn Fn(&Event) -> bool,
) -> Vec<Item> {
    let mut newest: HashMap<String, &Event> = HashMap::new();
    for ev in events.iter().filter(|e| !events::idle_nudge(e)) {
        let k = events::chat_key(ev);
        if newest.get(&k).map_or(true, |n| ev.at >= n.at) {
            newest.insert(k, ev);
        }
    }
    let mut out: Vec<Item> = Vec::new();
    for (key, ev) in newest {
        let why = match ev.kind.as_str() {
            "ask" => Why::Ask,
            "notify" => Why::Needs,
            "stop" | "codex" => Why::Done,
            _ => continue,
        };
        let age = now.saturating_sub(ev.at);
        let limit = if why == Why::Done { DONE_SECS } else { events::REPLAY_SECS };
        if age > limit || dismissed.get(&key).is_some_and(|&at| ev.at <= at) || opened(ev) || !goal_shown(ev) {
            continue; // (a goal chat that goes on by itself is not waiting)
        }
        let chat = if !ev.session.is_empty() { ev.session.clone() } else { ev.chat.clone() };
        let account = if ev.account.is_empty() { "Claude".to_string() } else { ev.account.clone() };
        out.push(Item { quiet: quiet(&ev.account, &chat), title: title(ev), account, chat, key, why, at: ev.at });
    }
    out.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.key.cmp(&b.key)));
    out
}

/// One line of the drawn list.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// An item (index into the list) and whether it is indented under the row or header above.
    Chat { idx: usize, child: bool },
    /// A header for worker chats whose coordinator is not waiting (its account).
    Group(String),
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub entries: Vec<Entry>,
    /// Chats left out by the row cap.
    pub more: usize,
}

/// A live coordinator: not a card chat, and not a retired one ("Old Hive coordinator 3").
fn is_coordinator(i: &Item) -> bool {
    let t = i.title.trim_start().to_ascii_lowercase();
    card_id(&i.title).is_none() && !t.starts_with("old ") && t.contains("coordinator")
}

/// Order and group `items` (newest first): a `card <id>:` chat goes under the newest waiting
/// coordinator of its account, or under a header for its account when none waits. Groups sit
/// where their newest member is. At most `MAX_ROWS` lines; the rest are counted in `more`.
/// The session data Hive reads has no parent link, so grouping goes by title only.
pub fn plan(items: &[Item]) -> Plan {
    // (index of the newest member, entries)
    let mut groups: Vec<(usize, Vec<Entry>)> = Vec::new();
    let mut lead_of: HashMap<usize, usize> = HashMap::new(); // worker idx -> leader idx
    for (i, it) in items.iter().enumerate() {
        if card_id(&it.title).is_some() {
            if let Some(l) = items.iter().position(|l| l.account == it.account && is_coordinator(l)) {
                lead_of.insert(i, l);
            }
        }
    }
    let mut orphan_group: HashMap<&str, usize> = HashMap::new(); // account -> index in groups
    for (i, it) in items.iter().enumerate() {
        if card_id(&it.title).is_some() {
            if let Some(&l) = lead_of.get(&i) {
                let g = groups.iter_mut().find(|(_, e)| matches!(e[0], Entry::Chat { idx, .. } if idx == l));
                if let Some((first, e)) = g {
                    *first = (*first).min(i);
                    e.push(Entry::Chat { idx: i, child: true });
                } else {
                    groups.push((i.min(l), vec![Entry::Chat { idx: l, child: false }, Entry::Chat { idx: i, child: true }]));
                }
            } else if let Some(&g) = orphan_group.get(it.account.as_str()) {
                groups[g].1.push(Entry::Chat { idx: i, child: true });
            } else {
                orphan_group.insert(&it.account, groups.len());
                groups.push((i, vec![Entry::Group(it.account.clone()), Entry::Chat { idx: i, child: true }]));
            }
        } else if !groups.iter().any(|(_, e)| matches!(e[0], Entry::Chat { idx, .. } if idx == i)) {
            groups.push((i, vec![Entry::Chat { idx: i, child: false }]));
        }
    }
    groups.sort_by_key(|(first, _)| *first);
    let all: Vec<Entry> = groups.into_iter().flat_map(|(_, e)| e).collect();
    let mut entries: Vec<Entry> = all.iter().take(MAX_ROWS).cloned().collect();
    // A header with nothing under it is not worth a row.
    if matches!(entries.last(), Some(Entry::Group(_))) {
        entries.pop();
    }
    let shown = entries.iter().filter(|e| matches!(e, Entry::Chat { .. })).count();
    let total = all.iter().filter(|e| matches!(e, Entry::Chat { .. })).count();
    Plan { entries, more: total - shown }
}

/// Where keyboard focus can sit in the flyout list: "Dismiss all", a waiting chat (by its
/// dismissal key, so a refresh that reorders the list keeps the focus on the same chat), or an
/// account row (index into the snapshot rows).
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    Dismiss,
    Chat(String),
    Row(usize),
}

/// Tab order top to bottom: "Dismiss all" (only when something waits), the drawn waiting rows in
/// plan order, then the `rows` account rows.
pub fn slots(items: &[Item], plan: &Plan, rows: usize) -> Vec<Slot> {
    let mut out = Vec::new();
    if !items.is_empty() {
        out.push(Slot::Dismiss);
    }
    for e in &plan.entries {
        if let Entry::Chat { idx, .. } = e {
            if let Some(it) = items.get(*idx) {
                out.push(Slot::Chat(it.key.clone()));
            }
        }
    }
    out.extend((0..rows).map(Slot::Row));
    out
}

/// The slot after (or before, with `back`) `cur`, wrapping. Nothing focused starts at the first
/// slot going forward and the last going back; a focus that is no longer in the list does the same.
pub fn step(order: &[Slot], cur: Option<&Slot>, back: bool) -> Option<Slot> {
    let n = order.len();
    if n == 0 {
        return None;
    }
    let at = cur.and_then(|c| order.iter().position(|s| s == c));
    let i = match (at, back) {
        (None, false) => 0,
        (None, true) => n - 1,
        (Some(i), false) => (i + 1) % n,
        (Some(i), true) => (i + n - 1) % n,
    };
    Some(order[i].clone())
}

/// Index in `items` of the waiting chat a slot stands for.
pub fn index_of(items: &[Item], slot: &Slot) -> Option<usize> {
    match slot {
        Slot::Chat(k) => items.iter().position(|i| &i.key == k),
        _ => None,
    }
}

/// Where focus goes after the chat `key` is dismissed: the next drawn waiting row, else the
/// previous one, else "Dismiss all" is gone too and focus drops to the first account row.
pub fn focus_after_dismiss(order: &[Slot], key: &str, rows: usize) -> Option<Slot> {
    let at = order.iter().position(|s| matches!(s, Slot::Chat(k) if k == key))?;
    let alive = |s: &&Slot| !matches!(s, Slot::Chat(k) if k == key);
    let next = order[at + 1..].iter().find(alive).cloned();
    let prev = || order[..at].iter().rev().find(alive).cloned();
    let pick = next.or_else(prev);
    match pick {
        // Only "Dismiss all" is left above and nothing waits any more: skip to the rows.
        Some(Slot::Dismiss) if !order.iter().any(|s| matches!(s, Slot::Chat(k) if k != key)) => (rows > 0).then_some(Slot::Row(0)),
        other => other,
    }
}

/// "5m", "2h 10m": how long a chat has waited.
pub fn waited(at: u64, now: u64) -> String {
    crate::time::span(now.saturating_sub(at))
}

// ---- the cached source -----------------------------------------------------

/// Holds a value and reloads it at most once per `REFRESH`.
pub struct Cache<T> {
    at: Option<Instant>,
    value: T,
}

impl<T: Default> Cache<T> {
    pub const fn new(value: T) -> Self {
        Cache { at: None, value }
    }

    /// The held value; `load` runs only when it is older than `REFRESH` (or never loaded).
    pub fn get(&mut self, now: Instant, load: impl FnOnce() -> T) -> &T {
        if self.at.map_or(true, |t| now.saturating_duration_since(t) >= REFRESH) {
            self.value = load();
            self.at = Some(now);
        }
        &self.value
    }

    pub fn clear(&mut self) {
        self.at = None;
    }
}

thread_local! {
    static CACHE: RefCell<Cache<Vec<Item>>> = const { RefCell::new(Cache::new(Vec::new())) };
    static TITLES: RefCell<HashMap<String, (Instant, String)>> = RefCell::new(HashMap::new());
}

/// The waiting chats, from the cache (reads files at most every 5 s). Call from refresh and open, never from paint.
pub fn current() -> Vec<Item> {
    CACHE.with(|c| c.borrow_mut().get(Instant::now(), load).clone())
}

/// Make the next `current()` read afresh (after a dismissal or an open).
pub fn invalidate() {
    CACHE.with(|c| c.borrow_mut().clear());
}

/// Record each chat as cleared up to its own newest event time `at` (not "now"): a question that
/// arrived after the list was read is newer than the record and shows again.
pub fn merge_dismissed(m: &mut HashMap<String, u64>, entries: &[(String, u64)]) {
    for (k, at) in entries {
        let e = m.entry(k.clone()).or_insert(0);
        *e = (*e).max(*at);
    }
}

/// Clear these chats `(key, event time)` from the list (opened or dismissed), in the record a closed
/// needs-you card leaves. The shared file is re-read just before the write; only the UI thread
/// writes it (`card.rs` runs on it too), so no other writer can interleave.
pub fn dismiss(entries: &[(String, u64)]) {
    if !entries.is_empty() {
        let path = crate::data_file("hive-replayed.json");
        let mut m = read_replayed(&path);
        merge_dismissed(&mut m, entries);
        write_replayed(&path, &m);
    }
    invalidate();
}

/// The replayed file, read again a few times if it does not parse (another writer may be mid-write).
/// A missing file is an empty map; a file that stays unreadable is also empty, as `load_replayed` does.
fn read_replayed(path: &std::path::Path) -> HashMap<String, u64> {
    for _ in 0..3 {
        match std::fs::read_to_string(path) {
            Err(_) => return HashMap::new(),
            Ok(t) => {
                if let Ok(m) = serde_json::from_str(&t) {
                    return m;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    HashMap::new()
}

/// Write the map through a temp file and a rename, so a reader never sees a truncated file
/// (Delete repeats fast). Entries past the replay window are dropped, as `save_replayed` does.
fn write_replayed(path: &std::path::Path, m: &HashMap<String, u64>) {
    let now = crate::time::now();
    let m: HashMap<&String, &u64> = m.iter().filter(|(_, &at)| now.saturating_sub(at) <= events::REPLAY_SECS).collect();
    let Ok(text) = serde_json::to_string(&m) else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn load() -> Vec<Item> {
    if crate::demo::on() {
        return demo();
    }
    let Some(cfg) = crate::tray::config() else { return Vec::new() };
    let now = crate::time::now();
    let events = events::read_recent();
    let dismissed = events::load_replayed();
    let (snoozed, muted) = read_quiet(crate::time::now_ms());
    let opened = |ev: &Event| {
        cfg.find(&ev.account).and_then(|a| crate::claude::focused_at(&cfg, a, &ev.session)).is_some_and(|ms| ms / 1000 > ev.at)
    };
    let title = |ev: &Event| {
        let key = events::chat_key(ev);
        if let Some(t) = TITLES.with(|t| t.borrow().get(&key).filter(|(at, _)| at.elapsed() < TITLE_TTL).map(|(_, t)| t.clone())) {
            return t;
        }
        let found = events::chat_title(&cfg, &ev.account, &ev.session)
            .or_else(|| std::path::Path::new(&ev.cwd).file_name().map(|n| n.to_string_lossy().to_string()));
        if let Some(t) = &found {
            TITLES.with(|m| {
                let mut m = m.borrow_mut();
                if m.len() > 200 {
                    m.clear();
                }
                m.insert(key, (Instant::now(), t.clone()));
            });
        }
        found.unwrap_or_else(|| "a chat".into())
    };
    let quiet = |account: &str, chat: &str| {
        let k = (account.to_string(), chat.to_string());
        if muted.contains(&k) {
            Quiet::Muted
        } else if snoozed.contains(&k) {
            Quiet::Snoozed
        } else {
            Quiet::No
        }
    };
    build(&events, now, &dismissed, &opened, &title, &quiet, &|e| events::goal_shown(e, now))
}

type Pairs = Vec<(String, String)>;

/// (snoozed, muted) chats from the snooze file (card 116): `account` and `chat` of each live entry.
fn read_quiet(now_ms: u64) -> (Pairs, Pairs) {
    let text = std::fs::read_to_string(crate::data_file("hive-snooze.json")).unwrap_or_default();
    parse_quiet(&text, now_ms)
}

fn parse_quiet(text: &str, now_ms: u64) -> (Pairs, Pairs) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return (Vec::new(), Vec::new()) };
    let s = |x: &serde_json::Value, f: &str| x.get(f).and_then(|y| y.as_str()).map(str::to_string);
    let list = |name: &str, live: &dyn Fn(&serde_json::Value) -> bool| -> Pairs {
        v.get(name)
            .and_then(|a| a.as_array())
            .into_iter()
            .flatten()
            .filter(|x| live(x))
            .filter_map(|x| Some((s(x, "account")?, s(x, "chat")?)))
            .collect()
    };
    let snoozed = list("snoozed", &|x| x.get("until").and_then(|u| u.as_u64()).is_some_and(|u| u > now_ms));
    let muted = list("muted", &|_| true);
    (snoozed, muted)
}

/// Made-up chats for `--render` and `--demo`.
pub fn demo() -> Vec<Item> {
    let now = crate::time::now();
    let it = |account: &str, id: &str, title: &str, why: Why, mins: u64, quiet: Quiet| Item {
        account: account.into(),
        chat: format!("local_{id}"),
        key: format!("local_{id}"),
        title: title.into(),
        why,
        at: now - mins * 60,
        quiet,
    };
    vec![
        it("Work", "a1", "Release planning coordinator", Why::Ask, 3, Quiet::No),
        it("Work", "a2", "card 14: Fix the login bug", Why::Done, 5, Quiet::No),
        it("Work", "a3", "card 15: Rebuild the search index", Why::Needs, 9, Quiet::Snoozed),
        it("Personal", "b1", "Trip planning", Why::Needs, 22, Quiet::No),
        it("Side", "c1", "card 7: Refactor the importer", Why::Done, 41, Quiet::Muted),
        it("Side", "c2", "card 8: Add unit tests", Why::Done, 55, Quiet::No),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn replayed_write_is_atomic_prunes_old_entries_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("hive-wait-replayed-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hive-replayed.json");
        let now = crate::time::now();
        let mut m = super::read_replayed(&path);
        assert!(m.is_empty());
        m.insert("fresh".into(), now);
        m.insert("old".into(), now.saturating_sub(events::REPLAY_SECS + 100));
        super::write_replayed(&path, &m);
        assert!(!path.with_extension("json.tmp").exists(), "temp file must be renamed away");
        let back = super::read_replayed(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back.get("fresh"), Some(&now));
        // A second write replaces the file in place.
        let mut m2 = back;
        m2.insert("second".into(), now);
        super::write_replayed(&path, &m2);
        assert_eq!(super::read_replayed(&path).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    const NOW: u64 = 1_000_000;

    fn ev(kind: &str, account: &str, session: &str, msg: &str, ago: u64) -> Event {
        Event { kind: kind.into(), account: account.into(), session: session.into(), message: msg.into(), at: NOW - ago, ..Default::default() }
    }

    fn items(evs: &[Event], dismissed: &HashMap<String, u64>, opened: &dyn Fn(&Event) -> bool, quiet: &dyn Fn(&str, &str) -> Quiet) -> Vec<Item> {
        build(evs, NOW, dismissed, opened, &|e| format!("title of {}", e.session), quiet, &|_| true)
    }

    fn plain(evs: &[Event]) -> Vec<Item> {
        items(evs, &HashMap::new(), &|_| false, &|_, _| Quiet::No)
    }

    fn item(account: &str, title: &str, ago: u64) -> Item {
        Item { account: account.into(), chat: format!("local_{title}"), key: title.into(), title: title.into(), why: Why::Done, at: NOW - ago, quiet: Quiet::No }
    }

    #[test]
    fn newest_first_one_row_per_chat() {
        let evs = [
            ev("stop", "Work", "local_a", "", 600),
            ev("notify", "Work", "local_b", "Claude needs your permission", 300),
            ev("ask", "Personal", "local_c", "", 100),
            ev("stop", "Work", "local_a", "", 60), // a's newer event replaces its older one
        ];
        let got = plain(&evs);
        let order: Vec<&str> = got.iter().map(|i| i.chat.as_str()).collect();
        assert_eq!(order, ["local_a", "local_c", "local_b"]);
        assert_eq!(got[0].why, Why::Done);
        assert_eq!(got[1].why, Why::Ask);
        assert_eq!(got[2].why, Why::Needs);
    }

    #[test]
    fn nudges_answers_opened_and_dismissed_chats_drop_out() {
        let evs = [
            ev("ask", "Work", "local_a", "", 500),
            ev("notify", "Work", "local_a", "Claude is waiting for your input", 100), // a nudge: not news, a stays a question
            ev("notify", "Work", "local_b", "needs you", 400),
            ev("notify", "Work", "local_c", "needs you", 300),
            ev("stop", "Work", "local_d", "", 4 * 3600), // finished long ago
            ev("notify", "Work", "local_e", "needs you", 200),
        ];
        let mut dismissed = HashMap::new();
        dismissed.insert("local_c".to_string(), NOW - 100); // cleared after its event
        let got = items(&evs, &dismissed, &|e| e.session == "local_b", &|_, _| Quiet::No);
        let order: Vec<&str> = got.iter().map(|i| i.chat.as_str()).collect();
        assert_eq!(order, ["local_e", "local_a"]);
        assert_eq!(got[1].why, Why::Ask);
        // A newer event after the dismissal brings the chat back.
        let mut dismissed = HashMap::new();
        dismissed.insert("local_c".to_string(), NOW - 400);
        assert!(items(&evs, &dismissed, &|_| false, &|_, _| Quiet::No).iter().any(|i| i.chat == "local_c"));
    }

    #[test]
    fn snoozed_and_muted_chats_are_marked_not_hidden() {
        let evs = [ev("notify", "Work", "local_a", "x", 10), ev("notify", "Work", "local_b", "x", 20), ev("notify", "Work", "local_c", "x", 30)];
        let quiet = |a: &str, c: &str| match (a, c) {
            ("Work", "local_a") => Quiet::Snoozed,
            ("Work", "local_b") => Quiet::Muted,
            _ => Quiet::No,
        };
        let got = items(&evs, &HashMap::new(), &|_| false, &quiet);
        assert_eq!(got.iter().map(|i| i.quiet).collect::<Vec<_>>(), [Quiet::Snoozed, Quiet::Muted, Quiet::No]);
    }

    #[test]
    fn snooze_file_reads_live_snoozes_and_mutes() {
        let text = r#"{"snoozed":[{"account":"Work","chat":"local_a","until":5000},{"account":"Work","chat":"local_old","until":10}],
                       "muted":[{"account":"Side","chat":"local_m","kind":"notify","at":1}]}"#;
        let (s, m) = parse_quiet(text, 1000);
        assert_eq!(s, [("Work".to_string(), "local_a".to_string())]);
        assert_eq!(m, [("Side".to_string(), "local_m".to_string())]);
        assert_eq!(parse_quiet("not json", 1), (Vec::new(), Vec::new()));
    }

    #[test]
    fn a_retired_coordinator_does_not_adopt_cards() {
        let list = [item("Work", "card 1: x", 5), item("Work", "Old Hive coordinator 3", 10), item("Work", "Hive coordinator", 20)];
        let p = plan(&list);
        // The live coordinator (idx 2) takes the card chat; the old one stands alone.
        assert_eq!(p.entries[..2], [Entry::Chat { idx: 2, child: false }, Entry::Chat { idx: 0, child: true }]);
        assert!(p.entries.contains(&Entry::Chat { idx: 1, child: false }));
        // With only the retired one waiting, the card chat gets the account header instead.
        let p = plan(&[item("Work", "card 1: x", 5), item("Work", "Old Hive coordinator 3", 10)]);
        assert!(p.entries.contains(&Entry::Group("Work".into())));
    }

    #[test]
    fn a_dismissal_uses_the_event_time_so_a_newer_event_shows_again() {
        let evs = [ev("ask", "Work", "local_a", "", 100)];
        let shown = plain(&evs);
        let mut m = HashMap::new();
        merge_dismissed(&mut m, &[(shown[0].key.clone(), shown[0].at)]);
        assert_eq!(m["local_a"], NOW - 100);
        assert!(items(&evs, &m, &|_| false, &|_, _| Quiet::No).is_empty(), "the dismissed question is gone");
        // A question that arrives afterwards (even within the same 5 s cache window) is newer than the record.
        let newer = [evs[0].clone(), ev("ask", "Work", "local_a", "", 2)];
        assert_eq!(items(&newer, &m, &|_| false, &|_, _| Quiet::No).len(), 1);
        // An older dismissal never lowers a newer one.
        merge_dismissed(&mut m, &[("local_a".to_string(), 5)]);
        assert_eq!(m["local_a"], NOW - 100);
    }

    #[test]
    fn mutes_are_looked_up_by_session_like_the_card_gate() {
        let mut with_both = ev("notify", "Work", "local_s", "x", 10);
        with_both.chat = "cli-uuid".into();
        let mut cli_only = ev("notify", "Work", "", "x", 20);
        cli_only.chat = "cli-2".into();
        cli_only.cwd = "C:/p".into();
        let seen = RefCell::new(Vec::new());
        let quiet = |a: &str, c: &str| {
            seen.borrow_mut().push((a.to_string(), c.to_string()));
            if c == "local_s" { Quiet::Muted } else { Quiet::No }
        };
        let got = items(&[with_both, cli_only], &HashMap::new(), &|_| false, &quiet);
        assert_eq!(got[0].quiet, Quiet::Muted);
        let seen = seen.into_inner();
        assert!(seen.contains(&("Work".to_string(), "local_s".to_string())), "session first");
        assert!(seen.contains(&("Work".to_string(), "cli-2".to_string())), "chat id only when there is no session");
        assert!(!seen.iter().any(|(_, c)| c == "cli-uuid"));
    }

    #[test]
    fn card_titles_are_recognized() {
        assert_eq!(card_id("card 121: Flyout waiting list"), Some("121"));
        assert_eq!(card_id("Card 9b: x"), Some("9b"));
        assert_eq!(card_id("card game: thoughts"), None);
        assert_eq!(card_id("Hive coordinator"), None);
        assert_eq!(card_id("card 12 no colon"), None);
    }

    #[test]
    fn card_chats_sit_under_their_coordinator() {
        let list = [
            item("Work", "card 2: second", 10),
            item("Work", "Hive coordinator", 20),
            item("Personal", "Trip", 25),
            item("Work", "card 1: first", 30),
        ];
        let p = plan(&list);
        assert_eq!(
            p.entries,
            [
                Entry::Chat { idx: 1, child: false },
                Entry::Chat { idx: 0, child: true },
                Entry::Chat { idx: 3, child: true },
                Entry::Chat { idx: 2, child: false },
            ]
        );
        assert_eq!(p.more, 0);
    }

    #[test]
    fn a_coordinator_in_another_account_does_not_adopt_cards() {
        let list = [item("Side", "Hive coordinator", 10), item("Work", "card 1: x", 20), item("Work", "card 2: y", 30)];
        let p = plan(&list);
        assert_eq!(
            p.entries,
            [
                Entry::Chat { idx: 0, child: false },
                Entry::Group("Work".into()),
                Entry::Chat { idx: 1, child: true },
                Entry::Chat { idx: 2, child: true },
            ]
        );
    }

    fn chat(k: &str) -> Slot {
        Slot::Chat(k.into())
    }

    #[test]
    fn focus_order_is_dismiss_all_then_waiting_rows_then_accounts() {
        // Drawn order (coordinator, its card chat, another account) differs from list order.
        let list = [item("Work", "card 2: second", 10), item("Work", "Hive coordinator", 20), item("Personal", "Trip", 25)];
        let p = plan(&list);
        let s = slots(&list, &p, 2);
        assert_eq!(s, [Slot::Dismiss, chat("Hive coordinator"), chat("card 2: second"), chat("Trip"), Slot::Row(0), Slot::Row(1)]);
        // Nothing waiting: no "Dismiss all" slot, only the account rows.
        assert_eq!(slots(&[], &plan(&[]), 2), [Slot::Row(0), Slot::Row(1)]);
        // Rows past the cap are not drawn, so they are not focusable.
        let many: Vec<Item> = (0..15).map(|i| item("Work", &format!("chat {i}"), i as u64 * 10)).collect();
        let s = slots(&many, &plan(&many), 0);
        assert_eq!(s.len(), 1 + MAX_ROWS);
    }

    #[test]
    fn tab_and_arrows_walk_the_order_and_wrap() {
        let list = [item("Work", "a", 10), item("Work", "b", 20)];
        let order = slots(&list, &plan(&list), 1);
        assert_eq!(order, [Slot::Dismiss, chat("a"), chat("b"), Slot::Row(0)]);
        let mut cur = None;
        let mut seen = Vec::new();
        for _ in 0..5 {
            cur = step(&order, cur.as_ref(), false);
            seen.push(cur.clone().unwrap());
        }
        assert_eq!(seen, [Slot::Dismiss, chat("a"), chat("b"), Slot::Row(0), Slot::Dismiss]);
        // Backwards from nothing lands on the last slot; backwards from the first wraps to it too.
        assert_eq!(step(&order, None, true), Some(Slot::Row(0)));
        assert_eq!(step(&order, Some(&Slot::Dismiss), true), Some(Slot::Row(0)));
        assert_eq!(step(&order, Some(&chat("b")), true), Some(chat("a")));
        // A focus that left the list (its chat was opened elsewhere) restarts from the top.
        assert_eq!(step(&order, Some(&chat("gone")), false), Some(Slot::Dismiss));
        assert_eq!(step(&[], None, false), None);
    }

    #[test]
    fn after_a_dismissal_focus_moves_to_the_next_row_then_the_previous_then_the_accounts() {
        let order = [Slot::Dismiss, chat("a"), chat("b"), chat("c"), Slot::Row(0)];
        assert_eq!(focus_after_dismiss(&order, "b", 1), Some(chat("c")));
        assert_eq!(focus_after_dismiss(&order, "c", 1), Some(Slot::Row(0)));
        let only = [Slot::Dismiss, chat("a"), Slot::Row(0)];
        assert_eq!(focus_after_dismiss(&only, "a", 1), Some(Slot::Row(0)));
        assert_eq!(focus_after_dismiss(&[Slot::Dismiss, chat("a")], "a", 0), None);
        assert_eq!(focus_after_dismiss(&order, "zzz", 1), None);
    }

    #[test]
    fn rows_are_capped_at_twelve_with_a_count_of_the_rest() {
        let list: Vec<Item> = (0..15).map(|i| item("Work", &format!("chat {i}"), i as u64 * 10)).collect();
        let p = plan(&list);
        assert_eq!(p.entries.len(), MAX_ROWS);
        assert_eq!(p.more, 3);
        assert_eq!(p.entries[0], Entry::Chat { idx: 0, child: false });
        let few = plan(&list[..12]);
        assert_eq!((few.entries.len(), few.more), (12, 0));
        assert_eq!(plan(&[]), Plan::default());
    }

    #[test]
    fn a_header_cut_off_from_its_chats_is_dropped() {
        let mut list: Vec<Item> = (0..11).map(|i| item("Work", &format!("chat {i}"), i as u64)).collect();
        list.push(item("Side", "card 1: a", 100));
        list.push(item("Side", "card 2: b", 101));
        let p = plan(&list);
        // 11 chats, then a header would be row 12 with nothing under it.
        assert_eq!(p.entries.len(), 11);
        assert_eq!(p.more, 2);
    }

    #[test]
    fn cache_reads_at_most_every_five_seconds() {
        let mut c: Cache<Vec<u32>> = Cache::new(Vec::new());
        let t0 = Instant::now();
        let mut loads = 0;
        for secs in [0u64, 1, 2, 4] {
            c.get(t0 + Duration::from_secs(secs), || {
                loads += 1;
                vec![loads]
            });
        }
        assert_eq!(loads, 1, "four paints inside 5 s read once");
        let v = c.get(t0 + Duration::from_secs(5), || {
            loads += 1;
            vec![loads]
        });
        assert_eq!(v, &vec![2]);
        c.clear();
        c.get(t0 + Duration::from_secs(6), || {
            loads += 1;
            vec![loads]
        });
        assert_eq!(loads, 3, "clear forces the next read");
    }

    #[test]
    fn waited_reads_like_a_duration() {
        assert_eq!(waited(NOW - 5 * 60, NOW), "5m");
        assert_eq!(waited(NOW - 130 * 60, NOW), "2h 10m");
        assert_eq!(waited(NOW + 5, NOW), "1m");
    }
}
