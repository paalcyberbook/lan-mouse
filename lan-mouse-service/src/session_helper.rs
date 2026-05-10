//! Cross-session input bridge for the Windows Service.
//!
//! When the Lan Mouse+ service runs as `LocalSystem`, it lives in session 0
//! and has no desktop. `SendInput` calls from there are silently dropped —
//! the input never reaches the user's interactive desktop. To work around
//! this, the service spawns a *worker* process in the active console
//! session at SYSTEM integrity. The worker runs the regular daemon code
//! and can `SendInput` into any window, including UAC dialogs and other
//! High-integrity processes.
//!
//! Mechanics (this is the same trick used by RustDesk / AnyDesk):
//!
//! 1. We open SYSTEM's own primary token (we *are* SYSTEM as a service).
//! 2. `DuplicateTokenEx` copies it to a new primary token.
//! 3. `SetTokenInformation(TokenSessionId)` retargets that token to the
//!    active console session — possible because LocalSystem has
//!    `SeTcbPrivilege`.
//! 4. `CreateProcessAsUserW` spawns `lan-mouse.exe service-worker` with
//!    the retargeted token. The new process lives in the user's session
//!    but at SYSTEM integrity, so it has a desktop and can inject input.
//!
//! The service then babysits the worker: waits for it to exit, restarts
//! it on session change (logoff/login), and terminates it on SCM Stop.

#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

use super::Error;

/// Marker session ID returned by `WTSGetActiveConsoleSessionId` when
/// nobody is logged on at the console.
const NO_ACTIVE_SESSION: u32 = 0xFFFF_FFFF;

/// Owns the handles for a spawned worker so they're closed on drop.
pub struct WorkerHandle {
    process: HANDLE,
    thread: HANDLE,
}

unsafe impl Send for WorkerHandle {}
unsafe impl Sync for WorkerHandle {}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        unsafe {
            close_if(self.process);
            close_if(self.thread);
        }
    }
}

unsafe fn close_if(h: HANDLE) {
    if !h.is_null() && h != INVALID_HANDLE_VALUE {
        unsafe {
            CloseHandle(h);
        }
    }
}

/// Spawn `<exe_path> --service-worker` in the active console session at
/// SYSTEM integrity. Returns `Ok(None)` if no user is logged on yet — the
/// caller should retry.
pub fn spawn_worker(exe_path: &OsStr) -> Result<Option<WorkerHandle>, Error> {
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == NO_ACTIVE_SESSION {
            return Ok(None);
        }

        // Step 1: get our own (SYSTEM) primary token.
        let mut current: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut current,
        ) == 0
        {
            return Err(Error::Os(
                "OpenProcessToken",
                std::io::Error::last_os_error(),
            ));
        }

        // Step 2: duplicate as a new primary token.
        let mut dup: HANDLE = std::ptr::null_mut();
        let dup_ok = DuplicateTokenEx(
            current,
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityIdentification,
            TokenPrimary,
            &mut dup,
        );
        close_if(current);
        if dup_ok == 0 {
            return Err(Error::Os(
                "DuplicateTokenEx",
                std::io::Error::last_os_error(),
            ));
        }

        // Step 3: retarget to the active console session.
        let session_val: u32 = session;
        if SetTokenInformation(
            dup,
            TokenSessionId,
            &session_val as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        ) == 0
        {
            close_if(dup);
            return Err(Error::Os(
                "SetTokenInformation(TokenSessionId)",
                std::io::Error::last_os_error(),
            ));
        }

        // Step 4: build the user's environment block (so the worker sees
        // their HOMEPATH, APPDATA, etc.) and command line.
        let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
        if CreateEnvironmentBlock(&mut env, dup, 0) == 0 {
            close_if(dup);
            return Err(Error::Os(
                "CreateEnvironmentBlock",
                std::io::Error::last_os_error(),
            ));
        }

        let exe_w: Vec<u16> = exe_path.encode_wide().chain(std::iter::once(0)).collect();
        // CreateProcessAsUserW requires lpCommandLine to be writable, and
        // the path needs surrounding quotes if it contains spaces. The
        // worker is launched as the `service-worker` clap subcommand.
        let mut cmd_line: Vec<u16> = std::iter::once(b'"' as u16)
            .chain(exe_path.encode_wide())
            .chain(b"\" service-worker\0".iter().map(|&b| b as u16))
            .collect();
        // STARTUPINFO.lpDesktop must be set to the user's interactive
        // desktop so the worker can show windows and inject input.
        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let create_ok = CreateProcessAsUserW(
            dup,
            exe_w.as_ptr(),
            cmd_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // bInheritHandles
            CREATE_UNICODE_ENVIRONMENT,
            env,
            std::ptr::null(),
            &si,
            &mut pi,
        );
        DestroyEnvironmentBlock(env);
        close_if(dup);

        if create_ok == 0 {
            return Err(Error::Os(
                "CreateProcessAsUserW",
                std::io::Error::last_os_error(),
            ));
        }

        log::info!(
            "spawned service-worker (pid={}) into session {}",
            pi.dwProcessId,
            session
        );
        Ok(Some(WorkerHandle {
            process: pi.hProcess,
            thread: pi.hThread,
        }))
    }
}

/// Wait for the worker to exit OR for the shutdown flag to flip OR for
/// the active console session to change. Returns the reason.
pub enum WaitOutcome {
    Exited,
    Shutdown,
    SessionChanged,
}

pub fn wait_for_worker(
    handle: &WorkerHandle,
    shutdown: &Arc<AtomicBool>,
    pinned_session: u32,
) -> WaitOutcome {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return WaitOutcome::Shutdown;
        }
        let r = unsafe { WaitForSingleObject(handle.process, 1000) };
        if r == WAIT_OBJECT_0 {
            return WaitOutcome::Exited;
        }
        if r != WAIT_TIMEOUT {
            // Defensive: something is wrong with our handle. Treat as
            // exit so the supervisor restarts cleanly.
            return WaitOutcome::Exited;
        }
        // Detect login/logoff transitions so we can hand off to the new
        // session quickly rather than waiting until the worker dies on
        // its own (which it usually won't, since it doesn't notice).
        let active = unsafe { WTSGetActiveConsoleSessionId() };
        if active != pinned_session && active != NO_ACTIVE_SESSION {
            log::info!(
                "console session changed: {} → {}; restarting worker",
                pinned_session,
                active
            );
            return WaitOutcome::SessionChanged;
        }
    }
}

/// Try to terminate the worker cleanly. Falls back to TerminateProcess.
pub fn terminate_worker(handle: WorkerHandle) {
    unsafe {
        TerminateProcess(handle.process, 0);
        // handle drops, closing the HANDLEs.
    }
}

/// Service supervisor loop: keep a worker running in the active user
/// session for as long as the service is up.
pub fn supervise(exe_path: &OsStr, shutdown: Arc<AtomicBool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == NO_ACTIVE_SESSION {
            // No user logged on. Poll until someone arrives.
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        match spawn_worker(exe_path) {
            Ok(Some(handle)) => {
                backoff = Duration::from_secs(1); // reset on success
                match wait_for_worker(&handle, &shutdown, session) {
                    WaitOutcome::Shutdown => {
                        terminate_worker(handle);
                        return;
                    }
                    WaitOutcome::SessionChanged => {
                        terminate_worker(handle);
                        // immediately try again with the new session
                    }
                    WaitOutcome::Exited => {
                        log::warn!("service-worker exited; restarting after backoff");
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => {
                log::error!("failed to spawn service-worker: {e}");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}
