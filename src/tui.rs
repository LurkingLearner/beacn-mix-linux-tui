//! A small terminal UI for managing channel routing — see at a glance which
//! apps are on which channel, and assign / move / unassign them. It never opens
//! the Mix (the running daemon owns the USB interface): it only edits the same
//! `bindings.json` and PipeWire graph the daemon already reacts to. Volume/mute
//! stay read-only here — the hardware knobs remain the source of truth.

use crate::mix::Channel;
use crate::pw;
use crate::state::{Bindings, Levels};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::collections::HashSet;
use std::io;
use std::time::Duration;

/// Per-channel accent, matching the on-device panel (`src/screen.rs`).
const ACCENT: [Color; 4] = [
    Color::Rgb(86, 156, 255),  // blue
    Color::Rgb(95, 205, 140),  // green
    Color::Rgb(214, 162, 86),  // amber
    Color::Rgb(190, 130, 240), // violet
];

/// One manageable entry: either a live playback stream or an app that's bound to
/// a channel but isn't currently playing.
#[derive(Clone)]
enum Row {
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
}

impl Row {
    fn app(&self) -> &str {
        match self {
            Row::Live { app, .. } | Row::Idle { app, .. } => app,
        }
    }

    fn channel(&self) -> Option<Channel> {
        match self {
            Row::Live { channel, .. } => *channel,
            Row::Idle { channel, .. } => Some(*channel),
        }
    }

    fn stream_index(&self) -> Option<u32> {
        match self {
            Row::Live { index, .. } => Some(*index),
            Row::Idle { .. } => None,
        }
    }

    /// Row text for the selectable list, prefixed with its current channel.
    fn display(&self) -> String {
        let ch = match self.channel() {
            Some(c) => format!("CH{}", c.human()),
            None => " — ".to_string(),
        };
        match self {
            Row::Live { label, .. } => format!("[{ch}] {label}"),
            Row::Idle { app, .. } => format!("[{ch}] {app} (idle)"),
        }
    }
}

struct Snapshot {
    rows: Vec<Row>,
    levels: Levels,
}

/// Read the current PipeWire streams + bindings + levels into a fresh snapshot.
fn snapshot() -> Snapshot {
    let streams = pw::app_streams().unwrap_or_default();
    let bindings = Bindings::load().unwrap_or_default();
    let levels = Levels::load().unwrap_or_default();

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

    Snapshot { rows, levels }
}

/// Bind a row to a channel: move its live stream (if any) and persist the binding.
fn assign(row: &Row, ch: Channel) -> Result<()> {
    if let Some(idx) = row.stream_index() {
        pw::move_stream(idx, ch)?;
    }
    let mut bindings = Bindings::load().unwrap_or_default();
    bindings.set(row.app(), ch);
    bindings.save()
}

/// Drop a row's binding and move its live stream back to the default output.
fn unassign(row: &Row) -> Result<()> {
    if let Some(idx) = row.stream_index() {
        let default = pw::default_sink()?;
        pw::move_to_sink(idx, &default)?;
    }
    let mut bindings = Bindings::load().unwrap_or_default();
    bindings.remove(row.app());
    bindings.save()
}

/// Restore the terminal even on early return / panic.
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let _guard = TermGuard;

    // Make sure a panic still leaves the terminal usable (and shows the message).
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut state = ListState::default();
    state.select(Some(0));
    let mut snap = snapshot();
    let mut status = String::new();

    loop {
        // Keep the selection within bounds as streams come and go.
        if snap.rows.is_empty() {
            state.select(None);
        } else {
            let sel = state.selected().unwrap_or(0).min(snap.rows.len() - 1);
            state.select(Some(sel));
        }

        terminal.draw(|f| ui(f, &snap, &mut state, &status))?;

        // Block briefly for input; on timeout, just refresh the snapshot.
        if !event::poll(Duration::from_millis(750))? {
            snap = snapshot();
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let selected = state.selected().and_then(|i| snap.rows.get(i).cloned());
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Up | KeyCode::Char('k') => {
                let cur = state.selected().unwrap_or(0);
                state.select(Some(cur.saturating_sub(1)));
                status.clear();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !snap.rows.is_empty() {
                    let cur = state.selected().unwrap_or(0);
                    state.select(Some((cur + 1).min(snap.rows.len() - 1)));
                }
                status.clear();
            }
            KeyCode::Char(c @ '1'..='4') => {
                if let Some(row) = &selected {
                    let ch = Channel(c as usize - '1' as usize);
                    status = match assign(row, ch) {
                        Ok(()) => format!("Assigned {} → CH{}", row.app(), ch.human()),
                        Err(e) => format!("Assign failed: {e}"),
                    };
                }
                snap = snapshot();
            }
            KeyCode::Char('u') => {
                if let Some(row) = &selected {
                    status = match unassign(row) {
                        Ok(()) => format!("Unassigned {}", row.app()),
                        Err(e) => format!("Unassign failed: {e}"),
                    };
                }
                snap = snapshot();
            }
            KeyCode::Char('r') => {
                snap = snapshot();
                status.clear();
            }
            _ => {}
        }
    }

    Ok(())
}

fn ui(f: &mut Frame, snap: &Snapshot, state: &mut ListState, status: &str) {
    let chunks = Layout::vertical([
        Constraint::Length(11), // channel panels
        Constraint::Min(3),     // selectable stream list
        Constraint::Length(1),  // help / status
    ])
    .split(f.area());

    // Four channel panels, each listing the apps routed to it.
    let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(chunks[0]);
    for (i, col) in cols.iter().enumerate() {
        let ch = Channel(i);
        let vol = snap.levels.volumes[i];
        let title = if snap.levels.mutes[i] {
            format!(" CH{}  {vol}%  MUTE ", ch.human())
        } else {
            format!(" CH{}  {vol}% ", ch.human())
        };

        let mut lines: Vec<Line> = Vec::new();
        for row in &snap.rows {
            if row.channel() == Some(ch) {
                match row {
                    Row::Live { label, .. } => lines.push(Line::from(label.clone())),
                    Row::Idle { app, .. } => lines.push(Line::styled(
                        format!("{app} (idle)"),
                        Style::new().fg(Color::DarkGray),
                    )),
                }
            }
        }
        if lines.is_empty() {
            lines.push(Line::styled("—", Style::new().fg(Color::DarkGray)));
        }

        let block = Block::bordered()
            .title(title)
            .border_style(Style::new().fg(ACCENT[i]));
        f.render_widget(Paragraph::new(lines).block(block), *col);
    }

    // The selectable list of all streams + idle bound apps.
    let items: Vec<ListItem> = snap
        .rows
        .iter()
        .map(|r| ListItem::new(r.display()))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" Streams — select, then 1-4 to assign "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], state);

    // Help line, or the last action's status if there is one.
    let help = if status.is_empty() {
        "↑/↓ select · 1-4 assign · u unassign · r refresh · q quit".to_string()
    } else {
        status.to_string()
    };
    f.render_widget(
        Paragraph::new(Line::styled(help, Style::new().fg(Color::Gray))),
        chunks[2],
    );
}
