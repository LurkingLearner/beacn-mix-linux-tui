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

fn modules_path() -> PathBuf {
    base_dir("XDG_STATE_HOME", ".local/state").join("modules.json")
}

fn levels_path() -> PathBuf {
    base_dir("XDG_STATE_HOME", ".local/state").join("levels.json")
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
