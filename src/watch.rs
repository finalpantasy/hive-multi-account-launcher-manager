//! Change notifications instead of polling. One thread sleeps in
//! WaitForMultipleObjects on ReadDirectoryChangesW for the settings folder and
//! every profile folder, and wakes only when one of these files changes:
//!   hive.json                 settings edited (by hand or by the app)
//!   <profile>\lockfile        a Claude window started or closed
//!   plan-usage-history.json   Claude saved a new usage sample

use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, INFINITE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::win::wide_path;

pub const WM_WATCH: u32 = WM_APP + 6;
pub const CHANGED_CONFIG: usize = 1;
pub const CHANGED_LOCK: usize = 2;
pub const CHANGED_USAGE: usize = 4;
pub const CHANGED_EVENT: usize = 8;

static HWND: AtomicUsize = AtomicUsize::new(0);
static RESET: AtomicUsize = AtomicUsize::new(0);
static DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Start watching. `dirs[0]` is the settings folder, the rest are profiles.
pub fn start(hwnd: usize, dirs: Vec<PathBuf>) {
    HWND.store(hwnd, Ordering::SeqCst);
    *DIRS.lock().unwrap() = dirs;
    let reset = unsafe { CreateEventW(null(), 0, 0, null()) };
    RESET.store(reset as usize, Ordering::SeqCst);
    std::thread::spawn(run);
}

/// Replace the watched folders (accounts added or removed).
pub fn set_dirs(dirs: Vec<PathBuf>) {
    let mut cur = DIRS.lock().unwrap();
    if *cur != dirs {
        *cur = dirs;
        unsafe { SetEvent(RESET.load(Ordering::SeqCst) as HANDLE) };
    }
}

struct Watch {
    dir: HANDLE,
    event: HANDLE,
    ov: Box<OVERLAPPED>,
    buf: Vec<u32>,
    is_settings: bool,
}

impl Watch {
    fn arm(&mut self) -> bool {
        unsafe {
            ResetEvent(self.event);
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.event;
            *self.ov = ov;
            ReadDirectoryChangesW(
                self.dir,
                self.buf.as_mut_ptr() as *mut _,
                (self.buf.len() * 4) as u32,
                0,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE,
                null_mut(),
                &mut *self.ov,
                None,
            ) != 0
        }
    }

    /// Which kinds of change the completed read reported.
    fn collect(&self) -> usize {
        let mut bytes = 0u32;
        let ok = unsafe { GetOverlappedResult(self.dir, &*self.ov, &mut bytes, 0) } != 0;
        if !ok || bytes == 0 {
            // Buffer overflow: we don't know what changed, so refresh everything.
            return if self.is_settings { CHANGED_CONFIG | CHANGED_EVENT } else { CHANGED_LOCK | CHANGED_USAGE };
        }
        let base = self.buf.as_ptr() as *const u8;
        let mut offset = 0usize;
        let mut kinds = 0;
        loop {
            let info = unsafe { &*(base.add(offset) as *const FILE_NOTIFY_INFORMATION) };
            let len = info.FileNameLength as usize / 2;
            let name = unsafe { std::slice::from_raw_parts(info.FileName.as_ptr(), len) };
            let name = String::from_utf16_lossy(name).to_ascii_lowercase();
            kinds |= match (self.is_settings, name.as_str()) {
                (true, "hive.json") => CHANGED_CONFIG,
                (true, n) if n == crate::events::FILE => CHANGED_EVENT,
                (false, "lockfile") => CHANGED_LOCK,
                (false, "plan-usage-history.json") => CHANGED_USAGE,
                _ => 0,
            };
            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }
        kinds
    }
}

fn open(dir: &PathBuf, is_settings: bool) -> Option<Watch> {
    let name = wide_path(dir);
    let h = unsafe {
        CreateFileW(
            name.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED, null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut w = Watch {
        dir: h,
        event: unsafe { CreateEventW(null(), 1, 0, null()) },
        ov: Box::new(unsafe { std::mem::zeroed() }),
        buf: vec![0u32; 1024],
        is_settings,
    };
    if w.arm() {
        Some(w)
    } else {
        close(w);
        None
    }
}

fn close(w: Watch) {
    unsafe {
        CancelIoEx(w.dir, &*w.ov);
        let mut bytes = 0u32;
        GetOverlappedResult(w.dir, &*w.ov, &mut bytes, 1);
        CloseHandle(w.event);
        CloseHandle(w.dir);
    }
}

fn run() {
    let reset = RESET.load(Ordering::SeqCst) as HANDLE;
    loop {
        let dirs = DIRS.lock().unwrap().clone();
        let mut watches: Vec<Watch> = dirs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| open(d, i == 0))
            .collect();
        loop {
            let mut handles: Vec<HANDLE> = vec![reset];
            handles.extend(watches.iter().map(|w| w.event));
            let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
            let i = r.wrapping_sub(WAIT_OBJECT_0) as usize;
            if i == 0 {
                break; // folder list changed: rebuild
            }
            if i >= handles.len() {
                std::thread::sleep(std::time::Duration::from_secs(2)); // wait error: don't spin
                break;
            }
            let w = &mut watches[i - 1];
            let kinds = w.collect();
            if !w.arm() {
                std::thread::sleep(std::time::Duration::from_secs(2)); // folder gone: don't spin
                break;
            }
            if kinds != 0 {
                unsafe { PostMessageW(HWND.load(Ordering::SeqCst) as _, WM_WATCH, kinds, 0) };
            }
        }
        for w in watches {
            close(w);
        }
    }
}
