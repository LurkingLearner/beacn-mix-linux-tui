//! beacn-mix: drive a Beacn Mix as a PipeWire channel mixer.
//!
//! The Mix is a vendor-specific USB control surface (not a sound card): it
//! only emits encoder deltas + button presses. So the four "channels" live on
//! the host as PipeWire null-sinks, and we map the hardware encoders onto their
//! volumes. See README / plan for the full picture.

mod mix;
mod pw;
mod screen;
mod state;
mod tui;

use anyhow::{Context, Result};
use beacn_lib::controller::{ButtonState, Interactions};
use beacn_lib::crossbeam::channel::tick;
use beacn_lib::crossbeam::select;
use clap::{Parser, Subcommand};
use image::RgbImage;
use mix::{channel_for_button, channel_for_dial, Channel, Mix};
use screen::ChannelView;
use state::{Bindings, DisplayConfig, Levels, Modules};
use std::collections::HashSet;
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// How much each encoder tick moves a channel's volume, in percent.
const VOLUME_STEP: i32 = 2;
const VOLUME_MAX: i32 = 150;
/// How often to re-assert brightness + ping the device so the firmware keeps the
/// panel lit (dim or full) and beacn-lib's own dimmer never fires.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(30);
/// A ticker gap longer than this means the host was suspended; on the next tick
/// we re-wake the panel. Kept small so brief sleeps (a few seconds) still
/// trigger the wake-up — the previous 30 s missed those entirely.
const RESUME_GAP: Duration = Duration::from_secs(5);
/// How long to wait between attempts to re-open the USB device after it
/// disappears (disconnect, host resume, etc.). The kernel can take a beat to
/// re-enumerate after a USB bus reset, so a single failure is normal.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Panel brightness state we drive ourselves (beacn-lib's auto-dim is suppressed).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Active,
    Dimmed,
}

#[derive(Parser)]
#[command(
    name = "beacn-mix",
    about = "Use a Beacn Mix as a PipeWire channel mixer"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the mixer: encoders ride channel volume, encoder press toggles mute.
    Run,
    /// Create the four channel sinks (idempotent).
    Setup,
    /// Remove the channel sinks we created.
    Teardown,
    /// Interactively bind a playing app to one of the four channels.
    Assign,
    /// Terminal UI to view and manage channel routing (runs alongside the daemon).
    Tui,
    /// Print raw hardware events (dial/button) — a hardware sanity check.
    Events,
    /// Render a sample panel image to a file (no hardware needed).
    Preview {
        #[arg(default_value = "/tmp/beacn-mix-preview.jpg")]
        path: String,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => cmd_run(),
        Command::Setup => cmd_setup().map(|n| log::info!("Channels ready ({n} new modules).")),
        Command::Teardown => cmd_teardown(),
        Command::Assign => cmd_assign(),
        Command::Tui => tui::run(),
        Command::Events => cmd_events(),
        Command::Preview { path } => cmd_preview(&path),
    }
}

fn cmd_preview(path: &str) -> Result<()> {
    let views: [ChannelView; 4] = [
        ChannelView {
            label: "Media".into(),
            volume: 100,
            muted: false,
            apps: vec!["Firefox".into(), "YouTube Music".into()],
            is_mic: false,
        },
        ChannelView {
            label: "Chat".into(),
            volume: 90,
            muted: true,
            apps: vec!["Discord".into(), "Zoom".into()],
            is_mic: false,
        },
        ChannelView {
            label: "Music".into(),
            volume: 52,
            muted: false,
            apps: vec!["Spotify".into()],
            is_mic: false,
        },
        ChannelView {
            label: "Mic".into(),
            volume: 75,
            muted: false,
            apps: vec!["Yeti Microphone".into()],
            is_mic: true,
        },
    ];
    let background = state::background_path().and_then(|p| screen::load_background(&p));
    let jpeg = screen::render(&views, background.as_ref())?;
    std::fs::write(path, &jpeg).with_context(|| format!("writing {path}"))?;
    println!("Wrote sample panel ({} bytes) to {path}", jpeg.len());
    Ok(())
}

/// Ensure the four channel sinks exist; persist any modules we loaded.
fn cmd_setup() -> Result<usize> {
    let output = pw::default_sink().context("getting default output sink")?;
    let new_ids = pw::ensure_channels(&output)?;
    if !new_ids.is_empty() {
        let mut modules = Modules::load()?;
        modules.ids.extend(&new_ids);
        modules.save()?;
    }
    Ok(new_ids.len())
}

fn cmd_teardown() -> Result<()> {
    let modules = Modules::load()?;
    for id in &modules.ids {
        if let Err(e) = pw::unload_module(*id) {
            log::warn!("could not unload module {id}: {e}");
        }
    }
    Modules::clear()?;
    log::info!("Removed {} channel module(s).", modules.ids.len());
    Ok(())
}

fn cmd_events() -> Result<()> {
    let mix = Mix::open()?;
    println!("Listening for Beacn Mix events (Ctrl-C to stop). Turn knobs / press encoders...");
    for event in mix.events.iter() {
        match event {
            Interactions::DialChanged(dial, delta) => {
                println!(
                    "dial {dial:?} (channel {}) delta {delta:+}",
                    channel_for_dial(dial).human()
                );
            }
            Interactions::ButtonPress(button, st) => {
                let ch = channel_for_button(button).map(|c| c.human());
                println!("button {button:?} {st:?} (channel {ch:?})");
            }
        }
    }
    Ok(())
}

fn cmd_assign() -> Result<()> {
    cmd_setup()?;
    let streams = pw::app_streams()?;
    if streams.is_empty() {
        println!("No playing audio streams found. Start playback in an app and try again.");
        return Ok(());
    }

    println!("Playing streams:");
    for (i, s) in streams.iter().enumerate() {
        println!("  [{i}] {}", s.label());
    }
    let pick = prompt_index("Stream number", streams.len())?;
    let stream = &streams[pick];

    let ch = Channel(prompt_index("Channel (1-4)", 4)?);
    pw::move_stream(stream.index, ch)?;

    let mut bindings = Bindings::load()?;
    bindings.set(&stream.app, ch);
    bindings.save()?;

    println!("Bound '{}' to channel {}.", stream.app, ch.human());
    Ok(())
}

fn cmd_run() -> Result<()> {
    cmd_setup()?;

    // Per-channel mic bindings: when a channel has one, its encoder rides that
    // capture device's gain/mute instead of the output sink. Re-read live below.
    let mut mics = mic_bindings();

    // Restore the channel levels/mutes from the last session, applying each to
    // whatever the channel currently drives (its mic, or its sink).
    let saved = Levels::load().unwrap_or_default();
    let mut volumes = saved.volumes;
    let mut mutes = saved.mutes;
    for ch in Channel::ALL {
        apply_level(ch, &mics, volumes[ch.0], mutes[ch.0]);
    }

    // Background: keep app streams routed to their bound channels.
    std::thread::spawn(auto_route_loop);

    // The Mix handle is held in an Option so we can drop and re-open it when
    // the USB device dies (host resume after suspend, USB bus reset, cable
    // unplug/replug). The first open uses no retries — if it fails we just
    // bail, same as before. Later reconnects are retried.
    let mut mix = Some(Mix::open()?);

    // The Mix reports its encoder state on the first poll after opening, which
    // arrives as a burst of large dial deltas. Discard anything buffered in the
    // first moment so we don't lurch every channel's volume at startup.
    std::thread::sleep(Duration::from_millis(400));
    if let Some(m) = mix.as_ref() {
        for _ in m.events.try_iter() {}
    }

    // Optional backdrop image. Loaded once here, then reloaded on demand when the
    // TUI bumps `DisplayConfig.background_generation` (so dropping a new
    // background.jpg in the config dir can be picked up without a restart).
    let mut background = load_background();

    // Display behaviour (dim timeout + brightness), re-read live from the TUI.
    let mut display = DisplayConfig::load().unwrap_or_default();
    mix.as_ref().unwrap().init_display(display.full_brightness);

    let mut sources = channel_sources(&mics);
    let _ = refresh_screen(
        mix.as_ref().unwrap(),
        &display,
        &sources,
        &volumes,
        &mutes,
        &mics,
        background.as_ref(),
    );

    log::info!(
        "Mixer running. Turn an encoder to ride a channel; press it to mute. Ctrl-C to stop."
    );

    // Coalesce screen updates onto a ticker so fast knob spins don't flood the
    // panel with full-frame JPEGs.
    let ui = tick(Duration::from_millis(150));
    let mut dirty = false;
    let mut levels_dirty = false;
    let mut ticks: u32 = 0;

    // Panel power: dim after `display.dim_after_secs` of no activity, restore on
    // any input or TUI routing change. Activity = knob/button or a sources change.
    let mut screen = Screen::Active;
    let now = Instant::now();
    let mut last_activity = now;
    let mut last_liveness = now;
    let mut last_tick = now;
    // Set when the event channel closes (device disconnected / host resume that
    // broke the USB handle) so the next loop iteration reconnects.
    let mut device_dead = false;
    // Throttle reconnection attempts — the kernel can take a beat to re-enumerate
    // after a USB bus reset, but we don't want to spin at 100% CPU if it's gone
    // for good.
    let mut last_reconnect = Instant::now() - RECONNECT_BACKOFF;

    loop {
        // Reconnect first thing in each iteration. We can't do this inside
        // select! because `mix.events` borrows `mix` for the whole block.
        if device_dead || mix.is_none() {
            let now = Instant::now();
            if now.duration_since(last_reconnect) >= RECONNECT_BACKOFF {
                last_reconnect = now;
                log::warn!("Beacn Mix disconnected, attempting to reconnect...");
                // Drop the old (dead) handle before re-opening.
                mix = None;
                match Mix::open_with_retries(3) {
                    Ok(new_mix) => {
                        // Same startup-burst drain as the initial open.
                        std::thread::sleep(Duration::from_millis(400));
                        for _ in new_mix.events.try_iter() {}
                        new_mix.init_display(brightness_for(screen, &display));
                        // Push a fresh frame so the panel is showing something,
                        // not the dim/black state the firmware left it in.
                        if let Err(e) = refresh_screen(
                            &new_mix,
                            &display,
                            &sources,
                            &volumes,
                            &mutes,
                            &mics,
                            background.as_ref(),
                        ) {
                            log::warn!("post-reconnect screen push failed: {e:#}");
                        }
                        mix = Some(new_mix);
                        device_dead = false;
                        last_activity = now;
                        last_liveness = now;
                        last_tick = now;
                        log::info!("Reconnected to Beacn Mix.");
                    }
                    Err(e) => {
                        log::warn!("Reconnect failed: {e:#}");
                    }
                }
            }
        }

        let Some(mix_ref) = mix.as_ref() else {
            // Still disconnected and inside the backoff window — sleep a tick
            // and try again.
            std::thread::sleep(Duration::from_millis(150));
            continue;
        };

        select! {
            recv(mix_ref.events) -> msg => match msg {
                Ok(event) => {
                    // Any hardware input counts as activity: reset the dim timer
                    // and, if the panel had dimmed, bring it back to full.
                    last_activity = Instant::now();
                    if screen == Screen::Dimmed {
                        if let Err(e) = mix_ref.wake(display.full_brightness) {
                            log::warn!("wake failed (will reconnect): {e:#}");
                            device_dead = true;
                        } else {
                            screen = Screen::Active;
                            dirty = true;
                        }
                    }

                    match event {
                        Interactions::DialChanged(dial, delta) => {
                            let ch = channel_for_dial(dial);
                            let next = (volumes[ch.0] as i32 + delta as i32 * VOLUME_STEP).clamp(0, VOLUME_MAX);
                            volumes[ch.0] = next as u32;
                            set_channel_volume(ch, &mics, volumes[ch.0]);
                            log::info!("channel {} -> {}%", ch.human(), volumes[ch.0]);
                            dirty = true;
                            levels_dirty = true;
                        }
                        Interactions::ButtonPress(button, ButtonState::Press) => {
                            if let Some(ch) = channel_for_button(button) {
                                mutes[ch.0] = !mutes[ch.0];
                                set_channel_mute(ch, &mics, mutes[ch.0]);
                                log::info!("channel {} {}", ch.human(), if mutes[ch.0] { "muted" } else { "unmuted" });
                                dirty = true;
                                levels_dirty = true;
                            }
                        }
                        Interactions::ButtonPress(_, ButtonState::Release) => {}
                    }
                }
                Err(_) => {
                    // Event channel closed: the device's event-handler thread in
                    // beacn-lib has exited (USB error, device gone). Reconnect on
                    // the next iteration.
                    log::warn!("Event stream ended (device disconnected); will reconnect.");
                    device_dead = true;
                }
            },
            recv(ui) -> _ => {
                let now = Instant::now();

                // A large gap between ticks means the host was suspended; the
                // panel will have powered off, so re-wake it.
                if now.duration_since(last_tick) > RESUME_GAP {
                    if let Err(e) = mix_ref.wake(display.full_brightness) {
                        log::warn!("post-resume wake failed (will reconnect): {e:#}");
                        device_dead = true;
                    } else {
                        screen = Screen::Active;
                        last_activity = now;
                        dirty = true;
                    }
                }
                last_tick = now;

                if dirty {
                    if let Err(e) = refresh_screen(mix_ref, &display, &sources, &volumes, &mutes, &mics, background.as_ref()) {
                        // Transport failure = the device is gone. Anything else
                        // (e.g. a JPEG decoder hiccup) is logged inside refresh_screen.
                        log::warn!("screen update failed: {e}");
                        device_dead = true;
                    }
                    dirty = false;
                }
                if levels_dirty {
                    let _ = Levels { volumes, mutes }.save();
                    levels_dirty = false;
                }
                ticks += 1;

                // Roughly once a second: re-poll routing (so panel labels track
                // TUI assign/unassign) and re-read the display config (so Settings
                // edits apply live). A routing change also counts as activity.
                if ticks.is_multiple_of(7) {
                    // Re-read mic bindings; if they changed (TUI assign/unassign),
                    // re-apply the channels' levels to their new targets so the
                    // newly-bound mic immediately tracks the saved gain/mute.
                    let next_mics = mic_bindings();
                    if next_mics != mics {
                        mics = next_mics;
                        for ch in Channel::ALL {
                            apply_level(ch, &mics, volumes[ch.0], mutes[ch.0]);
                        }
                        dirty = true;
                    }

                    let next = channel_sources(&mics);
                    if next != sources {
                        sources = next;
                        dirty = true;
                        last_activity = now;
                        if screen == Screen::Dimmed {
                            if let Err(e) = mix_ref.wake(display.full_brightness) {
                                log::warn!("wake after routing change failed: {e:#}");
                                device_dead = true;
                            } else {
                                screen = Screen::Active;
                            }
                        }
                    }
                    let cfg = DisplayConfig::load().unwrap_or_default();
                    if cfg != display {
                        // A bumped generation is the TUI's "reload background" signal.
                        if cfg.background_generation != display.background_generation {
                            background = load_background();
                        }
                        display = cfg;
                        mix_ref.set_brightness(brightness_for(screen, &display));
                        dirty = true; // names/background may have changed; redraw the panel
                    }
                }

                // Dim after the configured idle period.
                if screen == Screen::Active
                    && now.duration_since(last_activity) >= Duration::from_secs(display.dim_after_secs)
                {
                    mix_ref.set_brightness(display.dim_brightness);
                    screen = Screen::Dimmed;
                }

                // Periodically re-assert brightness + ping the device so it keeps
                // the panel lit and beacn-lib's own dimmer never takes over.
                if now.duration_since(last_liveness) >= LIVENESS_INTERVAL {
                    if let Err(e) = mix_ref.keepalive() {
                        log::warn!("keepalive failed (will reconnect): {e}");
                        device_dead = true;
                    } else {
                        mix_ref.keep_awake();
                        mix_ref.set_brightness(brightness_for(screen, &display));
                    }
                    last_liveness = now;
                }
            },
        }
    }
}

/// Load the optional panel backdrop from the config dir, logging what it found.
/// Returns `None` (solid colour) when no usable image is present.
fn load_background() -> Option<RgbImage> {
    state::background_path().and_then(|p| {
        let bg = screen::load_background(&p);
        if bg.is_some() {
            log::info!("Using background image {}", p.display());
        }
        bg
    })
}

/// Brightness for the current panel state.
fn brightness_for(screen: Screen, display: &DisplayConfig) -> u8 {
    match screen {
        Screen::Active => display.full_brightness,
        Screen::Dimmed => display.dim_brightness,
    }
}

/// Build the four channel tiles from precomputed sources + current volumes/mutes
/// and push them to the device. Returns `Err` only on transport failure (device
/// gone) — a render failure is logged and treated as non-fatal.
fn refresh_screen(
    mix: &Mix,
    display: &DisplayConfig,
    sources: &[Vec<String>; 4],
    volumes: &[u32; 4],
    mutes: &[bool; 4],
    mics: &[Vec<String>; 4],
    background: Option<&RgbImage>,
) -> Result<()> {
    let views: [ChannelView; 4] = std::array::from_fn(|i| ChannelView {
        label: display.channel_label(i),
        volume: volumes[i],
        muted: mutes[i],
        apps: sources[i].clone(),
        is_mic: !mics[i].is_empty(),
    });
    let jpeg = match screen::render(&views, background) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("render failed: {e}");
            return Ok(());
        }
    };
    mix.set_screen(&jpeg)
}

/// Per-channel source list for the panel, derived from **live routing** — the
/// apps actually playing on each channel's sink — so two instances of the same
/// app (e.g. two Firefox windows) each show on their own channel instead of
/// colliding on one binding key. Falls back to the bound-but-idle app names when
/// nothing is playing on a channel.
fn channel_sources(mics: &[Vec<String>; 4]) -> [Vec<String>; 4] {
    let streams = pw::app_streams().unwrap_or_default();
    let bindings = Bindings::load().unwrap_or_default();
    // Resolve mic node names to friendly descriptions, but only pay for the
    // `pactl list sources` call when at least one channel actually has a mic.
    let source_list = if mics.iter().any(|m| !m.is_empty()) {
        pw::list_sources().unwrap_or_default()
    } else {
        Vec::new()
    };
    std::array::from_fn(|i| {
        let ch = Channel(i);
        // A mic channel shows its mics' names (the encoder rides all of them).
        if !mics[i].is_empty() {
            return mics[i]
                .iter()
                .map(|name| {
                    source_list
                        .iter()
                        .find(|s| &s.name == name)
                        .map(|s| s.label().to_string())
                        .unwrap_or_else(|| name.clone())
                })
                .collect();
        }
        let live: Vec<String> = streams
            .iter()
            .filter(|s| pw::channel_of_sink(&s.sink) == Some(ch))
            .map(|s| panel_label(s).to_string())
            .collect();
        if live.is_empty() {
            bindings.apps_for_channel(ch)
        } else {
            live
        }
    })
}

/// The per-channel mic bindings (channel -> the capture devices riding it).
fn mic_bindings() -> [Vec<String>; 4] {
    Bindings::load().unwrap_or_default().mics_array()
}

/// Apply a channel's volume to whatever it currently drives: every bound mic's
/// gain (input), or its output sink. A channel may hold several mics (e.g. a
/// hardwired one and a wireless one you swap between) — we set them all, and a
/// mic that's currently absent simply fails its `pactl` call (logged at debug,
/// not propagated, so an unplugged mic never takes down the daemon).
fn set_channel_volume(ch: Channel, mics: &[Vec<String>; 4], percent: u32) {
    if mics[ch.0].is_empty() {
        if let Err(e) = pw::set_volume(ch, percent) {
            log::warn!("channel {} set volume failed: {e}", ch.human());
        }
    } else {
        for source in &mics[ch.0] {
            if let Err(e) = pw::set_source_volume(source, percent) {
                log::debug!("mic '{source}' volume failed (absent?): {e}");
            }
        }
    }
}

/// Apply a channel's mute to all its bound mics, or its output sink.
fn set_channel_mute(ch: Channel, mics: &[Vec<String>; 4], mute: bool) {
    if mics[ch.0].is_empty() {
        if let Err(e) = pw::set_mute(ch, mute) {
            log::warn!("channel {} set mute failed: {e}", ch.human());
        }
    } else {
        for source in &mics[ch.0] {
            if let Err(e) = pw::set_source_mute(source, mute) {
                log::debug!("mic '{source}' mute failed (absent?): {e}");
            }
        }
    }
}

/// Push both volume and mute for a channel to its current target(s) (mics or
/// sink). Used on startup and when a mic binding changes.
fn apply_level(ch: Channel, mics: &[Vec<String>; 4], volume: u32, mute: bool) {
    set_channel_volume(ch, mics, volume);
    set_channel_mute(ch, mics, mute);
}

/// Longest `media.name` we'll show in place of the app name on the panel. Above
/// this it's a volatile tab title (e.g. a full video name), so we keep the app
/// name instead of a truncated fragment.
const PANEL_LABEL_MAX: usize = 13;

/// What to print for a stream on the panel: a short, descriptive media name when
/// the app gives one (e.g. "YouTube Music"), otherwise the app name. This lets
/// two instances of the same app stay distinguishable when they happen to expose
/// a clean media name, without churning on long page titles.
fn panel_label(s: &pw::Stream) -> &str {
    if !s.media.is_empty() && s.media != s.app && s.media.chars().count() <= PANEL_LABEL_MAX {
        &s.media
    } else {
        &s.app
    }
}

/// Periodically move newly-appeared app streams onto their bound channel.
/// Bindings are re-read each pass so `assign` takes effect without a restart.
fn auto_route_loop() {
    let mut routed: HashSet<u32> = HashSet::new();
    loop {
        if let Ok(streams) = pw::app_streams() {
            let live: HashSet<u32> = streams.iter().map(|s| s.index).collect();
            routed.retain(|i| live.contains(i));

            if let Ok(bindings) = Bindings::load() {
                for s in &streams {
                    if routed.contains(&s.index) {
                        continue;
                    }
                    // Leave streams already sitting on a channel alone — they may
                    // have been placed there deliberately (e.g. a second instance
                    // of an app moved to a different channel via the TUI). Only
                    // auto-route streams that aren't on a channel yet.
                    if pw::channel_of_sink(&s.sink).is_some() {
                        routed.insert(s.index);
                        continue;
                    }
                    if let Some(ch) = bindings.channel_for_app(&s.app) {
                        match pw::move_stream(s.index, ch) {
                            Ok(()) => {
                                log::info!("routed '{}' -> channel {}", s.app, ch.human());
                                routed.insert(s.index);
                            }
                            Err(e) => log::debug!("auto-route '{}' failed: {e}", s.app),
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Prompt for a number in `0..len` (accepting either 0-based or, for channels,
/// a 1-based value) and return a 0-based index.
fn prompt_index(label: &str, len: usize) -> Result<usize> {
    loop {
        print!("{label}: ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let raw: usize = match line.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                println!("Enter a number.");
                continue;
            }
        };
        // Channel prompts are 1-based; stream list is 0-based. Normalise.
        let idx = if label.starts_with("Channel") {
            raw.wrapping_sub(1)
        } else {
            raw
        };
        if idx < len {
            return Ok(idx);
        }
        println!("Out of range.");
    }
}
