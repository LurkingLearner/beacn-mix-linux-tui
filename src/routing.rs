//! Shared routing model for the two front-ends (`tui` and `gui`): snapshot the
//! current PipeWire streams + mics + saved bindings into [`Row`]s, and bind /
//! unbind a row to a channel. Both UIs render these rows their own way, but the
//! underlying "what exists and how do I (un)assign it" logic lives here so it
//! can't drift between them.

use crate::mix::Channel;
use crate::pw;
use crate::state::Bindings;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// One manageable entry: a live playback stream, an app that's bound to a
/// channel but isn't currently playing, or a capture device (mic) that can ride
/// a channel's gain.
#[derive(Clone)]
pub enum Row {
    Live {
        index: u32,
        app: String,
        label: String,
        channel: Option<Channel>,
    },
    Idle {
        app: String,
        channel: Channel,
    },
    Mic {
        /// Source node name (the pactl handle).
        name: String,
        /// Friendly description for display.
        label: String,
        /// Channel this mic is currently bound to, if any.
        channel: Option<Channel>,
        /// False when the mic is bound but its device isn't currently present
        /// (e.g. a wireless mic that's detached) — still shown so it can be
        /// unbound.
        present: bool,
    },
}

impl Row {
    /// A short human label for status messages.
    pub fn app(&self) -> &str {
        match self {
            Row::Live { app, .. } | Row::Idle { app, .. } => app,
            Row::Mic { label, .. } => label,
        }
    }

    pub fn channel(&self) -> Option<Channel> {
        match self {
            Row::Live { channel, .. } | Row::Mic { channel, .. } => *channel,
            Row::Idle { channel, .. } => Some(*channel),
        }
    }

    pub fn stream_index(&self) -> Option<u32> {
        match self {
            Row::Live { index, .. } => Some(*index),
            Row::Idle { .. } | Row::Mic { .. } => None,
        }
    }

    /// The friendly display name (no channel prefix): the stream/app label, or
    /// the mic's description. Used for list rendering and filter matching.
    pub fn name(&self) -> &str {
        match self {
            Row::Live { label, .. } | Row::Mic { label, .. } => label,
            Row::Idle { app, .. } => app,
        }
    }
}

/// Read the current PipeWire streams + mics + bindings into a fresh row list:
/// live playback streams, bound-but-idle apps, present capture devices, and
/// bound-but-absent mics, in that order.
pub fn rows() -> Vec<Row> {
    let streams = pw::app_streams().unwrap_or_default();
    let bindings = Bindings::load().unwrap_or_default();

    let live_apps: HashSet<&str> = streams.iter().map(|s| s.app.as_str()).collect();

    let mut rows: Vec<Row> = streams
        .iter()
        .map(|s| Row::Live {
            index: s.index,
            app: s.app.clone(),
            label: s.label(),
            channel: pw::channel_of_sink(&s.sink),
        })
        .collect();

    // Bound apps that aren't currently playing — still worth showing/unbinding.
    for ch in Channel::ALL {
        for app in bindings.apps_for_channel(ch) {
            if !live_apps.contains(app.as_str()) {
                rows.push(Row::Idle { app, channel: ch });
            }
        }
    }

    // Capture devices (mics): one row each, showing which channel (if any) they
    // ride. Assigning one binds it so that channel's encoder rides its gain.
    let mics = pw::list_sources().unwrap_or_default();
    let present: HashSet<&str> = mics.iter().map(|m| m.name.as_str()).collect();
    for m in &mics {
        rows.push(Row::Mic {
            name: m.name.clone(),
            label: m.label().to_string(),
            channel: bindings.channel_for_mic(&m.name),
            present: true,
        });
    }
    // Bound mics whose device isn't currently present (e.g. a wireless mic
    // that's detached) — still show them so they can be unbound.
    for (name, &ch) in &bindings.mic_by_source {
        if !present.contains(name.as_str()) {
            rows.push(Row::Mic {
                name: name.clone(),
                label: name.clone(),
                channel: Some(Channel(ch)),
                present: false,
            });
        }
    }

    rows
}

/// Bind a row to a channel: move its live stream (if any) and persist the
/// binding. For a mic there's no graph move — we just persist the binding and the
/// daemon starts riding that mic's gain on the channel within ~1s.
pub fn assign(row: &Row, ch: Channel) -> Result<()> {
    let mut bindings = Bindings::load().unwrap_or_default();
    if let Row::Mic { name, .. } = row {
        bindings.set_mic(ch, name);
        return bindings.save();
    }
    if let Some(idx) = row.stream_index() {
        pw::move_stream(idx, ch).context("moving stream")?;
    }
    bindings.set(row.app(), ch);
    bindings.save()
}

/// Drop a row's binding. For an app, move its live stream back to the default
/// output; for a mic, just clear the binding (the daemon stops riding its gain).
pub fn unassign(row: &Row) -> Result<()> {
    let mut bindings = Bindings::load().unwrap_or_default();
    if let Row::Mic { name, .. } = row {
        bindings.remove_mic(name);
        return bindings.save();
    }
    if let Some(idx) = row.stream_index() {
        let default = pw::default_sink().context("getting default sink")?;
        pw::move_to_sink(idx, &default).context("moving to default sink")?;
    }
    bindings.remove(row.app());
    bindings.save()
}

/// The per-channel mic bindings (channel -> the capture devices riding it).
/// Shared by the daemon and panel preview so both make the same mic-first
/// routing decision.
pub fn mic_bindings() -> [Vec<String>; 4] {
    Bindings::load().unwrap_or_default().mics_array()
}

/// Per-channel labels for the device panel, derived from the same live routing
/// rules the daemon uses. Channels with mics show only those mic names; the
/// remaining channels show live stream labels, falling back to saved app
/// bindings when idle.
pub fn panel_sources(mics: &[Vec<String>; 4]) -> [Vec<String>; 4] {
    let streams = pw::app_streams().unwrap_or_default();
    let bindings = Bindings::load().unwrap_or_default();
    let source_list = if mics.iter().any(|m| !m.is_empty()) {
        pw::list_sources().unwrap_or_default()
    } else {
        Vec::new()
    };
    std::array::from_fn(|i| {
        let ch = Channel(i);
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

/// Longest `media.name` we'll show in place of the app name on the panel.
const PANEL_LABEL_MAX: usize = 13;

fn panel_label(s: &pw::Stream) -> &str {
    if !s.media.is_empty() && s.media != s.app && s.media.chars().count() <= PANEL_LABEL_MAX {
        &s.media
    } else {
        &s.app
    }
}
