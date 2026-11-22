//! Single-instance ownership with graceful handover between versions.
//!
//! Releases are deployed into versioned folders (`BrightWheel-<ver>`), so an
//! upgrade can leave the previous binary resident in the tray. A named mutex
//! elects the owner; a named event lets a newer build ask the owner to quit
//! through Slint's event loop, so the tray icon, timers and windows are torn
//! down normally instead of being killed.

use std::time::Duration;

use anyhow::{Result, bail};

const MUTEX_NAME: &str = r"Local\BrightWheel.SingleInstance";
const QUIT_EVENT_NAME: &str = r"Local\BrightWheel.QuitRequest";
const PROCESS_NAME: &str = "BrightWheel.exe";

/// Holds the named mutex for the lifetime of the process. Windows releases it
/// automatically if the process dies, so a crash cannot wedge future launches.
pub struct InstanceGuard {
    #[cfg(windows)]
    mutex: isize,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            let _ = CloseHandle(self.mutex as *mut core::ffi::c_void);
        }
    }
}

/// Become the single resident instance. If another instance owns the mutex,
/// ask it to retire gracefully and wait for it to release ownership.
#[cfg(windows)]
pub fn acquire(grace: Duration) -> Result<InstanceGuard> {
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

    let name: Vec<u16> = format!("{MUTEX_NAME}\0").encode_utf16().collect();
    unsafe {
        // Ask for immediate ownership: the first process becomes the owner,
        // later processes only get a handle to the existing object.
        let mutex = CreateMutexW(ptr::null(), 1, name.as_ptr());
        if mutex.is_null() {
            bail!("CreateMutexW failed");
        }

        if GetLastError() == ERROR_ALREADY_EXISTS {
            let signalled = request_quit();
            let timeout = grace.as_millis().min(u32::MAX as u128) as u32;
            let mut released = signalled && WaitForSingleObject(mutex, timeout) == WAIT_OBJECT_0;
            if !released {
                // Old builds have no quit listener. Nothing to do gracefully,
                // so retire the stale process before taking over.
                force_stop_previous();
                released = WaitForSingleObject(mutex, 2000) == WAIT_OBJECT_0;
            }
            if !released {
                let _ = CloseHandle(mutex);
                bail!("a previous BrightWheel instance is still running");
            }
        }

        Ok(InstanceGuard {
            mutex: mutex as isize,
        })
    }
}

#[cfg(not(windows))]
pub fn acquire(_grace: Duration) -> Result<InstanceGuard> {
    Ok(InstanceGuard {})
}

/// Signal the running instance's quit event. Returns true when an owner with
/// a listener was found (older builds have no such event).
#[cfg(windows)]
fn request_quit() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};

    let name: Vec<u16> = format!("{QUIT_EVENT_NAME}\0").encode_utf16().collect();
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if event.is_null() {
            return false;
        }
        let _ = SetEvent(event);
        let _ = CloseHandle(event);
        true
    }
}

/// Last-resort retirement for builds that predate the quit listener.
#[cfg(windows)]
fn force_stop_previous() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_TERMINATE, TerminateProcess,
    };

    let me = std::process::id();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snapshot, &mut entry);
        while ok != 0 {
            let raw: Vec<u16> = entry
                .szExeFile
                .iter()
                .take_while(|unit| **unit != 0)
                .copied()
                .collect();
            let name = String::from_utf16_lossy(&raw);
            if name.eq_ignore_ascii_case(PROCESS_NAME) && entry.th32ProcessID != me {
                let process = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
                if !process.is_null() {
                    let _ = TerminateProcess(process, 0);
                    let _ = CloseHandle(process);
                }
            }
            ok = Process32NextW(snapshot, &mut entry);
        }
        let _ = CloseHandle(snapshot);
    }
}

/// Start listening for a newer build's takeover request. Must be called after
/// `acquire` succeeds, so only the owning instance can be asked to retire.
#[cfg(windows)]
pub fn install_quit_listener() -> Result<()> {
    use std::ptr;
    use std::thread;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, WaitForSingleObject,
    };

    let name: Vec<u16> = format!("{QUIT_EVENT_NAME}\0").encode_utf16().collect();
    unsafe {
        let event = CreateEventW(ptr::null(), 0, 0, name.as_ptr());
        if event.is_null() {
            bail!("CreateEventW failed");
        }
        // The previous owner may have left the shared event signalled while
        // handing over; start from a clean, unsignalled state.
        let _ = ResetEvent(event);
        // Raw handles are not Send; carry the numeric value into the thread.
        let event_bits = event as isize;
        thread::spawn(move || {
            let event = event_bits as *mut core::ffi::c_void;
            if WaitForSingleObject(event, INFINITE) != WAIT_FAILED {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            let _ = CloseHandle(event);
        });
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn install_quit_listener() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_names_are_session_local_and_stable() {
        assert!(MUTEX_NAME.starts_with(r"Local\BrightWheel."));
        assert!(QUIT_EVENT_NAME.starts_with(r"Local\BrightWheel."));
        assert_ne!(MUTEX_NAME, QUIT_EVENT_NAME);
    }

    #[test]
    fn target_process_name_is_the_release_binary() {
        assert_eq!(PROCESS_NAME, "BrightWheel.exe");
    }
}
