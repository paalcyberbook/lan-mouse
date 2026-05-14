#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(feature = "remote_log"))]
use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
#[cfg(feature = "remote_log")]
use lan_mouse::remote_log;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use lan_mouse_cli::CliError;
#[cfg(feature = "gtk")]
use lan_mouse_gtk::GtkError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
use std::{
    future::Future,
    io,
    process::{self, Child},
};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum LanMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    // On Windows GUI-subsystem builds there is no attached console. If we were
    // launched from an existing cmd/PowerShell, attach to that console so
    // env_logger output is still visible in the terminal the user invoked us from.
    #[cfg(windows)]
    attach_parent_console();

    // init logging
    #[cfg(feature = "remote_log")]
    remote_log::init();
    #[cfg(not(feature = "remote_log"))]
    {
        let env = Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info");
        env_logger::init_from_env(env);
    }

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileA, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            // No parent console (launched from Explorer). Run silent; logs just disappear.
            return;
        }

        // After AttachConsole, Rust's stdout/stderr still point at whatever handles
        // the GUI-subsystem loader gave us (typically NULL), so writes vanish.
        // Rebind STD_OUTPUT_HANDLE / STD_ERROR_HANDLE to the attached console.
        let conout = CreateFileA(
            b"CONOUT$\0".as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if !conout.is_null() && conout != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, conout);
            SetStdHandle(STD_ERROR_HANDLE, conout);
        }
    }
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
            #[cfg(windows)]
            Command::Service => {
                // SCM dispatch entry. The service runs as LocalSystem in
                // session 0 and acts as a supervisor: it spawns
                // `lan-mouse.exe service-worker` into the active console
                // session at SYSTEM integrity (see lan_mouse_service::
                // session_helper). The supervisor doesn't run the daemon
                // itself — only the worker does, so the existing IPC port
                // bind happens exactly once in the user session.
                drop(config); // free the watcher before handing off
                lan_mouse_service::run_service_dispatch().map_err(|e| {
                    log::error!("SCM dispatch failed: {e}");
                    io::Error::other(e.to_string())
                })?;
            }
            #[cfg(windows)]
            Command::ServiceWorker => {
                // Spawned by the supervisor into the user's session at
                // SYSTEM integrity. From here on it's the regular daemon
                // — the only difference vs. plain `daemon` mode is that
                // we got here via CreateProcessAsUserW with a retargeted
                // SYSTEM token, so SendInput can reach UAC dialogs.
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service-worker: daemon already running on this host"),
                    r => r?,
                }
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(feature = "gtk")]
            {
                let mut service = start_service()?;
                let res = lan_mouse_gtk::run();
                #[cfg(unix)]
                {
                    // on unix we give the service a chance to terminate gracefully
                    let pid = service.id() as libc::pid_t;
                    unsafe {
                        libc::kill(pid, libc::SIGINT);
                    }
                    service.wait()?;
                }
                service.kill()?;
                res?;
            }
            #[cfg(not(feature = "gtk"))]
            {
                // run daemon if gtk is diabled
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

fn start_service() -> Result<Child, io::Error> {
    let child = process::Command::new(std::env::current_exe()?)
        .args(std::env::args().skip(1))
        .arg("daemon")
        .spawn()?;
    Ok(child)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
