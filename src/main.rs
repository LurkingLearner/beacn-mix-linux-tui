//! beacn-mix: drive a Beacn Mix as a PipeWire channel mixer.
//!
//! The Mix is a vendor-specific USB control surface (not a sound card): it
//! only emits encoder deltas + button presses. So the four "channels" live on
//! the host as PipeWire null-sinks, and we map the hardware encoders onto their
//! volumes. See README / plan for the full picture.

mod control;
#[cfg(feature = "gui")]
mod gui;
mod levels;
mod mix;
mod pw;
mod routing;
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
use state::{Bindings, DisplayConfig, Levels, Modules, OutputConfig};
use std::collections::HashSet;
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// How much each encoder tick moves a channel's volume, in percent.
const VOLUME_STEP: i32 = 2;
const VOLUME_MAX: i32 = 150;
/// The displayed audio level is quantized to this many steps (2% each) so a
/// meter that hasn't visibly moved never marks the screen dirty — a silent
/// system keeps its throttled "no frames unless something changed" behaviour.
const LEVEL_STEPS: u8 = 50;
/// The panel can accept positioned JPEGs, so gauge changes (knob spins, mute
/// toggles, live meters) update at 20 fps as small patches without repeatedly
/// transferring the unchanged 800×480 frame.
const PANEL_UPDATE_INTERVAL: Duration = Duration::from_millis(50);
/// Minimum spacing between full 800×480 frames. The firmware cannot decode
/// full frames quickly; pushing them faster than this makes it NAK, and the
/// retry storm in the USB library permanently wedges the display until a hard
/// power cycle. Structural changes therefore wait out this floor (falling
/// through to cheap gauge patches so knobs stay responsive), and everything the
/// user can spin/toggle repaints as a patch, never a full frame.
const FULL_FRAME_MIN_INTERVAL: Duration = Duration::from_millis(200);
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
    /// GUI window to manage routing and settings (mouse-driven; requires `--features gui`).
    Gui,
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
        Command::Gui => {
            #[cfg(feature = "gui")]
            {
                gui::run()
            }
            #[cfg(not(feature = "gui"))]
            {
                anyhow::bail!(
                    "GUI support not compiled in. Rebuild with: cargo build --features gui"
                )
            }
        }
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
            level: 0.68,
        },
        ChannelView {
            label: "Chat".into(),
            volume: 90,
            muted: true,
            apps: vec!["Discord".into(), "Zoom".into()],
            is_mic: false,
            level: 0.5, // hidden: muted channels never show the meter
        },
        ChannelView {
            label: "Music".into(),
            volume: 52,
            muted: false,
            apps: vec!["Spotify".into()],
            is_mic: false,
            level: 0.0,
        },
        ChannelView {
            label: "Mic".into(),
            volume: 75,
            muted: false,
            apps: vec!["Yeti Microphone".into()],
            is_mic: true,
            level: 0.9,
        },
    ];
    let display = DisplayConfig::load().unwrap_or_default();
    let background = state::background_path_for(&display)
        .and_then(|p| screen::load_background(&p, display.background_scrim));
    let jpeg = screen::render(&views, background.as_ref())?;
    std::fs::write(path, &jpeg).with_context(|| format!("writing {path}"))?;
    println!("Wrote sample panel ({} bytes) to {path}", jpeg.len());
    Ok(())
}

/// Ensure the four channel sinks exist; persist any modules we loaded.
fn cmd_setup() -> Result<usize> {
    let output = configured_output()?;
    let new_ids = pw::ensure_channels(&output)?;
    if !new_ids.is_empty() {
        let mut modules = Modules::load()?;
        modules.ids.extend(&new_ids);
        modules.save()?;
    }
    Ok(new_ids.len())
}

/// The output sink the channels should feed: the one saved in `OutputConfig` when
/// it's set and currently present, else the system default. (Only affects the
/// *initial* loopback creation — switching an existing setup goes via
/// [`apply_output`].)
fn configured_output() -> Result<String> {
    if let Some(sink) = OutputConfig::load().unwrap_or_default().sink {
        if pw::list_outputs()
            .map(|outs| outs.iter().any(|o| o.name == sink))
            .unwrap_or(false)
        {
            return Ok(sink);
        }
        log::warn!("configured output '{sink}' not present; using system default");
    }
    pw::default_sink().context("getting default output sink")
}

/// Reconcile the channel loopbacks with the desired output: if a sink is chosen
/// (and present) and the channels aren't already feeding it, reload the loopbacks
/// onto it and keep the persisted `Modules` list in sync. A `None` config or a
/// missing device is a no-op (the channels keep their current output).
fn apply_output(cfg: &OutputConfig) {
    let Some(target) = cfg.sink.as_deref() else {
        return;
    };
    if pw::current_output().as_deref() == Some(target) {
        return; // already feeding the chosen device
    }
    match pw::list_outputs() {
        Ok(outs) if outs.iter().any(|o| o.name == target) => {}
        Ok(_) => {
            log::warn!("chosen output '{target}' not present; keeping current output");
            return;
        }
        Err(e) => {
            log::warn!("listing outputs failed: {e}");
            return;
        }
    }
    match pw::set_output(target) {
        Ok(swaps) => {
            if let Err(e) = persist_module_swaps(&swaps) {
                log::warn!("persisting output module swap failed: {e}");
            }
        }
        Err(e) => log::warn!("switching output failed: {e}"),
    }
}

/// Update the persisted `Modules` list after [`pw::set_output`] reloaded loopbacks:
/// drop the old loopback ids (0 = none) and record the new ones, so `teardown`
/// still removes exactly the modules that are live.
fn persist_module_swaps(swaps: &[(u32, u32)]) -> Result<()> {
    if swaps.is_empty() {
        return Ok(());
    }
    let mut modules = Modules::load()?;
    for (old, new) in swaps {
        if *old != 0 {
            modules.ids.retain(|id| id != old);
        }
        modules.ids.push(*new);
    }
    modules.save()
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

    // Output device the channels feed (Klipsch vs. DAC, etc.). Reconcile once at
    // startup in case the saved choice differs from what the loopbacks feed (e.g.
    // they were left pointing elsewhere by a previous session), then re-read live.
    let mut output_cfg = OutputConfig::load().unwrap_or_default();
    apply_output(&output_cfg);

    // Per-channel mic bindings: when a channel has one, its encoder rides that
    // capture device's gain/mute instead of the output sink. Re-read live below.
    let mut mics = routing::mic_bindings();

    // Live level metering: capture each channel's monitor (or its bound mics)
    // and show a thin meter arc inside the volume gauge. Targets are re-pointed
    // whenever the mic bindings change.
    let meter = levels::LevelMeter::new();
    meter.set_targets(meter_targets(&mics));

    // Restore the channel levels/mutes from the last session, applying each to
    // whatever the channel currently drives (its mic, or its sink).
    let saved = Levels::load().unwrap_or_default();
    let mut volumes = saved.volumes;
    let mut mutes = saved.mutes;
    for ch in Channel::ALL {
        apply_level(ch, &mics, volumes[ch.0], mutes[ch.0]);
    }

    // GUI mute controls must go through this endpoint: `run` owns the live
    // mute state and is responsible for applying it to PipeWire and the panel.
    let control = control::Listener::bind()?;

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

    // Display behaviour (dim timeout + brightness + chosen backdrop), re-read
    // live from the TUI.
    let mut display = DisplayConfig::load().unwrap_or_default();

    // Optional backdrop image. Loaded once here, then reloaded on demand when the
    // Settings can change the backdrop, its scrim, or `background_generation`
    // (so a different / overwritten image is picked up without a restart).
    let mut background = load_background(&display);
    mix.as_ref().unwrap().init_display(display.full_brightness);

    let mut sources = routing::panel_sources(&mics);
    // Last levels drawn on the panel, quantized to LEVEL_STEPS — only channels
    // whose displayed level changed receive a new meter patch.
    let mut shown_levels = [0u8; 4];
    let mut panel_base = match refresh_screen(
        mix.as_ref().unwrap(),
        &display,
        &sources,
        &volumes,
        &mutes,
        &mics,
        &shown_levels,
        background.as_ref(),
    ) {
        Ok(base) => base,
        Err(e) => {
            log::warn!("initial screen push failed: {e:#}");
            None
        }
    };
    log::info!(
        "Mixer running. Turn an encoder to ride a channel; press it to mute. Ctrl-C to stop."
    );

    // Coalesce state changes and update only the small gauge regions at 20 fps.
    // A structural change (`full_dirty`) sends one full base frame, but never
    // more often than FULL_FRAME_MIN_INTERVAL; anything a knob/mute/meter can
    // change goes out as a per-channel gauge patch (`gauge_dirty`).
    let ui = tick(PANEL_UPDATE_INTERVAL);
    let mut full_dirty = false;
    let mut gauge_dirty = [false; 4];
    let mut levels_dirty = false;
    let mut ticks: u32 = 0;

    // Panel power: dim after `display.dim_after_secs` of no activity, restore on
    // any input or TUI routing change. Activity = knob/button or a sources change.
    let mut screen = Screen::Active;
    let now = Instant::now();
    let mut last_activity = now;
    let mut last_liveness = now;
    let mut last_tick = now;
    // When the last full 800×480 frame was pushed. Used to rate-floor full
    // frames (see FULL_FRAME_MIN_INTERVAL) so we never wedge the firmware.
    let mut last_full_frame = now;
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
                        match refresh_screen(
                            &new_mix,
                            &display,
                            &sources,
                            &volumes,
                            &mutes,
                            &mics,
                            &shown_levels,
                            background.as_ref(),
                        ) {
                            Ok(base) => {
                                panel_base = base;
                                last_full_frame = now;
                            }
                            Err(e) => {
                                panel_base = None;
                                log::warn!("post-reconnect screen push failed: {e:#}");
                            }
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

        if let Err(e) = control.service(|command| match command {
            control::Command::SetMute { channel, muted } => {
                let Some(ch) = (channel < 4).then_some(Channel(channel)) else {
                    anyhow::bail!("invalid channel {}", channel + 1);
                };
                mutes[ch.0] = muted;
                set_channel_mute(ch, &mics, muted);
                Levels { volumes, mutes }.save()?;
                gauge_dirty[ch.0] = true;
                levels_dirty = false;
                log::info!(
                    "channel {} {} from GUI",
                    ch.human(),
                    if muted { "muted" } else { "unmuted" }
                );
                Ok(())
            }
        }) {
            log::warn!("control request failed: {e:#}");
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
                            full_dirty = true;
                        }
                    }

                    match event {
                        Interactions::DialChanged(dial, delta) => {
                            let ch = channel_for_dial(dial);
                            let next = (volumes[ch.0] as i32 + delta as i32 * VOLUME_STEP).clamp(0, VOLUME_MAX);
                            volumes[ch.0] = next as u32;
                            set_channel_volume(ch, &mics, volumes[ch.0]);
                            log::info!("channel {} -> {}%", ch.human(), volumes[ch.0]);
                            gauge_dirty[ch.0] = true;
                            levels_dirty = true;
                        }
                        Interactions::ButtonPress(button, ButtonState::Press) => {
                            if let Some(ch) = channel_for_button(button) {
                                mutes[ch.0] = !mutes[ch.0];
                                set_channel_mute(ch, &mics, mutes[ch.0]);
                                log::info!("channel {} {}", ch.human(), if mutes[ch.0] { "muted" } else { "unmuted" });
                                gauge_dirty[ch.0] = true;
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
                        full_dirty = true;
                    }
                }
                last_tick = now;

                // Live meters: dirty a channel's gauge only when its *displayed*
                // (quantized) level moved, and never while dimmed — playing
                // audio must not keep pushing to a dimmed panel, and level motion
                // deliberately does not count as activity for the dim timer.
                if screen == Screen::Active {
                    let next_levels = quantized_levels(&meter);
                    if next_levels != shown_levels {
                        for i in 0..4 {
                            if next_levels[i] != shown_levels[i] {
                                gauge_dirty[i] = true;
                            }
                        }
                        shown_levels = next_levels;
                    }
                }

                // Full frame: only for structural changes, and never faster than
                // the firmware can decode (see FULL_FRAME_MIN_INTERVAL) or the
                // retry storm wedges the display. Inside the floor we leave
                // `full_dirty` pending and fall through to gauge patches so knobs
                // stay responsive.
                if full_dirty
                    && now.duration_since(last_full_frame) >= FULL_FRAME_MIN_INTERVAL
                {
                    match refresh_screen(mix_ref, &display, &sources, &volumes, &mutes, &mics, &shown_levels, background.as_ref()) {
                        Ok(base) => {
                            panel_base = base;
                            last_full_frame = now;
                            full_dirty = false;
                            // The full frame already contains every gauge, so no
                            // patch is needed this pass.
                            gauge_dirty = [false; 4];
                        }
                        Err(e) => {
                            panel_base = None;
                            log::warn!("screen update failed: {e}");
                            device_dead = true;
                        }
                    }
                }

                // Cheap per-channel gauge patches for knob/mute/meter changes.
                if !device_dead {
                    let mut clear_base = false;
                    if let Some(base) = panel_base.as_ref() {
                        for i in 0..4 {
                            if !gauge_dirty[i] {
                                continue;
                            }
                            let view = ChannelView {
                                label: String::new(), // unused by the gauge
                                volume: volumes[i],
                                muted: mutes[i],
                                apps: Vec::new(),
                                is_mic: !mics[i].is_empty(),
                                level: shown_levels[i] as f32 / LEVEL_STEPS as f32,
                            };
                            match screen::render_gauge_patch(base, i, &view) {
                                Ok((x, y, jpeg)) => {
                                    if let Err(e) = mix_ref.set_screen_region(x, y, &jpeg) {
                                        log::warn!("gauge patch update failed: {e}");
                                        device_dead = true;
                                        clear_base = true;
                                        break;
                                    }
                                    gauge_dirty[i] = false;
                                }
                                Err(e) => {
                                    log::warn!("gauge patch render failed: {e}");
                                }
                            }
                        }
                    }
                    if clear_base {
                        panel_base = None;
                    }
                }
                if levels_dirty {
                    let _ = Levels { volumes, mutes }.save();
                    levels_dirty = false;
                }
                ticks += 1;

                // Roughly once a second: re-poll routing (so panel labels track
                // TUI assign/unassign) and re-read the display config (so Settings
                // edits apply live). A routing change also counts as activity.
                if ticks.is_multiple_of(20) {
                    // Re-read mic bindings; if they changed (TUI assign/unassign),
                    // re-apply the channels' levels to their new targets so the
                    // newly-bound mic immediately tracks the saved gain/mute.
                    let next_mics = routing::mic_bindings();
                    if next_mics != mics {
                        mics = next_mics;
                        for ch in Channel::ALL {
                            apply_level(ch, &mics, volumes[ch.0], mutes[ch.0]);
                        }
                        // Re-point the level meters (mic gain vs. sink monitor).
                        meter.set_targets(meter_targets(&mics));
                        full_dirty = true;
                    }

                    let next = routing::panel_sources(&mics);
                    if next != sources {
                        sources = next;
                        full_dirty = true;
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
                    // Output-device switch from the TUI Settings page.
                    let ocfg = OutputConfig::load().unwrap_or_default();
                    if ocfg != output_cfg {
                        output_cfg = ocfg;
                        apply_output(&output_cfg);
                    }

                    let cfg = DisplayConfig::load().unwrap_or_default();
                    if cfg != display {
                        // Reload the backdrop when its file or scrim changes, or when
                        // Settings bumps the generation (overwrite-in-place reload).
                        if cfg.background_file != display.background_file
                            || cfg.background_scrim != display.background_scrim
                            || cfg.background_generation != display.background_generation
                        {
                            background = load_background(&cfg);
                        }
                        display = cfg;
                        mix_ref.set_brightness(brightness_for(screen, &display));
                        full_dirty = true; // names/background may have changed; redraw the panel
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

/// Load the backdrop the display config selects (a file under `backgrounds/`),
/// logging what it found. Returns `None` (solid colour) when nothing is selected
/// or the chosen file is missing/undecodable.
fn load_background(display: &DisplayConfig) -> Option<RgbImage> {
    state::background_path_for(display).and_then(|p| {
        let bg = screen::load_background(&p, display.background_scrim);
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

/// Build and push a full panel frame with the current gauges (levels included)
/// composited on. The returned gauge-free RGB **base** is retained as the origin
/// for later small [`screen::render_gauge_patch`] updates.
///
/// A render/encode failure is non-fatal (`Ok(None)` — keep running, the panel
/// just won't refresh this pass); only a transport failure is an `Err`.
#[allow(clippy::too_many_arguments)]
fn refresh_screen(
    mix: &Mix,
    display: &DisplayConfig,
    sources: &[Vec<String>; 4],
    volumes: &[u32; 4],
    mutes: &[bool; 4],
    mics: &[Vec<String>; 4],
    levels: &[u8; 4],
    background: Option<&RgbImage>,
) -> Result<Option<RgbImage>> {
    let views: [ChannelView; 4] = std::array::from_fn(|i| ChannelView {
        label: display.channel_label(i),
        volume: volumes[i],
        muted: mutes[i],
        apps: sources[i].clone(),
        is_mic: !mics[i].is_empty(),
        level: levels[i] as f32 / LEVEL_STEPS as f32,
    });
    // Render the base (no gauges) once, then composite the gauges for the full
    // frame — the base is exactly what patches crop from later.
    let base = match screen::render_base_rgb(&views, background) {
        Ok(image) => image,
        Err(e) => {
            log::warn!("render failed: {e}");
            return Ok(None);
        }
    };
    let displayed = match screen::composite_gauges(&base, &views) {
        Ok(image) => image,
        Err(e) => {
            log::warn!("gauge composite failed: {e}");
            return Ok(None);
        }
    };
    let jpeg = match screen::encode_jpeg(&displayed) {
        Ok(jpeg) => jpeg,
        Err(e) => {
            log::warn!("JPEG encode failed: {e}");
            return Ok(None);
        }
    };
    mix.set_screen(&jpeg)?;
    Ok(Some(base))
}

/// What each channel's level meter should capture: the bound mics when there
/// are any (the meter then shows the loudest of them — i.e. you talking),
/// otherwise the channel sink's monitor (the audio apps play into it).
fn meter_targets(mics: &[Vec<String>; 4]) -> [Vec<String>; 4] {
    std::array::from_fn(|i| {
        if mics[i].is_empty() {
            vec![format!("{}.monitor", pw::sink_name(Channel(i)))]
        } else {
            mics[i].clone()
        }
    })
}

/// Current per-channel displayed levels, perceptually mapped (dBFS) and
/// quantized to `LEVEL_STEPS` so tiny fluctuations don't churn the panel.
fn quantized_levels(meter: &levels::LevelMeter) -> [u8; 4] {
    let raw = meter.levels();
    std::array::from_fn(|i| (levels::display_fraction(raw[i]) * LEVEL_STEPS as f32).round() as u8)
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
