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
use mix::{channel_for_button, channel_for_dial, Channel, Mix};
use screen::ChannelView;
use state::{Bindings, Levels, Modules};
use std::collections::HashSet;
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// How much each encoder tick moves a channel's volume, in percent.
const VOLUME_STEP: i32 = 2;
const VOLUME_MAX: i32 = 150;
/// If this long passes with no input, assume the panel slept and wake it on the
/// next interaction.
const WAKE_AFTER_IDLE: Duration = Duration::from_secs(20);

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
            volume: 100,
            muted: false,
            apps: vec!["Firefox".into(), "YouTube Music".into()],
        },
        ChannelView {
            volume: 90,
            muted: true,
            apps: vec!["Discord".into(), "Zoom".into()],
        },
        ChannelView {
            volume: 52,
            muted: false,
            apps: vec!["Spotify".into()],
        },
        ChannelView {
            volume: 75,
            muted: false,
            apps: vec![],
        },
    ];
    let jpeg = screen::render(&views)?;
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

    // Restore the channel levels/mutes from the last session.
    let saved = Levels::load().unwrap_or_default();
    let mut volumes = saved.volumes;
    let mut mutes = saved.mutes;
    for ch in Channel::ALL {
        pw::set_volume(ch, volumes[ch.0])?;
        pw::set_mute(ch, mutes[ch.0])?;
    }

    // Background: keep app streams routed to their bound channels.
    std::thread::spawn(auto_route_loop);

    let mix = Mix::open()?;

    // The Mix reports its encoder state on the first poll after opening, which
    // arrives as a burst of large dial deltas. Discard anything buffered in the
    // first moment so we don't lurch every channel's volume at startup.
    std::thread::sleep(Duration::from_millis(400));
    for _ in mix.events.try_iter() {}

    mix.init_display();
    let mut sources = channel_sources();
    refresh_screen(&mix, &sources, &volumes, &mutes);

    log::info!(
        "Mixer running. Turn an encoder to ride a channel; press it to mute. Ctrl-C to stop."
    );

    // Coalesce screen updates onto a ticker so fast knob spins don't flood the
    // panel with full-frame JPEGs; re-arm the dim timer periodically.
    let ui = tick(Duration::from_millis(150));
    let mut dirty = false;
    let mut levels_dirty = false;
    let mut ticks: u32 = 0;
    let mut last_event = Instant::now();

    loop {
        select! {
            recv(mix.events) -> msg => match msg {
                Ok(event) => {
                    // First input after an idle gap: the panel has likely gone
                    // into firmware sleep, so wake it before redrawing.
                    let now = Instant::now();
                    if now.duration_since(last_event) > WAKE_AFTER_IDLE {
                        mix.wake();
                        dirty = true;
                    }
                    last_event = now;

                    match event {
                        Interactions::DialChanged(dial, delta) => {
                            let ch = channel_for_dial(dial);
                            let next = (volumes[ch.0] as i32 + delta as i32 * VOLUME_STEP).clamp(0, VOLUME_MAX);
                            volumes[ch.0] = next as u32;
                            if let Err(e) = pw::set_volume(ch, volumes[ch.0]) {
                                log::warn!("set volume failed: {e}");
                            } else {
                                log::info!("channel {} -> {}%", ch.human(), volumes[ch.0]);
                            }
                            dirty = true;
                            levels_dirty = true;
                        }
                        Interactions::ButtonPress(button, ButtonState::Press) => {
                            if let Some(ch) = channel_for_button(button) {
                                mutes[ch.0] = !mutes[ch.0];
                                if let Err(e) = pw::set_mute(ch, mutes[ch.0]) {
                                    log::warn!("set mute failed: {e}");
                                } else {
                                    log::info!("channel {} {}", ch.human(), if mutes[ch.0] { "muted" } else { "unmuted" });
                                }
                                dirty = true;
                                levels_dirty = true;
                            }
                        }
                        Interactions::ButtonPress(_, ButtonState::Release) => {}
                    }
                }
                Err(_) => {
                    log::warn!("Event stream ended (device disconnected?).");
                    break;
                }
            },
            recv(ui) -> _ => {
                if dirty {
                    refresh_screen(&mix, &sources, &volumes, &mutes);
                    dirty = false;
                }
                if levels_dirty {
                    let _ = Levels { volumes, mutes }.save();
                    levels_dirty = false;
                }
                ticks += 1;
                // Re-poll routing roughly once a second so panel labels track
                // assign/unassign done in the TUI without needing a knob turn.
                if ticks.is_multiple_of(7) {
                    let next = channel_sources();
                    if next != sources {
                        sources = next;
                        dirty = true;
                    }
                }
                if ticks.is_multiple_of(200) {
                    mix.keep_awake();
                }
            }
        }
    }

    // Persist the final state on a clean shutdown.
    let _ = Levels { volumes, mutes }.save();
    Ok(())
}

/// Build the four channel tiles from precomputed sources + current volumes/mutes.
fn refresh_screen(mix: &Mix, sources: &[Vec<String>; 4], volumes: &[u32; 4], mutes: &[bool; 4]) {
    let views: [ChannelView; 4] = std::array::from_fn(|i| ChannelView {
        volume: volumes[i],
        muted: mutes[i],
        apps: sources[i].clone(),
    });
    match screen::render(&views) {
        Ok(jpeg) => {
            if let Err(e) = mix.set_screen(&jpeg) {
                log::warn!("screen update failed: {e}");
            }
        }
        Err(e) => log::warn!("render failed: {e}"),
    }
}

/// Per-channel source list for the panel, derived from **live routing** — the
/// apps actually playing on each channel's sink — so two instances of the same
/// app (e.g. two Firefox windows) each show on their own channel instead of
/// colliding on one binding key. Falls back to the bound-but-idle app names when
/// nothing is playing on a channel.
fn channel_sources() -> [Vec<String>; 4] {
    let streams = pw::app_streams().unwrap_or_default();
    let bindings = Bindings::load().unwrap_or_default();
    std::array::from_fn(|i| {
        let ch = Channel(i);
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
