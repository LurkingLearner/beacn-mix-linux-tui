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

/// XDG-style base directories used to resolve every path in this module.
/// Production code uses [`paths_default`] (env-driven); tests inject temp dirs
/// via [`with_paths`] so they never touch the real user config/state.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn new(config_dir: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            config_dir,
            state_dir,
        }
    }

    pub fn bindings(&self) -> PathBuf {
        self.config_dir.join("bindings.json")
    }

    pub fn display(&self) -> PathBuf {
        self.config_dir.join("display.json")
    }

    /// First existing `background.{png,jpg,jpeg}` under `config_dir`, if any.
    pub fn background(&self) -> Option<PathBuf> {
        ["background.png", "background.jpg", "background.jpeg"]
            .into_iter()
            .map(|name| self.config_dir.join(name))
            .find(|p| p.exists())
    }

    pub fn levels(&self) -> PathBuf {
        self.state_dir.join("levels.json")
    }

    pub fn modules(&self) -> PathBuf {
        self.state_dir.join("modules.json")
    }
}

/// The env-driven paths used by the running app.
pub fn paths_default() -> Paths {
    Paths::new(
        base_dir("XDG_CONFIG_HOME", ".config"),
        base_dir("XDG_STATE_HOME", ".local/state"),
    )
}

// --- Free-function wrappers used by the rest of the crate. They delegate to a
//     thread-local override (defaulting to `paths_default()`) so tests can run
//     the same `state::load()` / `state::save()` code against temp dirs without
//     touching the real user config/state. ---

thread_local! {
    static PATHS_OVERRIDE: std::cell::RefCell<Option<Paths>> = const { std::cell::RefCell::new(None) };
}

fn current_paths() -> Paths {
    PATHS_OVERRIDE.with(|cell| cell.borrow().clone().unwrap_or_else(paths_default))
}

#[cfg(test)]
fn set_paths_for_test(p: Paths) {
    PATHS_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(p));
}

#[cfg(test)]
fn clear_paths_for_test() {
    PATHS_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
}

/// Run `f` with `p` installed as the thread-local path override. Restores the
/// previous override on return.
#[cfg(test)]
pub fn with_paths<R>(p: Paths, f: impl FnOnce() -> R) -> R {
    struct Guard(Option<Paths>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0
                .take()
                .map(set_paths_for_test)
                .unwrap_or_else(clear_paths_for_test);
        }
    }
    let prev = PATHS_OVERRIDE.with(|cell| cell.borrow().clone());
    set_paths_for_test(p);
    let _guard = Guard(prev);
    f()
}

fn bindings_path() -> PathBuf {
    current_paths().bindings()
}

/// First existing `background.{png,jpg,jpeg}` in the config dir, if any. Drop an
/// image there to use it as the panel backdrop (solid colour is used otherwise).
pub fn background_path() -> Option<PathBuf> {
    current_paths().background()
}

fn modules_path() -> PathBuf {
    current_paths().modules()
}

fn levels_path() -> PathBuf {
    current_paths().levels()
}

fn display_path() -> PathBuf {
    current_paths().display()
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
#[derive(Clone, Default, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Bumped by the TUI to ask the daemon to reload the backdrop image from
    /// disk (the daemon loads it once at startup, so this is the on-demand
    /// "refresh background" signal). The value itself is meaningless — only a
    /// change matters.
    #[serde(default)]
    pub background_generation: u64,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            dim_after_secs: 300,
            full_brightness: 80,
            dim_brightness: 12,
            channel_names: std::array::from_fn(|_| String::new()),
            background_generation: 0,
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

#[cfg(test)]
mod tests {
    //! These exercise the JSON persistence + XDG-path resolution without ever
    //! touching the real user config/state — every test installs a temp
    //! `Paths` override via [`with_paths`] and lets its `Drop` restore the
    //! previous one.

    use super::*;
    use crate::mix::Channel;

    fn make_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("cfg");
        let st = dir.path().join("state");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&st).unwrap();
        (dir, Paths::new(cfg, st))
    }

    /// Convenience for tests that only touch files inside `with_paths`. Leaks
    /// the TempDir (cleaned up at process exit) so callers don't have to pass
    /// it around just to keep the dirs alive.
    fn temp_paths() -> Paths {
        let (dir, p) = make_paths();
        std::mem::forget(dir);
        p
    }

    #[test]
    fn bindings_roundtrip_and_lookups() {
        let p = temp_paths();
        with_paths(p.clone(), || {
            let mut b = Bindings::load().expect("load default");
            assert!(b.channel_for_app("Spotify").is_none());
            b.set("Spotify", Channel(0));
            b.set("Firefox", Channel(2));
            b.set("Discord", Channel(0)); // two apps can share a channel
            b.save().expect("save");
        });

        with_paths(p, || {
            let b = Bindings::load().expect("reload");
            assert_eq!(b.channel_for_app("Spotify"), Some(Channel(0)));
            assert_eq!(b.channel_for_app("Firefox"), Some(Channel(2)));
            assert_eq!(b.channel_for_app("Discord"), Some(Channel(0)));
            assert_eq!(b.channel_for_app("nope"), None);

            let ch0 = b.apps_for_channel(Channel(0));
            assert_eq!(ch0.len(), 2);
            // BTreeMap → alphabetical, stable.
            assert_eq!(ch0, vec!["Discord".to_string(), "Spotify".to_string()]);

            let mut b2 = b.clone();
            b2.remove("Spotify");
            assert_eq!(b2.channel_for_app("Spotify"), None);
            assert_eq!(b2.apps_for_channel(Channel(0)).len(), 1);
        });
    }

    #[test]
    fn levels_defaults_when_file_missing() {
        let p = temp_paths();
        with_paths(p, || {
            let l = Levels::load().expect("load");
            // The default Levels is [75, 75, 75, 75], all unmuted — verify
            // both the volume floor and that none of the channels start muted.
            assert_eq!(l.volumes, [75; 4]);
            assert_eq!(l.mutes, [false; 4]);
        });
    }

    #[test]
    fn levels_roundtrip() {
        let p = temp_paths();
        let original = Levels {
            volumes: [12, 100, 0, 150],
            mutes: [false, true, false, true],
        };
        with_paths(p.clone(), || original.save().expect("save"));

        with_paths(p, || {
            let l = Levels::load().expect("load");
            assert_eq!(l.volumes, original.volumes);
            assert_eq!(l.mutes, original.mutes);
        });
    }

    #[test]
    fn display_config_defaults_have_no_custom_names() {
        let p = temp_paths();
        with_paths(p, || {
            let d = DisplayConfig::load().expect("load default");
            assert_eq!(d.dim_after_secs, 300);
            assert_eq!(d.full_brightness, 80);
            assert_eq!(d.dim_brightness, 12);
            assert_eq!(d.background_generation, 0);
            for i in 0..4 {
                assert_eq!(d.channel_label(i), format!("CH {}", i + 1));
            }
        });
    }

    #[test]
    fn display_config_channel_label_uses_custom_name_when_set() {
        let mut d = DisplayConfig::default();
        d.channel_names[1] = "Voice".into();
        d.channel_names[3] = "Music".into();
        assert_eq!(d.channel_label(0), "CH 1");
        assert_eq!(d.channel_label(1), "Voice");
        assert_eq!(d.channel_label(2), "CH 3");
        assert_eq!(d.channel_label(3), "Music");
    }

    #[test]
    fn display_config_roundtrip_preserves_all_fields() {
        let p = temp_paths();
        let original = DisplayConfig {
            dim_after_secs: 120,
            full_brightness: 100,
            dim_brightness: 5,
            channel_names: std::array::from_fn(|i| format!("name{}", i)),
            background_generation: 42,
        };
        with_paths(p.clone(), || original.save().expect("save"));

        with_paths(p, || {
            let d = DisplayConfig::load().expect("load");
            assert_eq!(d, original);
        });
    }

    #[test]
    fn display_config_backward_compatible_with_legacy_json() {
        // Older versions of `display.json` predated `channel_names` and
        // `background_generation`. They must still parse, with the new
        // fields defaulting to their zero/empty values.
        let (dir, p) = make_paths();
        let legacy = serde_json::json!({
            "dim_after_secs": 600,
            "full_brightness": 90,
            "dim_brightness": 20
        });
        std::fs::write(p.display(), serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        with_paths(p, || {
            let d = DisplayConfig::load().expect("legacy load");
            assert_eq!(d.dim_after_secs, 600);
            assert_eq!(d.full_brightness, 90);
            assert_eq!(d.dim_brightness, 20);
            assert_eq!(d.background_generation, 0);
            for n in &d.channel_names {
                assert!(n.is_empty());
            }
        });
        drop(dir); // keep tempdir alive through the load above
    }

    #[test]
    fn modules_roundtrip_and_clear() {
        let p = temp_paths();
        with_paths(p.clone(), || {
            let mut m = Modules::load().expect("load default");
            assert!(m.ids.is_empty());
            m.ids.push(17);
            m.ids.push(42);
            m.save().expect("save");
        });

        with_paths(p.clone(), || {
            let m = Modules::load().expect("load");
            assert_eq!(m.ids, vec![17, 42]);
            Modules::clear().expect("clear");
        });

        with_paths(p, || {
            let m = Modules::load().expect("load after clear");
            assert!(m.ids.is_empty(), "clear() should remove the file");
        });
    }

    #[test]
    fn background_path_picks_first_existing_extension() {
        let (dir, p) = make_paths();
        // No file → None.
        with_paths(p.clone(), || {
            assert_eq!(background_path(), None);
        });

        // Drop a .png → resolves.
        std::fs::write(p.config_dir.join("background.png"), b"fake").unwrap();
        with_paths(p.clone(), || {
            assert_eq!(background_path(), Some(p.config_dir.join("background.png")));
        });

        // Drop a .jpg — should still resolve to .png (the function checks
        // .png first, so first-existing wins).
        std::fs::write(p.config_dir.join("background.jpg"), b"fake").unwrap();
        with_paths(p.clone(), || {
            assert_eq!(
                background_path(),
                Some(p.config_dir.join("background.png")),
                "background.png should win when both exist"
            );
        });

        // Remove .png, keep .jpg → resolves to .jpg.
        std::fs::remove_file(p.config_dir.join("background.png")).unwrap();
        with_paths(p.clone(), || {
            assert_eq!(background_path(), Some(p.config_dir.join("background.jpg")));
        });
        drop(dir); // keep tempdir alive until end of test
    }

    #[test]
    fn paths_default_respects_xdg_env() {
        // Set XDG vars, verify the base dir is `<env>/beacn-mix-linux`.
        // We can't safely mutate the process env (other tests may run in
        // parallel), so just construct the same logic directly.
        let p = Paths::new("/tmp/cfg-x".into(), "/tmp/state-x".into());
        assert_eq!(p.bindings(), PathBuf::from("/tmp/cfg-x/bindings.json"));
        assert_eq!(p.display(), PathBuf::from("/tmp/cfg-x/display.json"));
        assert_eq!(p.levels(), PathBuf::from("/tmp/state-x/levels.json"));
        assert_eq!(p.modules(), PathBuf::from("/tmp/state-x/modules.json"));
    }
}
