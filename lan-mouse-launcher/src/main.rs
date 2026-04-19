#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::env;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let exe = match env::current_exe() {
        Ok(p) => p,
        Err(_) => return ExitCode::FAILURE,
    };
    let dir = match exe.parent() {
        Some(d) => d,
        None => return ExitCode::FAILURE,
    };

    let target_name = if cfg!(windows) {
        "lan-mouse.exe"
    } else {
        "lan-mouse"
    };
    let target = dir.join("bin").join(target_name);

    match Command::new(&target).args(env::args_os().skip(1)).spawn() {
        Ok(_) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
