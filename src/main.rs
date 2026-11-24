//! Skein (working name): a tray app that opens several Claude Desktop accounts
//! side by side, each in its own profile folder.
//!
//!   Hive.exe                           tray icon and flyout (or show the flyout if already running)
//!   Hive.exe           --startup       tray icon, wait for Google Drive, open startup accounts
//!   Hive.exe           --add-account   tray icon plus the Add account form
//!   Hive.exe           --open <name>   open one account and exit
//!   Hive.exe           --open-all      open every account and exit
//!   Hive.exe           --quit <name>   close that account's window completely
//!   Hive.exe           --save-layout   remember where each open Claude window is
//!   Hive.exe           --restore-layout  put the open windows back where they were saved
//!   Hive.exe           --event stop|notify   (run by Claude Code hooks) record a chat alert
//!   Hive.exe           --inbox post|read|hook   worker inbox file
//!   Hive.exe           --hook-entry    print the hooks to add to Claude Code's settings.json
//!   Hive.exe           --status        print each account's state

#![windows_subsystem = "windows"]

/// The app's name everywhere it is shown. One place to change it.
pub const APP_NAME: &str = "Hive";

mod card;
mod chatmove;
mod claude;
mod codex;
mod config;
mod demo;
mod doctor;
mod events;
mod inbox;
mod launcher;
mod layout;
mod memory;
mod nudge;
mod procs;
mod queue;
mod runner;
mod sound;
mod stats;
mod time;
mod trim;
mod tags;
mod taskbar;
mod tray;
mod ui;
mod watch;
mod win;

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A data file next to the exe. The app was "Claude Accounts" until 2026-10-15 and
/// "Switchboard" until 2026-10-16: a file still under an old name is moved to the new one
/// the first time it's needed.
pub fn data_file(name: &str) -> PathBuf {
    let p = exe_dir().join(name);
    if !p.exists() {
        if let Some(rest) = name.strip_prefix("hive") {
            for prefix in OLD_PREFIXES {
                let old = exe_dir().join(format!("{prefix}{rest}"));
                if old.exists() {
                    // If the move fails (the file is locked), keep using the old file:
                    // never start over with a new empty one.
                    if std::fs::rename(&old, &p).is_err() {
                        return old;
                    }
                    break;
                }
            }
        }
    }
    p
}

/// What data file names started with before, newest first.
const OLD_PREFIXES: [&str; 2] = ["switchboard", "claude-accounts"];

/// File names the app had before it was called Hive. Identical copies are kept under
/// them, since other programs may call them: Claude Code hooks, Codex's hooks (trusted by
/// their exact command) and notify setting, pinned taskbar buttons.
pub const OLD_EXES: [&str; 2] = ["Switchboard.exe", "ClaudeAccounts.exe"];

pub fn config_path() -> PathBuf {
    data_file("hive.json")
}

pub fn log_path() -> PathBuf {
    data_file("hive.log")
}

pub fn log(msg: &str) {
    use std::io::Write;
    let path = log_path();
    if std::fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let new = !path.exists();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        if new {
            // A UTF-8 mark, so Notepad and PowerShell 5 show non-ASCII titles right.
            let _ = f.write_all(b"\xEF\xBB\xBF");
        }
        let _ = writeln!(f, "{} {}", win::timestamp(), msg);
    }
}

fn console() {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

fn load() -> Option<config::Config> {
    match config::Config::load_or_migrate(&config_path()) {
        Ok(c) => Some(c),
        Err(e) => {
            println!("settings error: {e}");
            None
        }
    }
}

fn cli_status() {
    console();
    let Some(cfg) = load() else { return };
    let w = cfg.accounts.iter().map(|a| a.name.chars().count()).max().unwrap_or(0).max(7);
    println!("{:<w$} {:<26} {:<8} {:<5} {:<6} {}", "ACCOUNT", "STATE", "STARTUP", "KEEP", "CHROME", "FOLDER");
    for a in &cfg.accounts {
        let (_, state) = claude::status(&cfg, a);
        println!(
            "{:<w$} {:<26} {:<8} {:<5} {:<6} {}",
            a.name,
            state,
            if a.startup { "yes" } else { "no" },
            if a.keep_running { "yes" } else { "no" },
            if claude::chrome_is(&cfg, a) { "yes" } else { "" },
            claude::data_dir(&cfg, a).display()
        );
    }
    if !cfg.codex.accounts.is_empty() {
        let mains = procs::main_pids(&cfg, &procs::list());
        for a in &cfg.codex.accounts {
            let state = if mains.contains_key(&a.name) { "open (Codex)" } else { "closed (Codex)" };
            println!(
                "{:<w$} {:<26} {:<8} {:<5} {:<6} {}",
                a.name,
                state,
                if a.startup { "yes" } else { "no" },
                "",
                "",
                codex::account_dir(&cfg, &a.name).display()
            );
        }
    }
    let drive = &cfg.wait_for_drive;
    println!();
    println!("Claude.exe:        {}", claude::claude_exe(&cfg).map(|p| p.display().to_string()).unwrap_or("NOT FOUND".into()));
    println!(
        "Drive wait:        {} ({}: {})",
        if drive.enabled { "on" } else { "off" },
        drive.path,
        if claude::drive_ready(&drive.path) { "ready" } else { "not ready" }
    );
    println!("Start with Windows: {}", if tray::startup_enabled() { "on" } else { "off" });
    println!("Settings:          {}", config_path().display());
    let next = tray::use_next(&cfg).unwrap_or_else(|| "none (every account is past the line)".into());
    println!("Use next:          {next}  [soonest reset under {}%]", cfg.use_next_below);
    let found = doctor::all(&cfg);
    println!();
    if found.is_empty() {
        println!("Setup check:       no problems found");
    }
    for p in found {
        println!("Setup check:       {p}");
    }
}

fn cli_open(names: &[String], all: bool) {
    console();
    let Some(cfg) = load() else { return };
    let list: Vec<_> = if all {
        cfg.accounts.clone()
    } else {
        let wanted = names.join(" ");
        if let Some(c) = cfg.find_codex(&wanted) {
            if let Err(e) = codex::open_account(&cfg, &c.name) {
                println!("Couldn't open {}: {e}", c.name);
            }
            return;
        }
        if wanted.eq_ignore_ascii_case("Codex") {
            codex::open(&cfg);
            return;
        }
        match cfg.find(&wanted) {
            Some(a) => vec![a.clone()],
            None => {
                println!("No account called \"{wanted}\". Accounts: {}", cfg.accounts.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "));
                return;
            }
        }
    };
    // The running tray restores a window's saved spot only when it expects that window.
    for a in list.iter().filter(|a| !claude::is_running(&cfg, a)) {
        tray::tell_expect(&a.name);
    }
    launcher::run_blocking(&cfg, list);
    for (title, text) in launcher::take_notes() {
        println!("{title}: {text}");
    }
}

/// Close one account's window completely (no confirmation: the command is the confirmation).
fn cli_quit(names: &[String]) {
    console();
    let Some(cfg) = load() else { return };
    let wanted = names.join(" ");
    match cfg.find(&wanted) {
        Some(a) => match procs::quit(&cfg, a) {
            Ok(()) => println!("Quit {}", a.name),
            Err(e) => println!("{e}"),
        },
        None => println!("No account called \"{wanted}\"."),
    }
}

/// Save or restore the window layout from the command line.
fn cli_layout(save: bool, only: &[String]) {
    console();
    let Some(mut cfg) = load() else { return };
    let only = only.join(" ");
    let only = (!only.is_empty()).then_some(only);
    if save {
        let (mut lay, mut saved, left_out) = layout::save(&cfg);
        if let Some(n) = &only {
            lay.slots.retain(|k, _| k.eq_ignore_ascii_case(n));
            saved.retain(|s| s.to_ascii_lowercase().starts_with(&n.to_ascii_lowercase()));
        }
        if saved.is_empty() {
            println!("No open Claude window to save.");
            return;
        }
        cfg.layout.slots.extend(lay.slots);
        cfg.layout.saved_at = lay.saved_at;
        match cfg.save(&config_path()) {
            Ok(()) => println!("Saved: {}.{}", saved.join(", "), if left_out.is_empty() { String::new() } else { format!(" Left out (hidden or minimized): {}.", left_out.join(", ")) }),
            Err(e) => println!("Couldn't save: {e}"),
        }
    } else {
        if let Some(n) = &only {
            cfg.layout.slots.retain(|k, _| k.eq_ignore_ascii_case(n));
        }
        let (moved, closed, hidden) = layout::restore(&cfg);
        println!("Moved {moved}. Not open: {}. Hidden: {}.", closed.join(", "), hidden.join(", "));
    }
}

/// Claude windows opened from here must not inherit our output pipe: a script
/// reading our output would wait for it to close until Claude exits.
fn keep_std_handles_private() {
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let h = GetStdHandle(id);
            if !h.is_null() && h as isize != -1 {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Time each piece of work the flyout does when it opens (for tuning; not in --help).
fn bench() {
    console();
    let Some(cfg) = load() else { return };
    let time = |label: &str, f: &mut dyn FnMut() -> usize| {
        let mut runs = Vec::new();
        let mut n = 0;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            n = f();
            runs.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!("{label:<28} {:>8.1} {:>8.1} {:>8.1} ms   ({n})", runs[0], runs[1], runs[2]);
    };
    println!("{:<28} {:>8} {:>8} {:>8}", "step", "run 1", "run 2", "run 3");
    time("process list", &mut || procs::list().len());
    time("main window per account", &mut || procs::main_pids(&cfg, &procs::list()).len());
    time("memory per account", &mut || procs::memory(&cfg).len());
    time("is_running x accounts", &mut || cfg.accounts.iter().filter(|a| claude::is_running(&cfg, a)).count());
    time("org from logs x accounts", &mut || cfg.accounts.iter().filter(|a| matches!(claude::org(&cfg, a), claude::Org::Id(_))).count());
    time("usage x accounts", &mut || cfg.accounts.iter().filter_map(|a| claude::usage(&cfg, a, None)).count());
    time("codex usage", &mut || codex::read(&cfg).map_or(0, |c| c.windows.len()));
    time("chat index (all accounts)", &mut || cfg.accounts.iter().map(|a| claude::chats(&cfg, a).len()).sum());
    let mut cache = claude::ChatCache::default();
    time("chat index, cached", &mut || cfg.accounts.iter().map(|a| claude::chats_cached(&cfg, a, &mut cache).len()).sum());
    let file = std::env::temp_dir().join("hive-bench-chats.json");
    cache.changed = true;
    cache.save(&file);
    time("chat index, after restart", &mut || {
        let mut c = claude::ChatCache::load(&file);
        cfg.accounts.iter().map(|a| claude::chats_cached(&cfg, a, &mut c).len()).sum()
    });
    let _ = std::fs::remove_file(&file);
    time("one chat title (alerts)", &mut || {
        let a = &cfg.accounts[0];
        let id = claude::chats(&cfg, a).first().map(|c| c.id.clone()).unwrap_or_default();
        let t = std::time::Instant::now();
        let n = claude::chat_title(&cfg, a, &id).map_or(0, |s| s.len());
        println!("    (title lookup alone: {:.1} ms)", t.elapsed().as_secs_f64() * 1000.0);
        n
    });
    time("setup check", &mut || doctor::problems(&cfg).len());
}

fn cli_move_chats(args: &[String]) {
    console();
    let Some(cfg) = load() else { return };
    let flag = |f: &str| args.iter().any(|a| a.eq_ignore_ascii_case(f));
    let only = args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1)).map(|s| s.as_str());
    let names: Vec<&String> = args.iter().filter(|a| !a.starts_with("--") && Some(a.as_str()) != only).collect();
    let (Some(from), Some(to)) = (names.first().and_then(|n| cfg.find(n)), names.get(1).and_then(|n| cfg.find(n))) else {
        println!("Usage: --move-chats <from> <to> [--only local_id] [--include-work] [--go]");
        return;
    };
    match chatmove::plan(&cfg, from, to, flag("--include-work"), only) {
        Err(e) => println!("{e}"),
        Ok(p) => {
            println!("{} -> {}\n  {}", p.from.display(), p.to.display(), chatmove::describe(&p));
            if flag("--go") {
                println!("{}", chatmove::apply(&p).unwrap_or_else(|e| e));
            } else {
                println!("Dry run. Add --go to copy.");
            }
        }
    }
}

fn main() {
    keep_std_handles_private();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(|s| s.to_ascii_lowercase());
    match first.as_deref() {
        Some("--status") => cli_status(),
        Some("--doctor") => {
            console();
            if let Some(cfg) = load() {
                if !doctor::cli(&args[1..], &cfg) {
                    std::process::exit(1);
                }
            }
        }
        Some("--dashboard") => {
            let memory = args.get(1).is_some_and(|a| a.eq_ignore_ascii_case("memory"));
            if !tray::tell_dashboard(memory as usize) {
                console();
                println!("{} isn't running. Start it first, then run --dashboard again.", APP_NAME);
            }
        }
        Some("--bench") => bench(),
        // Draw the Settings tabs off screen at 100% and 150% (for layout checks).
        Some("--render") => {
            console();
            if let (Some(cfg), Some(dir)) = (load(), args.get(1)) {
                let opts = tray::opts_of(&cfg, &Default::default());
                for f in ui::render_preview(opts, std::path::Path::new(dir)) {
                    println!("{f}");
                }
            }
        }
        // For checking the question finder on a real chat transcript.
        Some("--find-question") => {
            console();
            let q = args.get(1).and_then(|p| events::question_in_transcript(std::path::Path::new(p)));
            println!("{}", q.unwrap_or_else(|| "(no question)".into()));
        }
        // --move-chats <from> <to> [--only local_id] [--include-work] [--go]: dry run unless --go.
        // --undo-move: remove what the last move added.
        Some("--move-chats") => cli_move_chats(&args[1..]),
        Some("--undo-move") => {
            console();
            if let Some(cfg) = load() {
                println!("{}", chatmove::undo(&cfg).unwrap_or_else(|e| e));
            }
        }
        Some("--open") => cli_open(&args[1..], false),
        Some("--open-all") => cli_open(&[], true),
        Some("--quit") => cli_quit(&args[1..]),
        Some("--event") => events::cli(&args[1..]),
        // The shared memory vault: the start-of-chat hook, and its upkeep.
        Some("--memory") => memory::cli(&args[1..]),
        // The worker inbox: post, read and hook (see inbox.rs).
        Some("--inbox") => inbox::cli(&args[1..]),
        // The shared job queue (see queue.rs).
        Some("--queue") => queue::cli(&args[1..]),
        // PostToolUse hook that shortens big Bash, PowerShell and MCP output (see trim.rs).
        Some("--trim-hook") => trim::cli(),
        Some("--hook-entry") => {
            console();
            println!("{}", events::hook_entry());
        }
        Some("--save-layout") => cli_layout(true, &args[1..]),
        Some("--restore-layout") => cli_layout(false, &args[1..]),
        Some("--startup") => tray::run(tray::Start::Startup),
        Some("--add-account") => tray::run(tray::Start::AddDialog),
        // Sample accounts and chats instead of real ones, for screenshots.
        Some("--card-demo") => card::demo(&args[1..]),
        Some("--demo") => {
            demo::enable();
            tray::run(tray::Start::Normal)
        }
        Some("--help") | Some("-h") | Some("/?") => {
            console();
            println!("Hive [--startup | --add-account | --open <name> | --open-all | --status]");
        }
        _ => tray::run(tray::Start::Normal),
    }
}
