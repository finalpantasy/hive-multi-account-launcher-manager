//! Which processes belong to which Claude window, how much memory each window
//! uses, and quitting one. Read only when the flyout opens or on Quit.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetWindowThreadProcessId, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
    WM_APP, WM_ENDSESSION, WM_QUERYENDSESSION,
};

use crate::claude;
use crate::config::{Account, Config};
use crate::win::{from_wide, wide};

/// Sent to the tray before an account is quit on purpose (wParam: its index in
/// the settings), so Keep running doesn't reopen it.
pub const WM_QUITTING: u32 = WM_APP + 7;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(h: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

pub fn list() -> Vec<Proc> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            out.push(Proc { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, name: from_wide(&e.szExeFile) });
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

fn open(pid: u32, access: u32) -> Option<HANDLE> {
    let h = unsafe { OpenProcess(access, 0, pid) };
    (!h.is_null()).then_some(h)
}

pub fn image_path(pid: u32) -> Option<String> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { CloseHandle(h) };
    ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// The process's command line (ProcessCommandLineInformation, Windows 8.1+).
fn command_line(pid: u32) -> Option<String> {
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut need = 0u32;
    unsafe { NtQueryInformationProcess(h, 60, null_mut(), 0, &mut need) };
    let mut out = None;
    if need > 16 {
        let mut buf = vec![0u64; (need as usize + 7) / 8];
        if unsafe { NtQueryInformationProcess(h, 60, buf.as_mut_ptr() as *mut c_void, need, &mut need) } >= 0 {
            // UNICODE_STRING { Length: u16, MaximumLength: u16, Buffer: *const u16 }
            let bytes = (buf[0] & 0xffff) as usize;
            let ptr = buf[1] as *const u16;
            if !ptr.is_null() {
                out = Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, bytes / 2) }));
            }
        }
    }
    unsafe { CloseHandle(h) };
    out
}

fn private_bytes(pid: u32) -> u64 {
    let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return 0 };
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(h, &mut c as *mut _ as *mut _, c.cb) } != 0;
    unsafe { CloseHandle(h) };
    if ok { c.PrivateUsage as u64 } else { 0 }
}

/// The value of `--user-data-dir=` in a command line, quoted or not.
fn user_data_dir(cmd: &str) -> Option<String> {
    const SWITCH: &str = "--user-data-dir=";
    let i = cmd.to_ascii_lowercase().find(SWITCH)?;
    let quoted_token = cmd[..i].ends_with('"'); // "--user-data-dir=C:\a b"
    let rest = &cmd[i + SWITCH.len()..];
    let value = if let Some(r) = rest.strip_prefix('"') {
        r.split('"').next() // --user-data-dir="C:\a b"
    } else if quoted_token {
        rest.split('"').next()
    } else {
        rest.split(char::is_whitespace).next()
    };
    Some(value.unwrap_or("").to_string())
}

/// Folder paths compared the way Windows does: any case, either slash, no trailing slash.
fn same_dir(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// The main Claude process of each open account (by account name).
pub fn main_pids(cfg: &Config, procs: &[Proc]) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for p in procs.iter().filter(|p| p.name.eq_ignore_ascii_case("claude.exe")) {
        let Some(path) = image_path(p.pid) else { continue };
        let custom = !cfg.claude_exe.trim().is_empty() && path.eq_ignore_ascii_case(cfg.claude_exe.trim());
        if !custom && !path.to_ascii_lowercase().contains("\\windowsapps\\claude_") {
            continue; // Claude Code CLI and other claude.exe builds
        }
        let Some(cmd) = command_line(p.pid) else { continue };
        if cmd.contains("--type=") {
            continue; // renderer, GPU and helper processes
        }
        let dir = user_data_dir(&cmd);
        let owner = cfg.accounts.iter().find(|a| match &dir {
            None => a.default_profile,
            Some(d) => !a.default_profile && same_dir(d, &claude::data_dir(cfg, a).to_string_lossy()),
        });
        if let Some(a) = owner {
            out.entry(a.name.clone()).or_insert(p.pid);
        }
    }
    // With extra Codex sign-ins, each Codex window counts as an account too (the
    // main one as "Codex"), so they get tags, taskbar buttons and layouts.
    if !cfg.codex.accounts.is_empty() {
        for (n, pid) in codex_pids(cfg, procs) {
            out.entry(n).or_insert(pid);
        }
    }
    out
}

/// The main process of each open Codex app window: "Codex" for the normal one,
/// an extra sign-in's name for its own instance.
pub fn codex_pids(cfg: &Config, procs: &[Proc]) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for p in procs.iter().filter(|p| p.name.eq_ignore_ascii_case("chatgpt.exe")) {
        let Some(path) = image_path(p.pid) else { continue };
        if !path.to_ascii_lowercase().contains("\\windowsapps\\openai.codex_") {
            continue;
        }
        let Some(cmd) = command_line(p.pid) else { continue };
        if cmd.contains("--type=") {
            continue;
        }
        let name = match user_data_dir(&cmd) {
            None => Some("Codex".to_string()),
            Some(d) => cfg
                .codex
                .accounts
                .iter()
                .find(|a| same_dir(&d, &crate::codex::account_app_dir(cfg, &a.name).to_string_lossy()))
                .map(|a| a.name.clone()),
        };
        if let Some(n) = name {
            out.entry(n).or_insert(p.pid);
        }
    }
    out
}

/// When a process started (0 if it can't be read).
fn started(h: HANDLE) -> u64 {
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    if unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } == 0 {
        return 0;
    }
    (c.dwHighDateTime as u64) << 32 | c.dwLowDateTime as u64
}

fn started_pid(pid: u32) -> u64 {
    let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return 0 };
    let t = started(h);
    unsafe { CloseHandle(h) };
    t
}

/// A process and everything it started, with each one's start time. Windows
/// never updates an orphan's parent id and reuses ids, so a child only counts
/// if it started after its parent. Nothing in `skip` (this app, the other
/// accounts' windows) is included, nor anything under it.
fn tree(procs: &[Proc], root: u32, skip: &[u32]) -> Vec<(u32, u64)> {
    let mut out = vec![(root, started_pid(root))];
    let mut i = 0;
    while i < out.len() {
        let (parent, born) = out[i];
        if born != 0 {
            for p in procs.iter().filter(|p| p.ppid == parent && p.pid != parent && !skip.contains(&p.pid)) {
                if out.iter().any(|&(q, _)| q == p.pid) {
                    continue;
                }
                let t = started_pid(p.pid);
                if t != 0 && t >= born {
                    out.push((p.pid, t));
                }
            }
        }
        i += 1;
    }
    out
}

/// What each open account is doing right now, for the dashboard's lights: how many Code
/// chats it has loaded (a claude.exe per chat) and the CPU time its processes have used
/// (100 ns units; the caller compares two readings). Codex sign-ins too.
pub fn activity(cfg: &Config) -> HashMap<String, (usize, u64)> {
    let procs = list();
    let mains = main_pids(cfg, &procs);
    let mut out = HashMap::new();
    let cpu = |pid: u32| -> u64 {
        let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return 0 };
        let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (z(), z(), z(), z());
        let ok = unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0;
        unsafe { CloseHandle(h) };
        let v = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        if ok { v(k) + v(u) } else { 0 }
    };
    for (name, &pid) in &mains {
        let all = tree(&procs, pid, &skip_list(&mains, pid));
        let chats = all
            .iter()
            .filter(|(p, _)| procs.iter().any(|x| x.pid == *p && x.name.eq_ignore_ascii_case("claude.exe")) && command_line(*p).is_some_and(|c| c.contains("stream-json")))
            .count();
        out.insert(name.clone(), (chats, all.iter().map(|(p, _)| cpu(*p)).sum()));
    }
    for (name, pid) in codex_pids(cfg, &procs) {
        let all = tree(&procs, pid, &[std::process::id()]);
        out.entry(name).or_insert((0, all.iter().map(|(p, _)| cpu(*p)).sum()));
    }
    out
}

/// What a tree walk from `keep` must stay out of: this app and the other accounts' windows.
fn skip_list(mains: &HashMap<String, u32>, keep: u32) -> Vec<u32> {
    let mut v: Vec<u32> = mains.values().copied().filter(|&p| p != keep).collect();
    v.push(std::process::id());
    v
}

/// Memory each open account uses: its window plus everything it started
/// (helpers, Claude Code sessions).
pub fn memory(cfg: &Config) -> HashMap<String, u64> {
    memory_in(cfg, &list())
}

pub fn memory_in(cfg: &Config, procs: &[Proc]) -> HashMap<String, u64> {
    let mains = main_pids(cfg, procs);
    mains
        .iter()
        .map(|(name, &pid)| {
            let all = tree(procs, pid, &skip_list(&mains, pid));
            (name.clone(), all.into_iter().map(|(p, _)| private_bytes(p)).sum())
        })
        .collect()
}

pub fn gb(bytes: u64) -> String {
    let g = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if g >= 1.0 { format!("{g:.1} GB") } else { format!("{:.0} MB", g * 1024.0) }
}

struct Find {
    pid: u32,
    windows: Vec<HWND>,
}

unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> i32 {
    let f = &mut *(lp as *mut Find);
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, &mut pid);
    if pid == f.pid {
        f.windows.push(h);
    }
    1
}

/// Close an account's window for real (not just to the tray). First the way
/// Windows asks apps to close at sign-out, then, if it is still running after
/// a few seconds, by ending its processes. Blocking: call off the UI thread.
pub fn quit(cfg: &Config, a: &Account) -> Result<(), String> {
    let procs = list();
    let mains = main_pids(cfg, &procs);
    let pid = *mains.get(&a.name).ok_or_else(|| format!("{} isn't open.", a.name))?;
    // Hold handles from the start: a handle pins the process, so a process id
    // reused after exit can never point us at an unrelated program.
    let access = PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
    let main = open(pid, access).ok_or("Couldn't reach its window.")?;
    let mut handles: Vec<HANDLE> = vec![main];
    for (p, born) in tree(&procs, pid, &skip_list(&mains, pid)).into_iter().skip(1) {
        let Some(h) = open(p, access) else { continue };
        if started(h) == born {
            handles.push(h);
        } else {
            unsafe { CloseHandle(h) }; // that id belongs to a different process now
        }
    }
    tell_tray(WM_QUITTING, cfg.accounts.iter().position(|x| x.name == a.name).unwrap_or(usize::MAX));
    unsafe {
        let mut f = Find { pid, windows: Vec::new() };
        EnumWindows(Some(collect), &mut f as *mut Find as LPARAM);
        let mut r = 0usize;
        for &w in &f.windows {
            SendMessageTimeoutW(w, WM_QUERYENDSESSION, 0, 1, SMTO_ABORTIFHUNG, 1500, &mut r);
        }
        for &w in &f.windows {
            SendMessageTimeoutW(w, WM_ENDSESSION, 1, 1, SMTO_ABORTIFHUNG, 1500, &mut r);
        }
        if WaitForSingleObject(main, 6000) != 0 {
            TerminateProcess(main, 0);
            WaitForSingleObject(main, 2000);
        }
        // Helpers and Claude Code sessions it started can outlive it: end those too.
        for &h in handles.iter().skip(1) {
            if WaitForSingleObject(h, 0) != 0 {
                TerminateProcess(h, 0);
            }
        }
        for h in handles {
            CloseHandle(h);
        }
    }
    // A forced end leaves the lockfile behind with no file event: tell the tray to look.
    tell_tray(crate::watch::WM_WATCH, crate::watch::CHANGED_LOCK);
    Ok(())
}

/// Message the running tray, if any (works from the command line too).
fn tell_tray(msg: u32, wp: usize) {
    let class = wide(crate::tray::CLASS);
    unsafe {
        let h = FindWindowW(class.as_ptr(), std::ptr::null());
        if h.is_null() {
            return;
        }
        if msg == WM_QUITTING {
            // Synchronous, so the tray knows before the window closes.
            let mut r = 0usize;
            SendMessageTimeoutW(h, msg, wp, 0, SMTO_ABORTIFHUNG, 2000, &mut r);
        } else {
            PostMessageW(h, msg, wp, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_dir_is_read_exactly() {
        let a = r#""C:\x\Claude.exe" "--user-data-dir=C:\P\Team 2" --flag"#;
        let b = r#"C:\x\Claude.exe --user-data-dir="C:\P\Team 2" --flag"#;
        let c = r#"C:\x\Claude.exe --user-data-dir=C:\P\Team --flag"#;
        assert_eq!(user_data_dir(a).as_deref(), Some(r"C:\P\Team 2"));
        assert_eq!(user_data_dir(b).as_deref(), Some(r"C:\P\Team 2"));
        assert_eq!(user_data_dir(c).as_deref(), Some(r"C:\P\Team"));
        assert_eq!(user_data_dir("Claude.exe --other"), None);
        assert!(!same_dir(r"C:\P\Team 2", r"C:\P\Team"));
        assert!(same_dir("c:/p/team/", r"C:\P\Team"));
    }
}
