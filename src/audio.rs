//! Best-effort OS volume control via `wpctl` (PipeWire).
//!
//! Finds the default audio sink from `wpctl status` and sets its volume.
//! Every operation is best-effort: failures are logged and ignored so a
//! system without PipeWire/wireplumber keeps the overlay working.

use std::process::Command;

/// Volume level 0..=100.
pub type Volume = i32;

/// Run `wpctl get-volume <id>` and return the parsed 0..=100 level, or None.
fn get_volume_for_sink(id: u32) -> Option<Volume> {
    let out = Command::new("wpctl").arg("get-volume").arg(id.to_string()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?;
    // Expected format: "Volume: 0.50" or "Volume: 0.50 [muted]"
    let value = line.split_whitespace().nth(1)?.parse::<f32>().ok()?;
    Some((value * 100.0).round() as Volume)
}

/// Return the default sink's node id by parsing `wpctl status`.
fn default_sink_id() -> Option<u32> {
    let out = Command::new("wpctl").arg("status").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut in_audio = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "Audio" {
            in_audio = true;
            continue;
        }
        if in_audio && trimmed.starts_with("└─") {
            // past the audio section
            break;
        }
        if in_audio && trimmed.starts_with("│  *") {
            // "│  *   53. Starship/... [vol: 0.15]" → sink id is 53
            let rest = trimmed.trim_start_matches("│  *").trim();
            let id: u32 = rest.split('.').next()?.trim().parse().ok()?;
            return Some(id);
        }
    }
    None
}

/// Current OS volume level of the default sink, 0..=100. None on failure.
pub fn get_volume() -> Option<Volume> {
    let id = default_sink_id()?;
    get_volume_for_sink(id)
}

/// Set the default sink volume to `level` (0..=100). Best-effort.
pub fn set_volume(level: Volume) {
    let level = level.clamp(0, 100);
    let id = match default_sink_id() {
        Some(id) => id,
        None => {
            eprintln!("overlayd: audio: no default sink found");
            return;
        }
    };
    let frac = level as f32 / 100.0;
    let status = Command::new("wpctl")
        .arg("set-volume")
        .arg(id.to_string())
        .arg(format!("{frac:.2}"))
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("overlayd: audio: wpctl set-volume exited {s:?}"),
        Err(e) => eprintln!("overlayd: audio: wpctl set-volume error: {e}"),
    }
}
