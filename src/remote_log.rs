//! Remote log shipper for cross-OS debugging.
//!
//! Fans out every `log::*!` call to a hosted `cb-logger` instance via
//! HTTP bulk, so Linux / Windows / macOS daemons can be compared in a
//! single timeline. Compares are usually done by creating a group on
//! the logger service, joining all three daemons, and reading the
//! group's combined log view.
//!
//! Opt-in: set `LOGGER_APIKEY` (or `CB_LOGGER_API_KEY`). Absent → the
//! shipper is disabled and logging behaves exactly like plain
//! `env_logger`. The shared API key is only used once, to register and
//! obtain a bearer token; the token is cached at
//! `$XDG_CONFIG_HOME/lan-mouse/remote-log-token.json` (0600 on Unix)
//! and reused across restarts.
//!
//! The shipper runs on a dedicated `std::thread` with a sync `mpsc`
//! channel and `ureq`'s blocking HTTP client, so logger init does not
//! depend on the tokio runtime that the daemon spins up later.
//! Network failures are dropped silently — logging must never block
//! or panic the daemon.

use std::{
    env,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread,
    time::{Duration, Instant},
};

use log::{Level, Log, Metadata, Record};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DEFAULT_HOST: &str = "https://logger.cyberbook.id";
const TOKEN_FILE: &str = "remote-log-token.json";
const STATUS_FILE: &str = "remote-log-status.txt";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const BATCH_INTERVAL: Duration = Duration::from_millis(1000);
const MAX_BATCH: usize = 256;
const QUEUE_CAP: usize = 4096;

/// Initialise logging. Wraps `env_logger` with a fan-out to the remote
/// shipper iff an API key is present in the environment. Call exactly
/// once, before any `log::*!` macro fires.
pub fn init() {
    let env = env_logger::Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info");
    let mut builder = env_logger::Builder::from_env(env);
    builder.format_timestamp_millis();
    let inner = builder.build();
    let max_level = inner.filter();

    let remote = match try_init_remote() {
        Ok(Some(handle)) => Some(handle),
        Ok(None) => None,
        Err(e) => {
            eprintln!("remote_log: disabled ({e})");
            None
        }
    };

    let logger = FanoutLogger { inner, remote };
    log::set_max_level(max_level);
    if let Err(e) = log::set_boxed_logger(Box::new(logger)) {
        eprintln!("remote_log: set_boxed_logger failed: {e}");
    }
}

struct FanoutLogger {
    inner: env_logger::Logger,
    remote: Option<RemoteHandle>,
}

impl Log for FanoutLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        self.inner.log(record);
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        if let Some(handle) = &self.remote {
            let entry = LogEntry {
                level: level_str(record.level()),
                message: format!("{}", record.args()),
                target: record.target().to_string(),
                module: record.module_path().map(|s| s.to_string()),
            };
            match handle.tx.try_send(entry) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    // shipper is falling behind / offline — drop on the
                    // floor rather than block the daemon.
                }
                Err(TrySendError::Disconnected(_)) => {
                    // shipper thread has died; nothing useful we can do.
                }
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

struct LogEntry {
    level: &'static str,
    message: String,
    target: String,
    module: Option<String>,
}

struct RemoteHandle {
    tx: SyncSender<LogEntry>,
}

#[derive(Serialize, Deserialize)]
struct TokenCache {
    client_id: String,
    token: String,
    name: String,
}

fn try_init_remote() -> Result<Option<RemoteHandle>, String> {
    if env::var("LAN_MOUSE_REMOTE_LOG_DISABLE").is_ok() {
        write_status("disabled by LAN_MOUSE_REMOTE_LOG_DISABLE");
        return Ok(None);
    }
    let host = env::var("CB_LOGGER_HOST").unwrap_or_else(|_| DEFAULT_HOST.into());

    let host_name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".into());
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let version = env!("CARGO_PKG_VERSION");
    let client_name = format!("lan-mouse-{os}-{host_name}");
    let metadata = json!({
        "os": os,
        "arch": arch,
        "hostname": host_name,
        "version": version,
    });

    let token_path = token_path()?;

    // The bearer token is what authorises log pushes; the API key is only
    // ever needed for one-time registration. So: load the cache first,
    // and only fall back to register-from-API-key when no usable cache
    // exists. Means a user who ran the register helper once doesn't have
    // to keep `LOGGER_APIKEY` in their environment forever — handy on
    // Windows where Explorer-launched processes inherit a stale env.
    let (token, source) = match load_cached_token(&token_path, &client_name) {
        Some(t) => (t, "cached token"),
        None => match read_api_key() {
            Some(api_key) => {
                let t = register_new(&host, &api_key, &client_name, &metadata, &token_path)?;
                (t, "freshly registered")
            }
            None => {
                let msg = format!(
                    "no cached token at {} and no LOGGER_APIKEY in env — run the register \
                     helper (Windows: register-logger.cmd; Linux/macOS: source apikey.env then \
                     start lan-mouse once) to opt in",
                    token_path.display()
                );
                write_status(&format!("disabled ({msg})"));
                return Ok(None);
            }
        },
    };
    let status_line = format!(
        "shipping to {host} as {client_name} (source: {source}, cache: {})",
        token_path.display()
    );
    eprintln!("remote_log: {status_line}");
    write_status(&status_line);

    let (tx, rx) = mpsc::sync_channel::<LogEntry>(QUEUE_CAP);
    let host_clone = host.clone();
    let meta_clone = metadata.clone();
    thread::Builder::new()
        .name("remote-log-shipper".into())
        .spawn(move || shipper_loop(host_clone, token, meta_clone, rx))
        .map_err(|e| format!("spawn shipper: {e}"))?;

    Ok(Some(RemoteHandle { tx }))
}

fn read_api_key() -> Option<String> {
    env::var("CB_LOGGER_API_KEY")
        .ok()
        .or_else(|| env::var("LOGGER_APIKEY").ok())
        .filter(|k| !k.trim().is_empty())
}

fn load_cached_token(token_path: &Path, client_name: &str) -> Option<String> {
    let raw = fs::read_to_string(token_path).ok()?;
    let cached: TokenCache = serde_json::from_str(&raw).ok()?;
    if cached.name == client_name && !cached.token.is_empty() {
        Some(cached.token)
    } else {
        None
    }
}

fn register_new(
    host: &str,
    api_key: &str,
    client_name: &str,
    metadata: &Value,
    token_path: &Path,
) -> Result<String, String> {
    let resp: TokenCache = ureq::post(&format!("{host}/v1/register"))
        .set("X-API-Key", api_key)
        .set("Content-Type", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send_json(json!({
            "name": client_name,
            "metadata": metadata,
        }))
        .map_err(|e| format!("register: {e}"))?
        .into_json::<RegisterResp>()
        .map(|r| TokenCache {
            client_id: r.client_id,
            token: r.token,
            name: client_name.to_string(),
        })
        .map_err(|e| format!("register parse: {e}"))?;
    persist_token(token_path, &resp)?;
    Ok(resp.token)
}

#[derive(Deserialize)]
struct RegisterResp {
    client_id: String,
    token: String,
}

fn persist_token(path: &Path, cache: &TokenCache) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string(cache).map_err(|e| format!("serialize: {e}"))?;
    let mut f = fs::OpenOptions::new();
    f.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        f.mode(0o600);
    }
    let mut file = f.open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(json.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Append a single-line status entry to a stable, predictable file so a
/// user on Windows (whose Explorer-launched lan-mouse.exe loses stderr)
/// can tail the file to see whether the shipper actually started.
fn write_status(line: &str) {
    let Ok(dir) = config_dir() else { return };
    let Ok(()) = fs::create_dir_all(&dir) else { return };
    let path = dir.join(STATUS_FILE);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id();
    let entry = format!("ts={now} pid={pid}  {line}\n");
    // Best-effort: ignore failures so a read-only profile can't break logging.
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(entry.as_bytes());
    }
}

fn config_dir() -> Result<PathBuf, String> {
    let base = if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else if let Ok(appdata) = env::var("APPDATA") {
        PathBuf::from(appdata)
    } else {
        return Err("no XDG_CONFIG_HOME / HOME / APPDATA in env".into());
    };
    Ok(base.join("lan-mouse"))
}

fn token_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join(TOKEN_FILE))
}

fn shipper_loop(host: String, token: String, meta: Value, rx: Receiver<LogEntry>) {
    let endpoint = format!("{host}/v1/logs/bulk");
    let agent = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .build();
    let mut buf: Vec<LogEntry> = Vec::with_capacity(MAX_BATCH);
    let mut last_flush = Instant::now();
    loop {
        // Block until at least one entry arrives or the channel closes.
        let timeout = BATCH_INTERVAL.saturating_sub(last_flush.elapsed());
        match rx.recv_timeout(timeout) {
            Ok(entry) => buf.push(entry),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                flush(&agent, &endpoint, &token, &meta, &mut buf);
                return;
            }
        }
        // Greedily drain anything else available.
        while buf.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(entry) => buf.push(entry),
                Err(_) => break,
            }
        }
        if buf.len() >= MAX_BATCH || last_flush.elapsed() >= BATCH_INTERVAL {
            flush(&agent, &endpoint, &token, &meta, &mut buf);
            last_flush = Instant::now();
        }
    }
}

fn flush(agent: &ureq::Agent, endpoint: &str, token: &str, meta: &Value, buf: &mut Vec<LogEntry>) {
    if buf.is_empty() {
        return;
    }
    let payload: Vec<Value> = buf
        .drain(..)
        .map(|e| {
            let mut m = meta.clone();
            if let Value::Object(map) = &mut m {
                map.insert("target".into(), Value::String(e.target));
                if let Some(module) = e.module {
                    map.insert("module".into(), Value::String(module));
                }
            }
            json!({
                "level": e.level,
                "message": e.message,
                "metadata": m,
            })
        })
        .collect();
    let res = agent
        .post(endpoint)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_json(Value::Array(payload));
    if let Err(e) = res {
        // First failure goes to stderr so the user sees it; afterwards
        // we stay quiet to avoid spamming if the network is down for a
        // long time.
        static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if WARNED.set(()).is_ok() {
            eprintln!("remote_log: bulk upload failed (further errors silenced): {e}");
        }
    }
}

fn level_str(l: Level) -> &'static str {
    match l {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
        Level::Trace => "trace",
    }
}

