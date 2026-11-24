//! Optional "Move chats": copy one account's Claude Code sidebar entries into
//! another account's list, so the same chats show up there and resume from the
//! shared transcripts in ~/.claude/projects.
//!
//! Safe by design: entries are only ever added (a chat the target already has is
//! left alone), the target's list is backed up first, a manifest lists exactly
//! what was added, and "Undo last move" removes just those files. Work (Team)
//! chats stay put unless asked for.

use std::fs;
use std::path::{Path, PathBuf};

use crate::claude;
use crate::config::{Account, Config};

/// Everything a move would do, worked out without changing anything.
#[derive(Debug, Default)]
pub struct Plan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub add: Vec<PathBuf>,
    pub already_there: usize,
    pub archived: usize,
}

pub fn is_work(a: &Account) -> bool {
    a.name.eq_ignore_ascii_case("team") || a.extra.get("work").and_then(|v| v.as_bool()).unwrap_or(false)
}

fn sessions_dir(profile: &Path) -> PathBuf {
    profile.join("claude-code-sessions")
}

fn entries(folder: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(folder) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("local_"))
        })
        .collect()
}

/// The list folder an account is using now: <sessions>\<account>\<org> with the
/// most recently written chat file (a profile keeps one per org it has signed in to).
pub fn active_folder(profiles: &[PathBuf]) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for p in profiles {
        let Ok(accts) = fs::read_dir(sessions_dir(p)) else { continue };
        for acct in accts.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            let Ok(orgs) = fs::read_dir(acct.path()) else { continue };
            for org in orgs.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
                let newest = entries(&org.path())
                    .iter()
                    .filter_map(|f| fs::metadata(f).and_then(|m| m.modified()).ok())
                    .max();
                if let Some(t) = newest {
                    if best.as_ref().map_or(true, |(b, _)| t > *b) {
                        best = Some((t, org.path()));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn archived(file: &Path) -> bool {
    fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("isArchived").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// What would be copied from one list folder to another. `only` limits it to one chat id.
pub fn plan_folders(from: &Path, to: &Path, only: Option<&str>) -> Plan {
    let mut plan = Plan { from: from.to_path_buf(), to: to.to_path_buf(), ..Default::default() };
    for f in entries(from) {
        let name = f.file_name().unwrap_or_default().to_owned();
        if let Some(id) = only {
            if name.to_string_lossy().trim_end_matches(".json") != id {
                continue;
            }
        }
        if to.join(&name).exists() {
            plan.already_there += 1;
        } else if archived(&f) {
            plan.archived += 1;
        } else {
            plan.add.push(f);
        }
    }
    plan.add.sort();
    plan
}

/// Plan a move between two accounts. Refuses when the target is open (it holds
/// its list in memory and would overwrite the copies) or when work chats would
/// go to a personal account without `include_work`.
pub fn plan(cfg: &Config, from: &Account, to: &Account, include_work: bool, only: Option<&str>) -> Result<Plan, String> {
    if from.name == to.name {
        return Err("Pick two different accounts.".into());
    }
    if is_work(from) && !is_work(to) && !include_work {
        return Err(format!("{} chats hold client data, so they aren't moved to {} unless you allow it.", from.name, to.name));
    }
    if claude::is_running(cfg, to) {
        return Err(format!("{} is open. Close it first (it keeps its chat list in memory), then try again.", to.name));
    }
    let src = active_folder(&claude::profile_dirs(cfg, from)).ok_or_else(|| format!("{} has no chat list yet.", from.name))?;
    let dst = active_folder(&claude::profile_dirs(cfg, to)).ok_or_else(|| {
        format!("{} has no chat list yet. Open it once and start any chat, then try again.", to.name)
    })?;
    Ok(plan_folders(&src, &dst, only))
}

pub fn root() -> PathBuf {
    crate::exe_dir().join("chat-moves")
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

/// Do a planned move under `root`: back up the target's list, add the entries,
/// write the manifest. Returns a one-line result.
pub fn apply_in(plan: &Plan, root: &Path, stamp: u64) -> Result<String, String> {
    if plan.add.is_empty() {
        return Ok(format!("Nothing to move: {} already there, {} archived skipped.", plan.already_there, plan.archived));
    }
    let dir = root.join(stamp.to_string());
    copy_dir(&plan.to, &dir.join("backup")).map_err(|e| format!("Couldn't back up the target list: {e}"))?;
    let mut added: Vec<String> = Vec::new();
    for f in &plan.add {
        let dest = plan.to.join(f.file_name().unwrap_or_default());
        if dest.exists() {
            continue;
        }
        fs::copy(f, &dest).map_err(|e| format!("Couldn't copy {}: {e}", f.display()))?;
        added.push(dest.display().to_string());
    }
    let manifest = serde_json::json!({ "target": plan.to.display().to_string(), "added": added });
    fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest).unwrap_or_default())
        .map_err(|e| format!("Copied, but couldn't write the undo list: {e}"))?;
    Ok(format!("Moved {} chat(s). Backup and undo list are in {}.", added.len(), dir.display()))
}

pub fn apply(plan: &Plan) -> Result<String, String> {
    apply_in(plan, &root(), crate::time::now())
}

/// Remove exactly the files the newest not-yet-undone move added.
pub fn undo_in(root: &Path) -> Result<String, String> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .map_err(|_| "No moves to undo.".to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("manifest.json").exists())
        .collect();
    dirs.sort();
    let dir = dirs.pop().ok_or_else(|| "No moves to undo.".to_string())?;
    let text = fs::read_to_string(dir.join("manifest.json")).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut removed = 0;
    for p in v.get("added").and_then(|a| a.as_array()).into_iter().flatten().filter_map(|s| s.as_str()) {
        if fs::remove_file(p).is_ok() {
            removed += 1;
        }
    }
    fs::rename(dir.join("manifest.json"), dir.join("manifest.undone.json")).map_err(|e| e.to_string())?;
    Ok(format!("Undid the last move: removed {removed} copied chat(s). Your other chats weren't touched."))
}

pub fn undo(cfg: &Config) -> Result<String, String> {
    let _ = cfg;
    undo_in(&root())
}

/// One line for the CLI and the menu: what a move would do.
pub fn describe(p: &Plan) -> String {
    format!("{} to copy, {} already there, {} archived skipped", p.add.len(), p.already_there, p.archived)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hive-chatmove-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn move_adds_only_missing_then_undoes() {
        let base = tmp("a");
        let (from, to, root) = (base.join("from"), base.join("to"), base.join("root"));
        fs::create_dir_all(&from).unwrap();
        fs::create_dir_all(&to).unwrap();
        fs::write(from.join("local_1.json"), r#"{"isArchived":false}"#).unwrap();
        fs::write(from.join("local_2.json"), r#"{"isArchived":false}"#).unwrap();
        fs::write(from.join("local_3.json"), r#"{"isArchived":true}"#).unwrap();
        fs::write(to.join("local_2.json"), "keep me").unwrap();
        fs::write(to.join("local_9.json"), "mine").unwrap();
        let plan = plan_folders(&from, &to, None);
        assert_eq!((plan.add.len(), plan.already_there, plan.archived), (1, 1, 1));
        apply_in(&plan, &root, 100).unwrap();
        assert!(to.join("local_1.json").exists());
        assert_eq!(fs::read_to_string(to.join("local_2.json")).unwrap(), "keep me");
        assert!(root.join("100").join("backup").join("local_9.json").exists());
        undo_in(&root).unwrap();
        assert!(!to.join("local_1.json").exists());
        assert_eq!(fs::read_to_string(to.join("local_9.json")).unwrap(), "mine");
        assert!(undo_in(&root).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn only_limits_to_one_chat() {
        let base = tmp("b");
        let (from, to) = (base.join("from"), base.join("to"));
        fs::create_dir_all(&from).unwrap();
        fs::create_dir_all(&to).unwrap();
        fs::write(from.join("local_a.json"), "{}").unwrap();
        fs::write(from.join("local_b.json"), "{}").unwrap();
        let plan = plan_folders(&from, &to, Some("local_b"));
        assert_eq!(plan.add.len(), 1);
        let _ = fs::remove_dir_all(&base);
    }
}
