//! Host audio side: the four channels are PipeWire null-sinks (`BeacnCh1..4`)
//! that apps get routed into, each looped back to the real output device.
//! The encoder rides the *sink* volume, so it's a true per-channel fader.
//!
//! Everything here drives PipeWire through the PulseAudio-compat `pactl`,
//! which keeps the code small and dependency-light for the MVP. A later pass
//! could swap this for the native `pipewire` crate.

use crate::mix::Channel;
use crate::state::Modules;
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::process::Command;

/// PipeWire node name for a channel's null sink.
pub fn sink_name(ch: Channel) -> String {
    format!("BeacnCh{}", ch.human())
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| anyhow!("failed to exec pactl: {e}"))?;
    if !out.status.success() {
        bail!(
            "pactl {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Name of the user's current default output sink.
pub fn default_sink() -> Result<String> {
    Ok(pactl(&["get-default-sink"])?.trim().to_owned())
}

/// Names of all currently loaded sinks.
fn existing_sinks() -> Result<Vec<String>> {
    Ok(pactl(&["list", "short", "sinks"])?
        .lines()
        .filter_map(|l| l.split('\t').nth(1).map(str::to_owned))
        .collect())
}

/// Map of sink numeric index -> sink name (`pactl list sink-inputs` reports the
/// sink as an index, but we want the name to recognise our `BeacnChN` sinks).
fn sink_name_by_index() -> Result<HashMap<String, String>> {
    Ok(pactl(&["list", "short", "sinks"])?
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            Some((it.next()?.to_owned(), it.next()?.to_owned()))
        })
        .collect())
}

/// Ensure all four channel sinks exist, each looped back to `output`.
/// Idempotent: existing channels are left untouched. Returns the pactl module
/// IDs that were *newly* loaded (so `teardown` can remove exactly those).
pub fn ensure_channels(output: &str) -> Result<Vec<u32>> {
    let present = existing_sinks()?;
    let mut created = Vec::new();

    for ch in Channel::ALL {
        let name = sink_name(ch);
        if present.contains(&name) {
            continue;
        }

        // The null sink apps will be routed into.
        let null_id = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={name}"),
            &format!(
                "sink_properties=device.description=Beacn_Channel_{}",
                ch.human()
            ),
        ])?;
        created.push(parse_module_id(&null_id)?);

        // Pipe that sink's monitor out to the real device so it's audible.
        let loop_id = pactl(&[
            "load-module",
            "module-loopback",
            &format!("source={name}.monitor"),
            &format!("sink={output}"),
            "source_dont_move=true",
            "sink_dont_move=true",
            "latency_msec=20",
        ])?;
        created.push(parse_module_id(&loop_id)?);

        log::info!(
            "Created channel {} -> sink '{name}' -> '{output}'",
            ch.human()
        );
    }

    Ok(created)
}

fn parse_module_id(s: &str) -> Result<u32> {
    s.trim()
        .parse()
        .map_err(|_| anyhow!("unexpected load-module output: {s:?}"))
}

/// Set a channel's volume as a percentage (0..=150).
pub fn set_volume(ch: Channel, percent: u32) -> Result<()> {
    pactl(&["set-sink-volume", &sink_name(ch), &format!("{percent}%")])?;
    Ok(())
}

/// Mute / unmute a channel.
pub fn set_mute(ch: Channel, mute: bool) -> Result<()> {
    pactl(&[
        "set-sink-mute",
        &sink_name(ch),
        if mute { "1" } else { "0" },
    ])?;
    Ok(())
}

/// Set a source's (mic's) volume as a percentage (0..=150). Unlike the channel
/// sinks, this rides the *real* capture device, so it behaves like a hardware
/// gain knob — every app recording from `source` is affected.
pub fn set_source_volume(source: &str, percent: u32) -> Result<()> {
    pactl(&["set-source-volume", source, &format!("{percent}%")])?;
    Ok(())
}

/// Mute / unmute a source (mic) at the device level.
pub fn set_source_mute(source: &str, mute: bool) -> Result<()> {
    pactl(&["set-source-mute", source, if mute { "1" } else { "0" }])?;
    Ok(())
}

/// Route a playback stream (sink-input) onto a channel.
pub fn move_stream(stream_index: u32, ch: Channel) -> Result<()> {
    pactl(&["move-sink-input", &stream_index.to_string(), &sink_name(ch)])?;
    Ok(())
}

/// Move a playback stream onto an arbitrary sink by name (used to unassign a
/// stream back to the real default output).
pub fn move_to_sink(stream_index: u32, sink: &str) -> Result<()> {
    pactl(&["move-sink-input", &stream_index.to_string(), sink])?;
    Ok(())
}

/// Which channel (if any) a sink name refers to, e.g. `BeacnCh2` -> Channel(1).
pub fn channel_of_sink(sink: &str) -> Option<Channel> {
    let n: usize = sink.strip_prefix("BeacnCh")?.parse().ok()?;
    (1..=4).contains(&n).then(|| Channel(n - 1))
}

/// Unload a previously-loaded module (used by teardown).
pub fn unload_module(id: u32) -> Result<()> {
    pactl(&["unload-module", &id.to_string()])?;
    Ok(())
}

/// A currently-playing application stream.
#[derive(Debug, Clone)]
pub struct Stream {
    pub index: u32,
    pub app: String,
    pub media: String,
    /// The sink id this stream is currently routed to.
    pub sink: String,
    /// The pactl module that owns this sink-input, if any. Our own loopbacks
    /// are owned by a `module-loopback` we loaded, which lets us filter them.
    pub owner_module: Option<u32>,
}

impl Stream {
    pub fn label(&self) -> String {
        if self.media.is_empty() || self.media == self.app {
            self.app.clone()
        } else {
            format!("{} — {}", self.app, self.media)
        }
    }
}

/// Enumerate live playback streams by parsing verbose `pactl list sink-inputs`.
pub fn list_streams() -> Result<Vec<Stream>> {
    let text = pactl(&["list", "sink-inputs"])?;
    let mut streams = Vec::new();
    let mut cur: Option<Stream> = None;

    let push = |streams: &mut Vec<Stream>, cur: &mut Option<Stream>| {
        if let Some(s) = cur.take() {
            streams.push(s);
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Sink Input #") {
            push(&mut streams, &mut cur);
            if let Ok(index) = rest.trim().parse::<u32>() {
                cur = Some(Stream {
                    index,
                    app: String::new(),
                    media: String::new(),
                    sink: String::new(),
                    owner_module: None,
                });
            }
        } else if let Some(s) = cur.as_mut() {
            if let Some(v) = trimmed.strip_prefix("Sink: ") {
                s.sink = v.trim().to_owned();
            } else if let Some(v) = trimmed.strip_prefix("Owner Module: ") {
                s.owner_module = v.trim().parse().ok();
            } else if let Some(v) = prop(trimmed, "application.name") {
                s.app = v;
            } else if let Some(v) = prop(trimmed, "media.name") {
                s.media = v;
            }
        }
    }
    push(&mut streams, &mut cur);

    // The `Sink:` line is a numeric index; resolve it to the sink name so
    // callers can recognise our `BeacnChN` channels. Leave it as-is if a name
    // already came through (older/newer pactl variants differ).
    let by_index = sink_name_by_index().unwrap_or_default();
    for s in &mut streams {
        if let Some(name) = by_index.get(&s.sink) {
            s.sink = name.clone();
        }
        if s.app.is_empty() {
            s.app = if s.media.is_empty() {
                format!("stream {}", s.index)
            } else {
                s.media.clone()
            };
        }
    }
    Ok(streams)
}

/// Live application streams, with our own channel loopbacks filtered out.
/// Primary filter is the owner module (we persist the loopback module ids);
/// the name-prefix check is a fallback for a stale `modules.json`.
pub fn app_streams() -> Result<Vec<Stream>> {
    let ours: std::collections::HashSet<u32> = Modules::load()
        .map(|m| m.ids.into_iter().collect())
        .unwrap_or_default();
    Ok(list_streams()?
        .into_iter()
        .filter(|s| !s.owner_module.is_some_and(|id| ours.contains(&id)))
        .filter(|s| !s.app.starts_with("loopback-") && !s.media.starts_with("loopback-"))
        .collect())
}

/// Parse a `key = "value"` property line, returning the unquoted value.
fn prop(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    Some(rest.trim_matches('"').to_owned())
}

/// A capture device (mic) — a real PipeWire *source*, not a monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The node name pactl uses to address it (e.g. `alsa_input.pci-...`).
    pub name: String,
    /// Friendly description for display (e.g. "Yeti Stereo Microphone").
    pub description: String,
}

impl Source {
    /// What to show the user: the friendly description, or the raw name as a
    /// fallback when the device exposes no description.
    pub fn label(&self) -> &str {
        if self.description.is_empty() {
            &self.name
        } else {
            &self.description
        }
    }
}

/// Enumerate real capture devices (mics) by parsing verbose `pactl list sources`,
/// skipping sink monitors (our channel loopbacks and every other sink's `.monitor`).
pub fn list_sources() -> Result<Vec<Source>> {
    Ok(parse_sources(&pactl(&["list", "sources"])?))
}

/// Pure parser for `pactl list sources`, split out so it can be unit-tested
/// without a running PipeWire. A source is dropped when it is a monitor of a
/// sink (`Monitor of Sink:` is anything but `n/a`, with a `.monitor` name
/// fallback for pactl variants that omit the line).
fn parse_sources(text: &str) -> Vec<Source> {
    let mut sources = Vec::new();
    let mut name = String::new();
    let mut description = String::new();
    let mut is_monitor = false;

    let flush =
        |sources: &mut Vec<Source>, name: &mut String, desc: &mut String, mon: &mut bool| {
            let n = std::mem::take(name);
            let d = std::mem::take(desc);
            let monitor = std::mem::replace(mon, false);
            if !n.is_empty() && !monitor && !n.ends_with(".monitor") {
                sources.push(Source {
                    name: n,
                    description: d,
                });
            }
        };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Source #") {
            flush(&mut sources, &mut name, &mut description, &mut is_monitor);
        } else if let Some(v) = trimmed.strip_prefix("Name: ") {
            // First (top-level) Name per block wins; ignore any later ones.
            if name.is_empty() {
                name = v.trim().to_owned();
            }
        } else if let Some(v) = trimmed.strip_prefix("Description: ") {
            if description.is_empty() {
                description = v.trim().to_owned();
            }
        } else if let Some(v) = trimmed.strip_prefix("Monitor of Sink: ") {
            if v.trim() != "n/a" {
                is_monitor = true;
            }
        }
    }
    flush(&mut sources, &mut name, &mut description, &mut is_monitor);
    sources
}

#[cfg(test)]
mod tests {
    //! Only the pure `pactl list sources` parser is exercised here — the rest of
    //! this module shells out to a live PipeWire, which the unit tests can't.

    use super::*;

    // Trimmed-down but realistic `pactl list sources` output: one real mic, one
    // sink monitor (must be filtered), and one of our own channel-loopback
    // monitors (also filtered, via the `.monitor` name fallback).
    const SAMPLE: &str = r#"
Source #50
        State: SUSPENDED
        Name: alsa_input.usb-Blue_Microphones_Yeti-00.analog-stereo
        Description: Yeti Stereo Microphone
        Monitor of Sink: n/a
        Properties:
                device.description = "Yeti Stereo Microphone"
Source #51
        State: RUNNING
        Name: alsa_output.pci-0000_00_1f.3.analog-stereo.monitor
        Description: Monitor of Built-in Audio
        Monitor of Sink: alsa_output.pci-0000_00_1f.3.analog-stereo
Source #77
        State: IDLE
        Name: BeacnCh1.monitor
        Description: Monitor of Beacn_Channel_1
"#;

    #[test]
    fn parse_sources_keeps_real_mics_and_drops_monitors() {
        let got = parse_sources(SAMPLE);
        assert_eq!(got.len(), 1, "only the real mic should survive");
        assert_eq!(
            got[0].name,
            "alsa_input.usb-Blue_Microphones_Yeti-00.analog-stereo"
        );
        assert_eq!(got[0].description, "Yeti Stereo Microphone");
        assert_eq!(got[0].label(), "Yeti Stereo Microphone");
    }

    #[test]
    fn parse_sources_handles_empty_input() {
        assert!(parse_sources("").is_empty());
    }

    #[test]
    fn source_label_falls_back_to_name_without_description() {
        let s = Source {
            name: "alsa_input.thing".into(),
            description: String::new(),
        };
        assert_eq!(s.label(), "alsa_input.thing");
    }
}
