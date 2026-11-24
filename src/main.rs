//! `overlayd` — lightweight Wayland overlay daemon (PLAN.md).
//!
//! M5: Config file at `$XDG_CONFIG_HOME/overlayd/config.toml`,
//!     systemd user unit at `packaging/overlayd@.service`,
//!     HUD mode via `overlayd ctl show hud`.
//!
//! Usage:
//!   overlayd              # run the daemon
//!   overlayd ctl toggle   # CLI client: send a command and exit

mod config;
mod core;
mod ipc;
mod modules;
mod pad;
mod wl;

slint::include_modules!();

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "ctl" {
        return ctl(&args[2..]);
    }
    wl::app::run()
}

/// CLI client: connect to the daemon's Unix socket, send one command,
/// print the response, exit.
fn ctl(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        eprintln!("usage: overlayd ctl <command>");
        eprintln!("  commands: show menu | show hud | hide | toggle | notify <text> | quit");
        std::process::exit(2);
    }
    let path = ipc::socket_path()?;
    let stream = UnixStream::connect(&path)?;
    // Send the command line.
    {
        let mut writer = stream.try_clone()?;
        writeln!(writer, "{}", args.join(" "))?;
        writer.flush()?;
    }
    // Read the response line.
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    print!("{response}");
    Ok(())
}
