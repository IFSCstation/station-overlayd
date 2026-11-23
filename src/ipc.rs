//! Unix-socket IPC (M1, PLAN.md §5).
//! Socket: `$XDG_RUNTIME_DIR/overlayd.sock`
//! Protocol: line-based text, one command per line.
//! Responses: `ok` or `err <message>`.
//!
//! The same binary doubles as a CLI client: `overlayd ctl <command>`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

/// Command received over IPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcMsg {
    ShowMenu,
    ShowHud,
    Hide,
    Toggle,
    /// Fire-and-forget notification text (handled by the notify module, M4).
    Notify(String),
    /// Shut down the daemon (platform-level, not a core command).
    Quit,
}

/// Socket path based on `XDG_RUNTIME_DIR`.
pub fn socket_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map_err(|_| "XDG_RUNTIME_DIR is not set (run under systemd user or export it)")?;
    Ok(runtime_dir.join("overlayd.sock"))
}

/// Bind a UnixListener at the socket path, removing any stale socket first.
pub fn bind_socket() -> Result<UnixListener, Box<dyn std::error::Error>> {
    let path = socket_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    Ok(listener)
}

/// Remove the socket file on shutdown.
pub fn cleanup_socket() -> Result<(), Box<dyn std::error::Error>> {
    let path = socket_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Parse a single line into an `IpcMsg`.
pub fn parse_line(line: &str) -> Result<IpcMsg, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("empty command".into());
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap();
    let arg = parts.next();
    match cmd {
        "show" => match arg {
            Some("menu") => Ok(IpcMsg::ShowMenu),
            Some("hud") => Ok(IpcMsg::ShowHud),
            Some(other) => Err(format!("unknown show subcommand: {other}")),
            None => Err("show requires menu or hud".into()),
        },
        "hide" => Ok(IpcMsg::Hide),
        "toggle" => Ok(IpcMsg::Toggle),
        "notify" => match arg {
            Some(text) if !text.is_empty() => Ok(IpcMsg::Notify(text.to_string())),
            Some(_) => Err("notify requires non-empty text".into()),
            None => Err("notify requires text".into()),
        },
        "quit" => Ok(IpcMsg::Quit),
        _ => Err(format!("unknown command: {cmd}")),
    }
}

/// Handle one client connection: read lines, process each, respond per-line.
pub fn handle_client(
    mut stream: UnixStream,
    app: &mut crate::wl::app::App,
) -> Result<(), Box<dyn std::error::Error>> {
    // Clone the stream for reading so we can write responses on the
    // original while still reading. This avoids deadlock: the client
    // sends a line and waits for a response; we respond immediately.
    let reader_stream = stream.try_clone()?;
    let reader = BufReader::new(reader_stream);
    for line in reader.lines() {
        let line = line?;
        let result = parse_line(&line);
        match result {
            Ok(msg) => {
                app.handle_ipc(msg);
                writeln!(stream, "ok")?;
            }
            Err(e) => writeln!(stream, "err {e}")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_show_menu() {
        assert_eq!(parse_line("show menu"), Ok(IpcMsg::ShowMenu));
    }

    #[test]
    fn parse_show_hud() {
        assert_eq!(parse_line("show hud"), Ok(IpcMsg::ShowHud));
    }

    #[test]
    fn parse_hide() {
        assert_eq!(parse_line("hide"), Ok(IpcMsg::Hide));
    }

    #[test]
    fn parse_toggle() {
        assert_eq!(parse_line("toggle"), Ok(IpcMsg::Toggle));
    }

    #[test]
    fn parse_notify() {
        assert_eq!(
            parse_line("notify hello world"),
            Ok(IpcMsg::Notify("hello world".to_string()))
        );
    }

    #[test]
    fn parse_quit() {
        assert_eq!(parse_line("quit"), Ok(IpcMsg::Quit));
    }

    #[test]
    fn parse_unknown() {
        assert!(parse_line("frobnicate").is_err());
    }

    #[test]
    fn parse_empty() {
        assert!(parse_line("").is_err());
        assert!(parse_line("   ").is_err());
    }
}
