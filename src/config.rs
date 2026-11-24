//! Configuration file support (M5, PLAN.md §5).
//!
//! Config file: `$XDG_CONFIG_HOME/overlayd/config.toml` (or
//! `~/.config/overlayd/config.toml`).
//! Format: TOML.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Full daemon configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Overlay panel size in logical pixels.
    #[serde(default)]
    pub panel: PanelConfig,
    /// Summon combo settings.
    #[serde(default)]
    pub summon: SummonConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PanelConfig {
    /// Panel width in logical pixels.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Panel height in logical pixels.
    #[serde(default = "default_height")]
    pub height: u32,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
        }
    }
}

fn default_width() -> u32 {
    640
}

fn default_height() -> u32 {
    360
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SummonConfig {
    /// Summon combo: button codes that must be held simultaneously.
    /// Default: [306, 315] = [BTN_SELECT, BTN_START].
    #[serde(default = "default_summon_buttons")]
    pub buttons: Vec<u16>,
}

impl Default for SummonConfig {
    fn default() -> Self {
        Self {
            buttons: default_summon_buttons(),
        }
    }
}

fn default_summon_buttons() -> Vec<u16> {
    vec![0x132, 0x133] // BTN_SELECT, BTN_START
}

/// Load configuration from disk. Returns defaults if no config file exists.
pub fn load() -> Result<Config, Box<dyn std::error::Error>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

/// Save configuration to disk (creating directories as needed).
#[allow(dead_code)]
pub fn save(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string(config)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Return the config file path based on XDG_CONFIG_HOME.
pub fn config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_next::home_dir()
                .map(|h| h.join(".config"))
                .unwrap_or_else(|| PathBuf::from("/etc/xdg"))
        });
    Ok(config_home.join("overlayd").join("config.toml"))
}

impl Default for Config {
    fn default() -> Self {
        Self {
            panel: PanelConfig {
                width: default_width(),
                height: default_height(),
            },
            summon: SummonConfig {
                buttons: default_summon_buttons(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let c = Config::default();
        assert_eq!(c.panel.width, 640);
        assert_eq!(c.panel.height, 360);
        assert_eq!(c.summon.buttons, vec![0x132, 0x133]);
    }

    #[test]
    fn parse_config() {
        let toml_str = r#"
[panel]
width = 800
height = 450

[summon]
buttons = [306, 315]
"#;
        let c: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(c.panel.width, 800);
        assert_eq!(c.panel.height, 450);
        assert_eq!(c.summon.buttons, vec![306, 315]);
    }
}
