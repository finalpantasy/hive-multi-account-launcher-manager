//! Opens accounts one at a time with a gap between new windows, then hands
//! Claude in Chrome back to the chosen account (every new window grabs it).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::claude;
use crate::config::{Account, Config};
use crate::log;

pub const WM_NOTE: u32 = WM_APP + 4;

static QUEUE: Mutex<VecDeque<(Account, Config)>> = Mutex::new(VecDeque::new());
static WORKER: AtomicBool = AtomicBool::new(false);
static NOTES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// Tray window to tell about notes; 0 when running as a command.
pub static NOTIFY_HWND: AtomicUsize = AtomicUsize::new(0);

pub fn note(title: String, text: String) {
    NOTES.lock().unwrap().push((title, text));
    let h = NOTIFY_HWND.load(Ordering::SeqCst);
    if h != 0 {
        unsafe { PostMessageW(h as _, WM_NOTE, 0, 0) };
    }
}

/// Oldest pending note: one per WM_NOTE, so none are skipped.
pub fn next_note() -> Option<(String, String)> {
    let mut notes = NOTES.lock().unwrap();
    (!notes.is_empty()).then(|| notes.remove(0))
}

pub fn take_notes() -> Vec<(String, String)> {
    std::mem::take(&mut *NOTES.lock().unwrap())
}

fn push(cfg: &Config, list: Vec<Account>) {
    let mut q = QUEUE.lock().unwrap();
    for a in list {
        if !q.iter().any(|(x, _)| x.name == a.name) {
            q.push_back((a, cfg.clone()));
        }
    }
}

/// Queue accounts and open them on a background thread.
pub fn enqueue(cfg: &Config, list: Vec<Account>) {
    push(cfg, list);
    if !WORKER.swap(true, Ordering::SeqCst) {
        std::thread::spawn(worker);
    }
}

/// Open accounts on this thread and return when done (command-line use).
pub fn run_blocking(cfg: &Config, list: Vec<Account>) {
    push(cfg, list);
    if !WORKER.swap(true, Ordering::SeqCst) {
        worker();
    }
}

fn queue_empty() -> bool {
    QUEUE.lock().unwrap().is_empty()
}

fn worker() {
    let mut last: Option<Config> = None;
    let mut opened_any = false;
    loop {
        let next = QUEUE.lock().unwrap().pop_front();
        if let Some((a, cfg)) = next {
            let was_open = claude::is_running(&cfg, &a);
            if !was_open && opened_any {
                sleep(Duration::from_secs(cfg.launch_delay_seconds as u64));
            }
            match claude::launch(&cfg, &a) {
                Ok(()) => {
                    log(&format!("{} {}", if was_open { "showed" } else { "opened" }, a.name));
                    if !was_open {
                        crate::tags::expect_window(&a.name);
                    }
                }
                Err(e) => {
                    log(&format!("could not open {}: {e}", a.name));
                    note(format!("Couldn't open {}", a.name), e);
                }
            }
            opened_any |= !was_open;
            last = Some(cfg);
            continue;
        }
        if opened_any {
            // New windows register as the Chrome host while they start, and a slow
            // start (cold sign-in) can do that late: hand Chrome back at +15 s and +45 s.
            let mut interrupted = false;
            for wait in [15, 30] {
                for _ in 0..wait {
                    sleep(Duration::from_secs(1));
                    if !queue_empty() {
                        break;
                    }
                }
                if !queue_empty() {
                    interrupted = true;
                    break;
                }
                if let Some(cfg) = &last {
                    hand_chrome_back(cfg);
                }
            }
            if interrupted {
                continue;
            }
            opened_any = false;
        }
        WORKER.store(false, Ordering::SeqCst);
        if queue_empty() || WORKER.swap(true, Ordering::SeqCst) {
            break;
        }
    }
}

fn hand_chrome_back(snapshot: &Config) {
    let cfg = Config::load(&crate::config_path()).unwrap_or_else(|_| snapshot.clone());
    let Some(a) = cfg.find(&cfg.chrome_account) else { return };
    if claude::chrome_shared() || !claude::is_running(&cfg, a) {
        return;
    }
    match claude::set_chrome(&cfg, a) {
        Ok(_) => log(&format!("Claude in Chrome handed to {}", a.name)),
        Err(e) => log(&format!("Chrome hand-back to {} failed: {e}", a.name)),
    }
}
