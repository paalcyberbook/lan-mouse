//! `lan-mouse cli logger ...` — manage the cb-logger client this host is
//! registered as. Reads the token cache that `lan_mouse::remote_log`
//! wrote at daemon startup; talks straight to the logger HTTP API (no
//! IPC, no running daemon needed).

use std::{
    env,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::json;

const DEFAULT_HOST: &str = "https://logger.cyberbook.id";
const TOKEN_FILE: &str = "remote-log-token.json";
const GROUP_FILE: &str = "remote-log-group.json";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct TokenCache {
    client_id: String,
    token: String,
    name: String,
}

#[derive(Deserialize, Serialize)]
struct GroupCache {
    id: String,
    name: String,
    owner_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invite_code: Option<String>,
}

pub fn create(name: &str) -> Result<(), String> {
    let tok = load_token()?;
    let resp: GroupCache = http_post(&tok.token, "/v1/groups", json!({ "name": name }))?;
    let path = config_dir()?.join(GROUP_FILE);
    persist(&path, &resp).map_err(|e| format!("persist group: {e}"))?;
    println!("created group \"{}\" ({})", resp.name, resp.id);
    if let Some(code) = resp.invite_code.as_deref() {
        println!("invite_code: {code}");
        println!(
            "share with the other hosts and have them run:\n  \
             lan-mouse cli logger join {code}"
        );
    }
    println!("saved to {}", path.display());
    Ok(())
}

pub fn join(invite_code: &str) -> Result<(), String> {
    let tok = load_token()?;
    let resp: GroupCache = http_post(&tok.token, "/v1/groups/join", json!({ "invite_code": invite_code }))?;
    let path = config_dir()?.join(GROUP_FILE);
    persist(&path, &resp).map_err(|e| format!("persist group: {e}"))?;
    println!("joined group \"{}\" ({})", resp.name, resp.id);
    println!("saved to {}", path.display());
    Ok(())
}

pub fn groups() -> Result<(), String> {
    let tok = load_token()?;
    let resp: serde_json::Value = http_get(&tok.token, "/v1/groups")?;
    let arr = resp.as_array().ok_or_else(|| "unexpected response shape".to_string())?;
    if arr.is_empty() {
        println!("(no groups; create one with `lan-mouse cli logger create <name>` or join via `... join <code>`)");
        return Ok(());
    }
    for g in arr {
        let id = g.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let n = g.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let owner = g.get("owner_id").and_then(|v| v.as_str()).unwrap_or("?");
        let mine = owner == tok.client_id;
        let suffix = if mine { " (owner)" } else { "" };
        println!("{id}  {n}{suffix}");
        if mine {
            if let Some(code) = g.get("invite_code").and_then(|v| v.as_str()) {
                println!("    invite_code: {code}");
            }
        }
    }
    Ok(())
}

pub fn versions() -> Result<(), String> {
    let tok = load_token()?;
    let resp: serde_json::Value = http_get(&tok.token, "/v1/client/metadata-versions")?;
    let arr = resp
        .as_array()
        .ok_or_else(|| "unexpected response shape".to_string())?;
    if arr.is_empty() {
        println!("(no versions — start lan-mouse at least once to populate)");
        return Ok(());
    }
    for v in arr {
        let n = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
        let ts = v.get("created_at").and_then(|x| x.as_str()).unwrap_or("?");
        let m = v
            .get("metadata")
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .unwrap_or_default();
        println!("v{n:<3}  {ts}  {m}");
    }
    Ok(())
}

pub fn tail(limit: u32, level: Option<&str>, mine_only: bool) -> Result<(), String> {
    let tok = load_token()?;
    let group_id = if mine_only {
        None
    } else {
        let group_path = config_dir()?.join(GROUP_FILE);
        fs::read_to_string(&group_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<GroupCache>(&raw).ok())
            .map(|g| g.id)
    };
    let limit = limit.clamp(1, 10000);
    // Always ask for inline metadata snapshots so we can render
    // `[os host]` regardless of which `metadata_version` produced the
    // row. The cost is a single JOIN per query on the server side.
    let mut path = match &group_id {
        Some(id) => format!("/v1/groups/{id}/logs?limit={limit}&with_metadata=1"),
        None => format!("/v1/logs?limit={limit}&with_metadata=1"),
    };
    if let Some(lvl) = level {
        path.push_str(&format!("&level={}", urlencode(lvl)));
    }
    let resp: serde_json::Value = http_get(&tok.token, &path)?;
    let arr = resp
        .as_array()
        .ok_or_else(|| "unexpected response shape".to_string())?;
    // Server returns newest-first; flip so a terminal scroll shows oldest at top.
    for entry in arr.iter().rev() {
        print_entry(entry);
    }
    eprintln!(
        "--- {} entries, {} ---",
        arr.len(),
        group_id
            .as_deref()
            .map(|s| format!("group {s}"))
            .unwrap_or_else(|| "own logs".to_string())
    );
    Ok(())
}

fn print_entry(entry: &serde_json::Value) {
    let time = entry.get("time").and_then(|v| v.as_str()).unwrap_or("--");
    let level = entry.get("level").and_then(|v| v.as_str()).unwrap_or("?");
    let msg = entry.get("message").and_then(|v| v.as_str()).unwrap_or("");
    // Prefer the metadata_snapshot (resolved client metadata at ingest
    // time, via ?with_metadata=1), then fall back to the per-entry
    // metadata — for rows from older client builds that still embedded
    // os/hostname in the entry payload itself.
    let snap = entry
        .get("metadata_snapshot")
        .or_else(|| entry.get("metadata"));
    let host = snap
        .and_then(|m| m.get("hostname"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let os = snap
        .and_then(|m| m.get("os"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let ver = entry
        .get("metadata_version")
        .and_then(|v| v.as_u64())
        .map(|v| format!("v{v}"))
        .unwrap_or_else(|| "--".to_string());
    println!("{time}  [{os:7} {host:20} {ver:4}] {level:5} {msg}");
}

fn urlencode(s: &str) -> String {
    // Tiny URL-encoder for query param values. Only the unreserved set is
    // passed through verbatim; everything else becomes %HH. The values we
    // pass (log levels, mostly) are ASCII alphanumeric so this is
    // overkill, but it's correct and dependency-free.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn http_post<T: for<'de> Deserialize<'de>>(token: &str, path: &str, body: serde_json::Value) -> Result<T, String> {
    let host = env::var("CB_LOGGER_HOST").unwrap_or_else(|_| DEFAULT_HOST.into());
    ureq::post(&format!("{host}{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send_json(body)
        .map_err(|e| format!("POST {path}: {e}"))?
        .into_json::<T>()
        .map_err(|e| format!("parse {path}: {e}"))
}

fn http_get<T: for<'de> Deserialize<'de>>(token: &str, path: &str) -> Result<T, String> {
    let host = env::var("CB_LOGGER_HOST").unwrap_or_else(|_| DEFAULT_HOST.into());
    ureq::get(&format!("{host}{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(HTTP_TIMEOUT)
        .call()
        .map_err(|e| format!("GET {path}: {e}"))?
        .into_json::<T>()
        .map_err(|e| format!("parse {path}: {e}"))
}

pub fn status() -> Result<(), String> {
    let tok = load_token()?;
    println!("client name: {}", tok.name);
    println!("client_id:   {}", tok.client_id);
    let group_path = config_dir()?.join(GROUP_FILE);
    match fs::read_to_string(&group_path) {
        Ok(raw) => match serde_json::from_str::<GroupCache>(&raw) {
            Ok(g) => {
                println!("group:       {} ({})", g.name, g.id);
                if let Some(code) = g.invite_code.as_deref() {
                    println!("invite_code: {code}  (you own this group — share with other hosts)");
                }
            }
            Err(e) => eprintln!("group cache present but unreadable: {e}"),
        },
        Err(_) => println!("group:       (not joined; run `lan-mouse cli logger join <code>`)"),
    }

    // Shipper state: the daemon's remote_log::init writes a line to this
    // file on every startup. Tail the last entry so the user knows
    // whether the most recent lan-mouse process is actually shipping.
    let status_path = config_dir()?.join("remote-log-status.txt");
    match fs::read_to_string(&status_path) {
        Ok(raw) => match raw.lines().rev().find(|l| !l.trim().is_empty()) {
            Some(last) => println!("shipper:     {last}"),
            None => println!("shipper:     (status file empty — start lan-mouse to populate)"),
        },
        Err(_) => println!(
            "shipper:     (no status yet — start lan-mouse once; status written to {})",
            status_path.display()
        ),
    }
    Ok(())
}

fn load_token() -> Result<TokenCache, String> {
    let path = config_dir()?.join(TOKEN_FILE);
    let raw = fs::read_to_string(&path).map_err(|e| {
        format!(
            "no token cache at {} ({e}). Start the daemon once with LOGGER_APIKEY set so it can \
             register.",
            path.display()
        )
    })?;
    serde_json::from_str::<TokenCache>(&raw)
        .map_err(|e| format!("parse {}: {e}", path.display()))
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

fn persist<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string(value).map_err(|e| format!("serialize: {e}"))?;
    let mut opts = fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(json.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}
