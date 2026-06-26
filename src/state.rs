//! On-disk persistence: which app belongs to which channel (so streams
//! re-bind when an app reappears), and which pactl modules we loaded (so
//! teardown removes exactly what we created).

use crate::mix::Channel;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn base_dir(env: &str, fallback: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(env) {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("beacn-mix-linux");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(fallback).join("beacn-mix-linux")
}

fn bindings_path() -> PathBuf {
    base_dir("XDG_CONFIG_HOME", ".config").join("bindings.json")
}

/// First existing `background.{png,jpg,jpeg}` in the config dir, if any. Drop an
/// image there to use it as the panel backdrop (solid colour is used otherwise).
pub fn background_path() -> Option<PathBuf> {
    let dir = base_dir("XDG_CONFIG_HOME", ".config");
    ["background.png", "background.jpg", "background.jpeg"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|p| p.exists())
}

fn modules_path() -> PathBuf {
    base_dir("XDG_STATE_HOME", ".local/state").join("modules.json")
}

fn levels_path() -> PathBuf {
    base_dir("XDG_STATE_HOME", ".local/state").join("levels.json")
}

fn display_path() -> PathBuf {
    base_dir("XDG_CONFIG_HOME", ".config").join("display.json")
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &PathBuf) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn save_json<T: Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(value)?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// app name -> channel index (0..=3).
#[derive(Default, Serialize, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    pub by_app: BTreeMap<String, usize>,
}

impl Bindings {
    pub fn load() -> Result<Self> {
        load_json(&bindings_path())
    }

    pub fn save(&self) -> Result<()> {
        save_json(&bindings_path(), self)
    }

    pub fn set(&mut self, app: &str, ch: Channel) {
        self.by_app.insert(app.to_owned(), ch.0);
    }

    pub fn channel_for_app(&self, app: &str) -> Option<Channel> {
        self.by_app.get(app).copied().map(Channel)
    }

    /// Drop an app's binding (so it stops auto-routing).
    pub fn remove(&mut self, app: &str) {
        self.by_app.remove(app);
    }

    /// All apps currently bound to a channel, in stable (alphabetical) order.
    pub fn apps_for_channel(&self, ch: Channel) -> Vec<String> {
        self.by_app
            .iter()
            .filter(|(_, &c)| c == ch.0)
            .map(|(app, _)| app.clone())
            .collect()
    }
}

/// Per-channel volume (%) and mute state, persisted across restarts.
#[derive(Serialize, Deserialize)]
pub struct Levels {
    pub volumes: [u32; 4],
    pub mutes: [bool; 4],
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            volumes: [75; 4],
            mutes: [false; 4],
        }
    }
}

impl Levels {
    pub fn load() -> Result<Self> {
        load_json(&levels_path())
    }

    pub fn save(&self) -> Result<()> {
        save_json(&levels_path(), self)
    }
}

/// Panel display behaviour, edited from the TUI Settings page and re-read live by
/// the `run` daemon. Brightness values are percentages (1..=100); the device
/// rejects 0, so the dim level is a low-but-visible value, not off.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Idle time before the panel dims, in seconds.
    pub dim_after_secs: u64,
    /// Brightness while active.
    pub full_brightness: u8,
    /// Brightness once dimmed.
    pub dim_brightness: u8,
    /// Optional custom label per channel; empty falls back to "CH n".
    #[serde(default)]
    pub channel_names: [String; 4],
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            dim_after_secs: 300,
            full_brightness: 80,
            dim_brightness: 12,
            channel_names: std::array::from_fn(|_| String::new()),
        }
    }
}

impl DisplayConfig {
    pub fn load() -> Result<Self> {
        load_json(&display_path())
    }

    pub fn save(&self) -> Result<()> {
        save_json(&display_path(), self)
    }

    /// Display label for a channel: the custom name, or "CH n" when unset.
    pub fn channel_label(&self, i: usize) -> String {
        let name = &self.channel_names[i];
        if name.is_empty() {
            format!("CH {}", i + 1)
        } else {
            name.clone()
        }
    }
}

/// pactl module IDs we loaded, persisted so `teardown` can unload them.
#[derive(Default, Serialize, Deserialize)]
pub struct Modules {
    #[serde(default)]
    pub ids: Vec<u32>,
}

impl Modules {
    pub fn load() -> Result<Self> {
        load_json(&modules_path())
    }

    pub fn save(&self) -> Result<()> {
        save_json(&modules_path(), self)
    }

    pub fn clear() -> Result<()> {
        let path = modules_path();
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }
}
