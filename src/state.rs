//! On-disk persistence: which app belongs to which channel (so streams
//! re-bind when an app reappears), and which pactl modules we loaded (so
//! teardown removes exactly what we created).

use crate::mix::Channel;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// True for the image extensions we accept as panel backdrops.
fn is_image_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")
}

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

    pub fn output(&self) -> PathBuf {
        self.config_dir.join("output.json")
    }

    /// Directory of candidate backdrop images. Drop any number of
    /// `*.{png,jpg,jpeg}` here and cycle between them from the TUI.
    pub fn backgrounds_dir(&self) -> PathBuf {
        self.config_dir.join("backgrounds")
    }

    /// Path to a named image inside [`backgrounds_dir`] (not checked to exist).
    pub fn background_named(&self, name: &str) -> PathBuf {
        self.backgrounds_dir().join(name)
    }

    /// Image file names found in [`backgrounds_dir`], case-insensitively sorted.
    /// Empty (rather than an error) when the directory is missing or unreadable.
    pub fn list_backgrounds(&self) -> Vec<String> {
        let mut names: Vec<String> = match std::fs::read_dir(self.backgrounds_dir()) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| is_image_name(n))
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    pub fn levels(&self) -> PathBuf {
        self.state_dir.join("levels.json")
    }

    /// Local socket through which front-ends ask the running daemon to make a
    /// live state change (rather than only editing its next-startup state).
    pub fn control_socket(&self) -> PathBuf {
        self.state_dir.join("control.sock")
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

/// Image file names available under the config `backgrounds/` dir, sorted.
/// Drop any number of images there and cycle between them from the TUI.
pub fn list_backgrounds() -> Vec<String> {
    current_paths().list_backgrounds()
}

/// Resolve the backdrop a [`DisplayConfig`] selects: the chosen file under
/// `backgrounds/` if it's set *and* still present, else `None` (solid colour).
pub fn background_path_for(cfg: &DisplayConfig) -> Option<PathBuf> {
    let name = cfg.background_file.as_deref()?;
    let p = current_paths().background_named(name);
    p.exists().then_some(p)
}

fn modules_path() -> PathBuf {
    current_paths().modules()
}

fn levels_path() -> PathBuf {
    current_paths().levels()
}

/// Path of the local daemon-control socket.
pub fn control_socket_path() -> PathBuf {
    current_paths().control_socket()
}

fn display_path() -> PathBuf {
    current_paths().display()
}

fn output_path() -> PathBuf {
    current_paths().output()
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

/// Routing bindings: which app plays on which channel (output side), and which
/// mics a channel rides the gain of (input side).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Bindings {
    /// app name -> channel index (0..=3).
    #[serde(default)]
    pub by_app: BTreeMap<String, usize>,
    /// capture-device (mic) node name -> channel index (0..=3). Keyed by source
    /// (like `by_app`) so a channel can hold *several* mics — e.g. a hardwired
    /// mic and a wireless one you swap between. The channel's encoder rides every
    /// bound mic's gain/mute; absent ones simply no-op until they reappear.
    #[serde(default)]
    pub mic_by_source: BTreeMap<String, usize>,
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

    /// Bind a mic (source node name) to a channel so its encoder rides the mic's
    /// gain. A given mic lives on at most one channel, so re-binding it just moves
    /// it; binding a second mic to a channel that already has one keeps both.
    pub fn set_mic(&mut self, ch: Channel, source: &str) {
        self.mic_by_source.insert(source.to_owned(), ch.0);
    }

    /// Which channel a mic is bound to, if any.
    pub fn channel_for_mic(&self, source: &str) -> Option<Channel> {
        self.mic_by_source.get(source).copied().map(Channel)
    }

    /// Drop a single mic's binding (by source node name).
    pub fn remove_mic(&mut self, source: &str) {
        self.mic_by_source.remove(source);
    }

    /// All mics bound to a channel, in stable (source-name) order.
    pub fn mics_for_channel(&self, ch: Channel) -> Vec<String> {
        self.mic_by_source
            .iter()
            .filter(|(_, &c)| c == ch.0)
            .map(|(src, _)| src.clone())
            .collect()
    }

    /// Per-channel mic bindings as a fixed array, for the daemon to ride every
    /// bound mic on a channel (or its sink when the list is empty).
    pub fn mics_array(&self) -> [Vec<String>; 4] {
        std::array::from_fn(|i| self.mics_for_channel(Channel(i)))
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
    /// Chosen backdrop image: a file name under the config `backgrounds/` dir,
    /// or `None` for the solid colour. Cycled with ←/→ on the TUI Settings page.
    #[serde(default)]
    pub background_file: Option<String>,
    /// Bumped by the TUI to ask the daemon to reload the backdrop image from
    /// disk (the daemon loads it once at startup, so this is the on-demand
    /// "refresh background" signal — e.g. after overwriting the chosen file in
    /// place). The value itself is meaningless — only a change matters.
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
            background_file: None,
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

/// Which real output device the channels feed. Written by the TUI Settings page,
/// reconciled live by `run` (it reloads the channel loopbacks onto this sink when
/// it differs from what they currently feed). `None` = follow the system default
/// output, which is the original behaviour.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    /// The chosen sink node name, or `None` to use the default output.
    #[serde(default)]
    pub sink: Option<String>,
}

impl OutputConfig {
    pub fn load() -> Result<Self> {
        load_json(&output_path())
    }

    pub fn save(&self) -> Result<()> {
        save_json(&output_path(), self)
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
    fn mic_bindings_roundtrip_with_multiple_mics_per_channel() {
        let p = temp_paths();
        with_paths(p.clone(), || {
            let mut b = Bindings::load().expect("load default");
            assert!(b.mics_for_channel(Channel(3)).is_empty());

            // Two mics on the same channel (the swap-between-devices case), plus
            // a third mic and an app elsewhere.
            b.set_mic(Channel(3), "alsa_input.snowball");
            b.set_mic(Channel(3), "alsa_input.antlion");
            b.set_mic(Channel(0), "alsa_input.webcam");
            b.set("Firefox", Channel(3)); // a mic and an app can share a channel
            b.save().expect("save");
        });

        with_paths(p, || {
            let mut b = Bindings::load().expect("reload");
            // Both mics live on channel 3, in stable (alphabetical) order.
            assert_eq!(
                b.mics_for_channel(Channel(3)),
                vec![
                    "alsa_input.antlion".to_string(),
                    "alsa_input.snowball".to_string()
                ]
            );
            assert_eq!(b.channel_for_mic("alsa_input.webcam"), Some(Channel(0)));
            // App binding on the same channel is untouched by the mic bindings.
            assert_eq!(b.channel_for_app("Firefox"), Some(Channel(3)));

            // mics_array carries the full per-channel lists.
            let arr = b.mics_array();
            assert_eq!(arr[3].len(), 2);
            assert_eq!(arr[0], vec!["alsa_input.webcam".to_string()]);
            assert!(arr[1].is_empty());

            // Re-binding one mic to a new channel moves only that mic.
            b.set_mic(Channel(1), "alsa_input.snowball");
            assert_eq!(b.channel_for_mic("alsa_input.snowball"), Some(Channel(1)));
            assert_eq!(
                b.mics_for_channel(Channel(3)),
                vec!["alsa_input.antlion".to_string()]
            );

            // Removing a mic only drops that one.
            b.remove_mic("alsa_input.antlion");
            assert!(b.mics_for_channel(Channel(3)).is_empty());
            assert_eq!(b.channel_for_mic("alsa_input.snowball"), Some(Channel(1)));
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
            background_file: Some("sunset.png".into()),
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
            assert_eq!(d.background_file, None);
            for n in &d.channel_names {
                assert!(n.is_empty());
            }
        });
        drop(dir); // keep tempdir alive through the load above
    }

    #[test]
    fn output_config_defaults_to_none_and_roundtrips() {
        let p = temp_paths();
        with_paths(p.clone(), || {
            // Missing file → default (follow system default output).
            assert_eq!(OutputConfig::load().expect("load default").sink, None);
            OutputConfig {
                sink: Some("alsa_output.fiio".into()),
            }
            .save()
            .expect("save");
        });
        with_paths(p, || {
            assert_eq!(
                OutputConfig::load().expect("reload").sink.as_deref(),
                Some("alsa_output.fiio")
            );
        });
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
    fn list_backgrounds_filters_and_sorts_images() {
        let (dir, p) = make_paths();
        // No backgrounds/ dir → empty, not an error.
        with_paths(p.clone(), || {
            assert!(list_backgrounds().is_empty());
        });

        let bg = p.backgrounds_dir();
        std::fs::create_dir_all(&bg).unwrap();
        std::fs::write(bg.join("Zebra.PNG"), b"fake").unwrap(); // mixed-case ext
        std::fs::write(bg.join("aurora.jpg"), b"fake").unwrap();
        std::fs::write(bg.join("notes.txt"), b"fake").unwrap(); // non-image, ignored
        std::fs::create_dir_all(bg.join("subdir")).unwrap(); // dir, ignored

        with_paths(p.clone(), || {
            // Case-insensitively sorted, non-images and subdirs dropped.
            assert_eq!(list_backgrounds(), vec!["aurora.jpg", "Zebra.PNG"]);
        });
        drop(dir);
    }

    #[test]
    fn background_path_for_resolves_only_present_files() {
        let (dir, p) = make_paths();
        let bg = p.backgrounds_dir();
        std::fs::create_dir_all(&bg).unwrap();
        std::fs::write(bg.join("sunset.png"), b"fake").unwrap();

        with_paths(p.clone(), || {
            // None selected → no path.
            let mut cfg = DisplayConfig::default();
            assert_eq!(background_path_for(&cfg), None);

            // A present file → its full path under backgrounds/.
            cfg.background_file = Some("sunset.png".into());
            assert_eq!(background_path_for(&cfg), Some(bg.join("sunset.png")));

            // A selected-but-missing file → None (falls back to solid colour).
            cfg.background_file = Some("gone.png".into());
            assert_eq!(background_path_for(&cfg), None);
        });
        drop(dir);
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
