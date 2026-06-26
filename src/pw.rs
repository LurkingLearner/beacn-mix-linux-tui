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
        created.push(load_loopback(ch, output)?);

        log::info!(
            "Created channel {} -> sink '{name}' -> '{output}'",
            ch.human()
        );
    }

    Ok(created)
}

/// Load a `module-loopback` piping a channel's monitor out to `output`, returning
/// the new module id. The `*_dont_move=true` flags pin it so PipeWire's own
/// stream-follows-default logic can't drag it off `output` — switching the output
/// is therefore done by reloading this module (see [`set_output`]), not moving it.
fn load_loopback(ch: Channel, output: &str) -> Result<u32> {
    let name = sink_name(ch);
    let id = pactl(&[
        "load-module",
        "module-loopback",
        &format!("source={name}.monitor"),
        &format!("sink={output}"),
        "source_dont_move=true",
        "sink_dont_move=true",
        "latency_msec=20",
    ])?;
    parse_module_id(&id)
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

/// A real output device (sink) the channels can be routed to — i.e. any sink
/// except our own `BeacnChN` null-sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The node name pactl uses to address it.
    pub name: String,
    /// Friendly description for display (e.g. "Klipsch R-15PM Analog Stereo").
    pub description: String,
}

impl Output {
    pub fn label(&self) -> &str {
        if self.description.is_empty() {
            &self.name
        } else {
            &self.description
        }
    }
}

/// Enumerate real output devices (sinks), excluding our own channel null-sinks.
pub fn list_outputs() -> Result<Vec<Output>> {
    Ok(parse_outputs(&pactl(&["list", "sinks"])?))
}

/// Pure parser for `pactl list sinks`, dropping our `BeacnChN` channel sinks so
/// only genuine output devices remain. Split out for unit testing.
fn parse_outputs(text: &str) -> Vec<Output> {
    let mut outputs = Vec::new();
    let mut name = String::new();
    let mut description = String::new();

    let flush = |outputs: &mut Vec<Output>, name: &mut String, desc: &mut String| {
        let n = std::mem::take(name);
        let d = std::mem::take(desc);
        if !n.is_empty() && !n.starts_with("BeacnCh") {
            outputs.push(Output {
                name: n,
                description: d,
            });
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Sink #") {
            flush(&mut outputs, &mut name, &mut description);
        } else if let Some(v) = trimmed.strip_prefix("Name: ") {
            if name.is_empty() {
                name = v.trim().to_owned();
            }
        } else if let Some(v) = trimmed.strip_prefix("Description: ") {
            if description.is_empty() {
                description = v.trim().to_owned();
            }
        }
    }
    flush(&mut outputs, &mut name, &mut description);
    outputs
}

/// One of our channel loopbacks, as discovered from `pactl list modules`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Loopback {
    module_id: u32,
    channel: Channel,
    /// The sink this loopback currently feeds.
    sink: String,
}

/// Discover our four channel loopbacks (module id + which channel + current sink)
/// by parsing `pactl list modules`. Used to read the current output and to know
/// which modules to reload when switching it.
fn channel_loopbacks() -> Result<Vec<Loopback>> {
    Ok(parse_loopbacks(&pactl(&["list", "modules"])?))
}

/// Pure parser for `pactl list modules`: pick out `module-loopback`s whose source
/// is one of our `BeacnChN.monitor`s, with the sink they feed. Split for testing.
fn parse_loopbacks(text: &str) -> Vec<Loopback> {
    let mut loops = Vec::new();
    let mut id: Option<u32> = None;
    let mut is_loopback = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Module #") {
            id = rest.trim().parse().ok();
            is_loopback = false;
        } else if trimmed == "Name: module-loopback" {
            is_loopback = true;
        } else if let Some(arg) = trimmed.strip_prefix("Argument: ") {
            if !is_loopback {
                continue;
            }
            // Argument is space-separated `key=value` tokens; we want the source
            // (to identify the channel) and the sink it currently feeds.
            let source = arg_token(arg, "source=");
            let sink = arg_token(arg, "sink=");
            if let (Some(source), Some(sink), Some(mid)) = (source, sink, id) {
                if let Some(channel) = source.strip_suffix(".monitor").and_then(channel_of_sink) {
                    loops.push(Loopback {
                        module_id: mid,
                        channel,
                        sink,
                    });
                }
            }
        }
    }
    loops
}

/// Value of a `key=value` token within a space-separated argument string.
/// `key` must include the trailing `=` (e.g. `"sink="`).
fn arg_token(arg: &str, key: &str) -> Option<String> {
    arg.split_whitespace()
        .find_map(|tok| tok.strip_prefix(key))
        .map(str::to_owned)
}

/// The sink our channels currently feed (read from the channel loopbacks), if any.
/// All four normally share one output, so the first is representative.
pub fn current_output() -> Option<String> {
    channel_loopbacks().ok()?.first().map(|l| l.sink.clone())
}

/// Repoint every channel loopback at `output`, so all Beacn-routed audio switches
/// to that device. Reloads each loopback module (the `dont_move` pins rule out a
/// plain move) and returns the `(old_id, new_id)` swaps so the caller can keep the
/// persisted `Modules` list in sync (`old_id` is 0 when a channel had no loopback).
pub fn set_output(output: &str) -> Result<Vec<(u32, u32)>> {
    let loops = channel_loopbacks()?;
    let mut swaps = Vec::new();
    for ch in Channel::ALL {
        match loops.iter().find(|l| l.channel == ch) {
            Some(lb) if lb.sink == output => {} // already feeding the target
            Some(lb) => {
                unload_module(lb.module_id)?;
                let new_id = load_loopback(ch, output)?;
                swaps.push((lb.module_id, new_id));
            }
            None => {
                let new_id = load_loopback(ch, output)?;
                swaps.push((0, new_id));
            }
        }
    }
    if !swaps.is_empty() {
        log::info!("Switched channel output -> '{output}'");
    }
    Ok(swaps)
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

    // A real mic, the active output, and one of our channel sinks (filtered out).
    const SINKS: &str = r#"
Sink #57
        State: SUSPENDED
        Name: alsa_output.usb-Generic_USB_Audio-00.HiFi__Headphones__sink
        Description: USB Audio Front Headphones
Sink #60
        State: RUNNING
        Name: alsa_output.usb-NAE_Klipsch_R-15PM-01.analog-stereo
        Description: Klipsch R-15PM Analog Stereo
Sink #1950
        State: RUNNING
        Name: BeacnCh1
        Description: Beacn_Channel_1
Sink #59892
        State: SUSPENDED
        Name: alsa_output.usb-GuangZhou_FiiO_Electronics_Co._Ltd_FiiO_K3-00.iec958-stereo
        Description: FiiO K3 Digital Stereo (IEC958)
"#;

    #[test]
    fn parse_outputs_lists_real_sinks_and_drops_channel_sinks() {
        let got = parse_outputs(SINKS);
        let names: Vec<&str> = got.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names.len(), 3, "the BeacnCh1 channel sink must be dropped");
        assert!(names.iter().all(|n| !n.starts_with("BeacnCh")));
        assert!(got
            .iter()
            .any(|o| o.label() == "Klipsch R-15PM Analog Stereo"));
        assert!(got
            .iter()
            .any(|o| o.label() == "FiiO K3 Digital Stereo (IEC958)"));
    }

    // Two of our channel loopbacks plus an unrelated module that must be ignored.
    const MODULES: &str = r#"
Module #100
        Name: module-loopback
        Argument: source=BeacnCh1.monitor sink=alsa_output.klipsch source_dont_move=true sink_dont_move=true latency_msec=20
Module #101
        Name: module-null-sink
        Argument: sink_name=BeacnCh1 sink_properties=device.description=Beacn_Channel_1
Module #102
        Name: module-loopback
        Argument: source=BeacnCh3.monitor sink=alsa_output.klipsch source_dont_move=true sink_dont_move=true latency_msec=20
Module #103
        Name: module-loopback
        Argument: source=SomeOther.monitor sink=alsa_output.klipsch
"#;

    #[test]
    fn parse_loopbacks_finds_channel_loopbacks_only() {
        let got = parse_loopbacks(MODULES);
        assert_eq!(got.len(), 2, "only our two BeacnCh loopbacks should match");
        assert_eq!(got[0].module_id, 100);
        assert_eq!(got[0].channel, Channel(0));
        assert_eq!(got[0].sink, "alsa_output.klipsch");
        assert_eq!(got[1].module_id, 102);
        assert_eq!(got[1].channel, Channel(2));
    }

    #[test]
    fn arg_token_extracts_keyed_values() {
        let arg = "source=BeacnCh2.monitor sink=alsa_output.foo latency_msec=20";
        assert_eq!(
            arg_token(arg, "source="),
            Some("BeacnCh2.monitor".to_string())
        );
        assert_eq!(arg_token(arg, "sink="), Some("alsa_output.foo".to_string()));
        assert_eq!(arg_token(arg, "missing="), None);
    }
}
