//! Windows Service wrapper for Lan Mouse+.
//!
//! Two responsibilities:
//!
//! 1. **Service lifecycle management** ([`install`], [`uninstall`], [`start`],
//!    [`stop`], [`status`]). Talks to the SCM (Service Control Manager) to
//!    register/control the `LanMousePlus` service. All of these require
//!    administrator rights to call — they will fail with `ERROR_ACCESS_DENIED`
//!    when invoked from a non-elevated process.
//!
//! 2. **Service entry point** ([`run_service_dispatch`]). Called from
//!    `main.rs` when the binary is launched with `--service`. Hands control
//!    to SCM's dispatch loop, registers a control handler that responds to
//!    Stop, and invokes the user-supplied daemon-runner closure.
//!
//! ## Session 0 isolation
//!
//! The service installs as `LocalSystem` and starts in session 0, which
//! has no desktop. To actually reach the user's interactive session, the
//! SCM dispatch entry point doesn't run the daemon directly — it spawns a
//! *worker* process in the active console session at SYSTEM integrity,
//! via the [`session_helper`] module. The worker runs the real daemon
//! code (`lan-mouse.exe --service-worker`) and is what `SendInput`
//! actually flows through. The service supervises the worker, restarting
//! it on session change (logoff/login) and terminating it on SCM Stop.

#![cfg(windows)]

mod session_helper;

use std::ffi::{OsStr, OsString};
use std::time::Duration;

use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

/// SCM service name — used as the registry key and for `sc.exe`/CLI
/// commands. Keep this stable; renaming would orphan an existing install.
pub const SERVICE_NAME: &str = "LanMousePlus";
/// Human-readable name shown in services.msc.
pub const SERVICE_DISPLAY_NAME: &str = "Lan Mouse+";
pub const SERVICE_DESCRIPTION: &str =
    "Lan Mouse+ daemon for cross-PC mouse, keyboard, and clipboard sharing.";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("windows-service error: {0}")]
    Win(#[from] windows_service::Error),
    #[error("service is already installed")]
    AlreadyInstalled,
    #[error("service is not installed")]
    NotInstalled,
    #[error("Win32 {0}: {1}")]
    Os(&'static str, #[source] std::io::Error),
}

/// Install the service. Registers `lan-mouse.exe --service` with the SCM
/// under the `LanMousePlus` name, set to start on boot. The caller must be
/// elevated.
pub fn install(exe_path: &OsStr) -> Result<(), Error> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;

    // Be idempotent: if the service is already installed, don't error out
    // — surface a typed result so callers can choose what to do.
    if open_service(&manager, ServiceAccess::QUERY_STATUS).is_ok() {
        return Err(Error::AlreadyInstalled);
    }

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path.into(),
        // SCM invokes `lan-mouse.exe service`. The service entry is the
        // supervisor that spawns the in-session worker — see service_main
        // and the session_helper module.
        launch_arguments: vec![OsString::from("service")],
        dependencies: vec![],
        // None = LocalSystem. Required so the supervisor can duplicate
        // SYSTEM's token and retarget it to the user session via
        // SetTokenInformation(TokenSessionId), which needs SeTcbPrivilege.
        account_name: None,
        account_password: None,
    };

    let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
    service.set_description(SERVICE_DESCRIPTION)?;
    Ok(())
}

/// Uninstall the service. Stops it first if it's running.
pub fn uninstall() -> Result<(), Error> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = match open_service(
        &manager,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(s) => s,
        Err(_) => return Err(Error::NotInstalled),
    };

    let status = service.query_status()?;
    if status.current_state != ServiceState::Stopped {
        let _ = service.stop();
        // Wait briefly for the service to transition. We don't loop forever
        // — SCM will return BUSY if another stop is in flight.
        std::thread::sleep(Duration::from_secs(1));
    }
    service.delete()?;
    Ok(())
}

/// Start the (already-installed) service.
pub fn start() -> Result<(), Error> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = open_service(&manager, ServiceAccess::START)?;
    service.start::<&OsStr>(&[])?;
    Ok(())
}

/// Stop the service.
pub fn stop() -> Result<(), Error> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = open_service(&manager, ServiceAccess::STOP)?;
    service.stop()?;
    Ok(())
}

/// Lightweight status snapshot used by the CLI / settings UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    Other,
}

pub fn status() -> Result<InstalledState, Error> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = match open_service(&manager, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => {
            // ERROR_SERVICE_DOES_NOT_EXIST
            return Ok(InstalledState::NotInstalled);
        }
        Err(e) => return Err(e.into()),
    };
    let s = service.query_status()?;
    Ok(match s.current_state {
        ServiceState::Stopped => InstalledState::Stopped,
        ServiceState::StartPending => InstalledState::StartPending,
        ServiceState::StopPending => InstalledState::StopPending,
        ServiceState::Running => InstalledState::Running,
        _ => InstalledState::Other,
    })
}

fn open_service(
    manager: &ServiceManager,
    access: ServiceAccess,
) -> Result<windows_service::service::Service, windows_service::Error> {
    manager.open_service(SERVICE_NAME, access)
}

// --- Service runtime entry --------------------------------------------------

define_windows_service!(ffi_service_main, service_main);

/// Hand control to the SCM dispatch loop. Blocks until the service stops.
///
/// Once SCM transitions the service to Running, [`service_main`] takes
/// over and runs the supervisor — which spawns and re-spawns the
/// `--service-worker` process inside the active console session at
/// SYSTEM integrity. SCM's Stop / Shutdown controls flip a shared flag
/// that the supervisor and the worker-wait loop observe.
pub fn run_service_dispatch() -> Result<(), Error> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let shutdown = Arc::new(AtomicBool::new(false));
    let s = shutdown.clone();
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                s.store(true, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
        Ok(h) => h,
        Err(e) => {
            log::error!("service control handler register failed: {e}");
            return;
        }
    };

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });

    // Supervise an in-session worker for as long as the service is up.
    match std::env::current_exe() {
        Ok(exe) => session_helper::supervise(exe.as_os_str(), shutdown),
        Err(e) => log::error!("could not resolve own exe path: {e}"),
    }

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
}
