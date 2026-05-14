use clap::{Args, Parser, Subcommand};
use futures::StreamExt;

use std::{net::IpAddr, path::PathBuf, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    AsyncFrontendEventReader, AsyncFrontendRequestWriter, ClientHandle, ConnectionError,
    FrontendEvent, FrontendRequest, IpcError, Position, connect_async,
};

#[cfg(feature = "remote_log")]
mod logger;

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
    /// Send a file or folder to a client. Wired into right-click context
    /// menus by the installer/Nautilus script ("Send via Lan Mouse+").
    /// With `--client`, sends to that specific client; without it, picks
    /// the currently-active (cursor-on) client, falling back to the only
    /// connected one.
    SendFile {
        /// Path to the file or folder to send.
        #[arg(long)]
        path: PathBuf,
        /// Optional explicit destination client handle. If omitted, the
        /// currently-active client is used (or the only connected one).
        #[arg(long)]
        client: Option<ClientHandle>,
    },
    /// (Windows only) Manage the Lan Mouse+ Windows Service. Letting the
    /// daemon run as a service lets it accept input even when you're at
    /// the lock screen or a UAC prompt is on top. All subcommands except
    /// `status` need administrator rights.
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Manage this host's cb-logger registration (`remote_log` feature).
    /// The daemon registers itself on first run with LOGGER_APIKEY set;
    /// these subcommands let you join a multi-host group and inspect
    /// state without going through curl.
    #[cfg(feature = "remote_log")]
    Logger {
        #[command(subcommand)]
        action: LoggerAction,
    },
}

#[cfg(feature = "remote_log")]
#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
pub enum LoggerAction {
    /// Show this host's registered client name + id and which group, if
    /// any, it's a member of.
    Status,
    /// Create a new logging group with the given display name. Caches
    /// the response (including the owner-only invite_code) locally so
    /// `status` can show it.
    Create { name: String },
    /// Join a logging group via an invite code (printed by `create` or
    /// shown in `status` on the owner host).
    Join { invite_code: String },
    /// List the groups this client owns / is a member of.
    Groups,
    /// Print recent log entries from the currently-saved group (falls
    /// back to this host's own logs if no group is joined).
    Tail {
        /// Max entries to fetch (server caps at 10000; default 50).
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Filter by level — `info`, `warn`, `error`, etc.
        #[arg(long)]
        level: Option<String>,
        /// Force per-client view even if a group is saved.
        #[arg(long)]
        mine_only: bool,
    },
}

#[cfg(windows)]
#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    /// Register the service with the SCM. Subsequent boots will start it
    /// automatically. Requires admin.
    Install,
    /// Stop and remove the service. Requires admin.
    Uninstall,
    /// Start the service (must be installed). Requires admin.
    Start,
    /// Stop the service. Requires admin.
    Stop,
    /// Print the current state (NotInstalled / Stopped / Running / …).
    Status,
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    // Subcommands that don't need the running daemon. Handle them before
    // connect_async so they work whether or not the service is up.
    #[cfg(feature = "remote_log")]
    if let CliSubcommand::Logger { action } = cmd.clone() {
        return handle_logger_action(action);
    }
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            enter_hook,
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(enter_hook) = enter_hook {
                        tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
                    for (handle, config, state) in clients {
                        let host = config.hostname.unwrap_or("unknown".to_owned());
                        let port = config.port;
                        let pos = config.pos;
                        let active = state.active;
                        let ips = state.ips;
                        println!(
                            "id {handle}: {host}:{port} ({pos}) active: {active}, ips: {ips:?}"
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
        CliSubcommand::SendFile { path, client } => {
            send_file(&mut rx, &mut tx, path, client).await?;
        }
        #[cfg(windows)]
        CliSubcommand::Service { action } => {
            handle_service_action(action);
        }
        #[cfg(feature = "remote_log")]
        CliSubcommand::Logger { .. } => unreachable!("handled before IPC connect"),
    }
    Ok(())
}

#[cfg(feature = "remote_log")]
fn handle_logger_action(action: LoggerAction) -> Result<(), CliError> {
    let res = match action {
        LoggerAction::Status => logger::status(),
        LoggerAction::Create { name } => logger::create(&name),
        LoggerAction::Join { invite_code } => logger::join(&invite_code),
        LoggerAction::Groups => logger::groups(),
        LoggerAction::Tail {
            limit,
            level,
            mine_only,
        } => logger::tail(limit, level.as_deref(), mine_only),
    };
    if let Err(e) = res {
        eprintln!("{e}");
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(windows)]
fn handle_service_action(action: ServiceAction) {
    use lan_mouse_service as svc;
    match action {
        ServiceAction::Install => match std::env::current_exe() {
            Ok(exe) => match svc::install(exe.as_os_str()) {
                Ok(()) => {
                    println!("installed Lan Mouse+ service");
                    notify_user("Lan Mouse+ service installed.", false);
                }
                Err(svc::Error::AlreadyInstalled) => {
                    println!("service is already installed");
                    notify_user("Lan Mouse+ service is already installed.", false);
                }
                Err(e) => {
                    eprintln!("install failed: {e}");
                    notify_user(
                        &format!(
                            "Could not install Lan Mouse+ service.\n\n{e}\n\n\
                             Run this command from an elevated PowerShell or Command Prompt \
                             (right-click → Run as administrator)."
                        ),
                        true,
                    );
                }
            },
            Err(e) => {
                eprintln!("could not resolve current exe: {e}");
                notify_user(&format!("Could not locate lan-mouse.exe: {e}"), true);
            }
        },
        ServiceAction::Uninstall => match svc::uninstall() {
            Ok(()) => {
                println!("uninstalled Lan Mouse+ service");
                notify_user("Lan Mouse+ service uninstalled.", false);
            }
            Err(svc::Error::NotInstalled) => {
                println!("service is not installed");
                notify_user("Lan Mouse+ service was not installed.", false);
            }
            Err(e) => {
                eprintln!("uninstall failed: {e}");
                notify_user(&format!("Uninstall failed: {e}"), true);
            }
        },
        ServiceAction::Start => match svc::start() {
            Ok(()) => println!("started"),
            Err(e) => {
                eprintln!("start failed: {e}");
                notify_user(&format!("Could not start service: {e}"), true);
            }
        },
        ServiceAction::Stop => match svc::stop() {
            Ok(()) => println!("stopped"),
            Err(e) => {
                eprintln!("stop failed: {e}");
                notify_user(&format!("Could not stop service: {e}"), true);
            }
        },
        ServiceAction::Status => match svc::status() {
            Ok(s) => println!("{s:?}"),
            Err(e) => eprintln!("status query failed: {e}"),
        },
    }
}

async fn send_file(
    rx: &mut AsyncFrontendEventReader,
    tx: &mut AsyncFrontendRequestWriter,
    path: PathBuf,
    explicit: Option<ClientHandle>,
) -> Result<(), CliError> {
    if !path.exists() {
        let msg = format!("Path not found: {}", path.display());
        eprintln!("{msg}");
        notify_user(&msg, true);
        return Ok(());
    }

    let target = if let Some(handle) = explicit {
        handle
    } else {
        match resolve_default_target(rx, tx).await? {
            Some(h) => h,
            None => {
                let msg = "No connected Lan Mouse+ client available. Open Lan Mouse+ \
                          and connect/activate the destination client first."
                    .to_string();
                eprintln!("{msg}");
                notify_user(&msg, true);
                return Ok(());
            }
        }
    };

    tx.request(FrontendRequest::SendFile {
        client: target,
        path: path.clone(),
    })
    .await?;

    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let msg = format!("Sending \"{name}\" to client {target}…");
    println!("{msg}");
    notify_user(&msg, false);
    Ok(())
}

/// Resolve which client should receive a context-menu send when the user
/// didn't pass `--client`. Preference order:
///   1. The currently-active client (the one the cursor is over / would
///      enter on the next cross). This matches the clipboard cursor-cross
///      semantics and "the one I'm currently working with."
///   2. The single connected (state.ips populated) client, if exactly one
///      such client exists.
///   3. None — caller surfaces a "no destination" error to the user.
async fn resolve_default_target(
    rx: &mut AsyncFrontendEventReader,
    tx: &mut AsyncFrontendRequestWriter,
) -> Result<Option<ClientHandle>, CliError> {
    tx.request(FrontendRequest::Enumerate()).await?;
    while let Some(e) = rx.next().await {
        if let FrontendEvent::Enumerate(clients) = e? {
            // Active client first.
            if let Some((h, _, _)) = clients.iter().find(|(_, _, s)| s.active) {
                return Ok(Some(*h));
            }
            // Otherwise, the only client with at least one known IP.
            let connected: Vec<ClientHandle> = clients
                .iter()
                .filter(|(_, _, s)| !s.ips.is_empty())
                .map(|(h, _, _)| *h)
                .collect();
            if connected.len() == 1 {
                return Ok(Some(connected[0]));
            }
            return Ok(None);
        }
    }
    Ok(None)
}

/// Surface a message to the user. On Windows we pop a `MessageBoxW` since
/// the CLI is invoked from a shell verb where stdout/stderr are invisible;
/// on other platforms this is a no-op (Explorer right-click only exists on
/// Windows, so the CLI path is rarely user-facing elsewhere).
#[cfg(windows)]
fn notify_user(msg: &str, error: bool) {
    use std::iter::once;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
    };

    let title: Vec<u16> = "Lan Mouse+".encode_utf16().chain(once(0)).collect();
    let body: Vec<u16> = msg.encode_utf16().chain(once(0)).collect();
    let icon = if error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    // SAFETY: pointers are NUL-terminated UTF-16 buffers backed by `title`
    // and `body` which outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | icon,
        );
    }
}

/// Linux/macOS: shell out to `notify-send` so the right-click context-menu
/// path on file managers like Nautilus/Thunar/Nemo gets visible feedback.
/// We don't fail if the binary is missing — stdout/stderr is the fallback.
#[cfg(not(windows))]
fn notify_user(msg: &str, error: bool) {
    let urgency = if error { "critical" } else { "normal" };
    let _ = std::process::Command::new("notify-send")
        .args([
            "--app-name=Lan Mouse+",
            "--urgency",
            urgency,
            "Lan Mouse+",
            msg,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
