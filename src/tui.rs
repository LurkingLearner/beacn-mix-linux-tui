//! A small terminal UI with two pages:
//!  - **Routing**: see which apps are on which channel, and assign / move /
//!    unassign them.
//!  - **Settings**: edit the panel display behaviour (dim timeout + brightness).
//!
//! It never opens the Mix (the running daemon owns the USB interface): it only
//! edits the same `bindings.json` / `display.json` and PipeWire graph the daemon
//! already reacts to. Volume/mute stay read-only here — the knobs are the source
//! of truth. `Tab` switches pages; `q` quits.

use crate::mix::Channel;
use crate::pw;
use crate::state::{Bindings, DisplayConfig, Levels};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Tabs};
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

/// Settings rows: dim-after, full brightness, dim brightness, 4 channel names,
/// then the "reload background" action button.
const SETTINGS_FIELDS: usize = 8;
/// Index of the first channel-name row (rows below this are numeric).
const NAME_FIELD_BASE: usize = 3;
/// Index of the "reload background" action row (just past the 4 name rows).
const REFRESH_FIELD: usize = NAME_FIELD_BASE + 4;
/// Max length of a custom channel name.
const NAME_MAX: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Routing,
    Settings,
}

/// One manageable entry: a live playback stream, an app that's bound to a
/// channel but isn't currently playing, or a capture device (mic) that can ride
/// a channel's gain.
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
    Mic {
        /// Source node name (the pactl handle).
        name: String,
        /// Friendly description for display.
        label: String,
        /// Channel this mic is currently bound to, if any.
        channel: Option<Channel>,
    },
}

impl Row {
    /// A short human label for status messages.
    fn app(&self) -> &str {
        match self {
            Row::Live { app, .. } | Row::Idle { app, .. } => app,
            Row::Mic { label, .. } => label,
        }
    }

    fn channel(&self) -> Option<Channel> {
        match self {
            Row::Live { channel, .. } | Row::Mic { channel, .. } => *channel,
            Row::Idle { channel, .. } => Some(*channel),
        }
    }

    fn stream_index(&self) -> Option<u32> {
        match self {
            Row::Live { index, .. } => Some(*index),
            Row::Idle { .. } | Row::Mic { .. } => None,
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
            Row::Mic { label, .. } => format!("[{ch}] {label} (mic)"),
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

    // Capture devices (mics): one row each, showing which channel (if any) they
    // ride. Selecting one + 1-4 binds it so that channel's encoder rides its gain.
    let mics = pw::list_sources().unwrap_or_default();
    let present: HashSet<&str> = mics.iter().map(|m| m.name.as_str()).collect();
    for m in &mics {
        rows.push(Row::Mic {
            name: m.name.clone(),
            label: m.label().to_string(),
            channel: bindings.channel_for_mic(&m.name),
        });
    }
    // Bound mics whose device isn't currently present (e.g. a wireless mic that's
    // detached) — still show them so they can be unbound.
    for (name, &ch) in &bindings.mic_by_source {
        if !present.contains(name.as_str()) {
            rows.push(Row::Mic {
                name: name.clone(),
                label: name.clone(),
                channel: Some(Channel(ch)),
            });
        }
    }

    Snapshot { rows, levels }
}

/// Bind a row to a channel: move its live stream (if any) and persist the
/// binding. For a mic there's no graph move — we just persist the binding and the
/// daemon starts riding that mic's gain on the channel within ~1s.
fn assign(row: &Row, ch: Channel) -> Result<()> {
    let mut bindings = Bindings::load().unwrap_or_default();
    if let Row::Mic { name, .. } = row {
        bindings.set_mic(ch, name);
        return bindings.save();
    }
    if let Some(idx) = row.stream_index() {
        pw::move_stream(idx, ch)?;
    }
    bindings.set(row.app(), ch);
    bindings.save()
}

/// Drop a row's binding. For an app, move its live stream back to the default
/// output; for a mic, just clear the binding (the daemon stops riding its gain).
fn unassign(row: &Row) -> Result<()> {
    let mut bindings = Bindings::load().unwrap_or_default();
    if let Row::Mic { name, .. } = row {
        bindings.remove_mic(name);
        return bindings.save();
    }
    if let Some(idx) = row.stream_index() {
        let default = pw::default_sink()?;
        pw::move_to_sink(idx, &default)?;
    }
    bindings.remove(row.app());
    bindings.save()
}

/// Nudge a settings field by `dir` (±1), each field with its own step + bounds.
fn adjust(cfg: &mut DisplayConfig, field: usize, dir: i64) {
    match field {
        0 => {
            let min = (cfg.dim_after_secs as i64 / 60 + dir).clamp(1, 120);
            cfg.dim_after_secs = (min * 60) as u64;
        }
        1 => cfg.full_brightness = (cfg.full_brightness as i64 + dir * 5).clamp(5, 100) as u8,
        2 => cfg.dim_brightness = (cfg.dim_brightness as i64 + dir).clamp(1, 100) as u8,
        _ => {}
    }
}

struct App {
    page: Page,
    snap: Snapshot,
    routing: ListState,
    settings: ListState,
    display: DisplayConfig,
    /// True while typing into the selected channel-name field.
    editing: bool,
    status: String,
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

    let mut routing = ListState::default();
    routing.select(Some(0));
    let mut settings = ListState::default();
    settings.select(Some(0));
    let mut app = App {
        page: Page::Routing,
        snap: snapshot(),
        routing,
        settings,
        display: DisplayConfig::load().unwrap_or_default(),
        editing: false,
        status: String::new(),
    };

    loop {
        // Keep selections within bounds as streams come and go.
        clamp_selection(&mut app.routing, app.snap.rows.len());
        clamp_selection(&mut app.settings, SETTINGS_FIELDS);

        terminal.draw(|f| draw(f, &mut app))?;

        // Block briefly for input; on timeout, refresh the routing snapshot.
        if !event::poll(Duration::from_millis(750))? {
            if app.page == Page::Routing {
                app.snap = snapshot();
            }
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // While renaming a channel, all keys go to the text field.
        if app.editing {
            handle_edit_key(&mut app, key.code);
            continue;
        }

        // Global keys.
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Tab => {
                app.page = match app.page {
                    Page::Routing => Page::Settings,
                    Page::Settings => Page::Routing,
                };
                app.status.clear();
                continue;
            }
            _ => {}
        }

        match app.page {
            Page::Routing => handle_routing_key(&mut app, key.code),
            Page::Settings => handle_settings_key(&mut app, key.code),
        }
    }

    Ok(())
}

fn clamp_selection(state: &mut ListState, len: usize) {
    if len == 0 {
        state.select(None);
    } else {
        state.select(Some(state.selected().unwrap_or(0).min(len - 1)));
    }
}

fn handle_routing_key(app: &mut App, code: KeyCode) {
    let selected = app
        .routing
        .selected()
        .and_then(|i| app.snap.rows.get(i).cloned());
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            let cur = app.routing.selected().unwrap_or(0);
            app.routing.select(Some(cur.saturating_sub(1)));
            app.status.clear();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !app.snap.rows.is_empty() {
                let cur = app.routing.selected().unwrap_or(0);
                app.routing
                    .select(Some((cur + 1).min(app.snap.rows.len() - 1)));
            }
            app.status.clear();
        }
        KeyCode::Char(c @ '1'..='4') => {
            if let Some(row) = &selected {
                let ch = Channel(c as usize - '1' as usize);
                app.status = match assign(row, ch) {
                    Ok(()) => format!("Assigned {} → CH{}", row.app(), ch.human()),
                    Err(e) => format!("Assign failed: {e}"),
                };
            }
            app.snap = snapshot();
        }
        KeyCode::Char('u') => {
            if let Some(row) = &selected {
                app.status = match unassign(row) {
                    Ok(()) => format!("Unassigned {}", row.app()),
                    Err(e) => format!("Unassign failed: {e}"),
                };
            }
            app.snap = snapshot();
        }
        KeyCode::Char('r') => {
            app.snap = snapshot();
            app.status.clear();
        }
        _ => {}
    }
}

fn handle_settings_key(app: &mut App, code: KeyCode) {
    let field = app.settings.selected().unwrap_or(0);
    let is_name = (NAME_FIELD_BASE..REFRESH_FIELD).contains(&field);
    let is_button = field == REFRESH_FIELD;
    // Numeric (adjustable) rows are everything that isn't a name or the button.
    let is_numeric = !is_name && !is_button;
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.settings.select(Some(field.saturating_sub(1)));
            app.status.clear();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.settings
                .select(Some((field + 1).min(SETTINGS_FIELDS - 1)));
            app.status.clear();
        }
        // Numeric fields adjust with ←/→; name fields are typed (Enter to start).
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') if is_numeric => {
            save_adjust(app, field, -1)
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') | KeyCode::Char('=')
            if is_numeric =>
        {
            save_adjust(app, field, 1)
        }
        KeyCode::Enter if is_name => {
            app.editing = true;
            app.status = "Type a name · Backspace deletes · Enter/Esc done".to_string();
        }
        KeyCode::Enter if is_button => request_background_reload(app),
        _ => {}
    }
}

/// Signal the daemon to reload the backdrop image: bump the generation counter and
/// save `display.json`. The daemon notices the change within ~1s and re-reads the
/// `background.{png,jpg,jpeg}` from the config dir.
fn request_background_reload(app: &mut App) {
    app.display.background_generation = app.display.background_generation.wrapping_add(1);
    app.status = match app.display.save() {
        Ok(()) => match crate::state::background_path() {
            Some(_) => "Reloading background — the daemon applies it within ~1s.".to_string(),
            None => {
                "No background.{png,jpg,jpeg} in the config dir — using solid colour.".to_string()
            }
        },
        Err(e) => format!("Save failed: {e}"),
    };
}

/// Text entry into the selected channel-name field.
fn handle_edit_key(app: &mut App, code: KeyCode) {
    let Some(i) = app
        .settings
        .selected()
        .and_then(|f| f.checked_sub(NAME_FIELD_BASE))
    else {
        app.editing = false;
        return;
    };
    match code {
        KeyCode::Enter | KeyCode::Esc => {
            app.editing = false;
            app.status = "Saved — the daemon applies it within ~1s.".to_string();
        }
        KeyCode::Backspace => {
            app.display.channel_names[i].pop();
            let _ = app.display.save();
        }
        KeyCode::Char(c)
            if (c.is_ascii_graphic() || c == ' ')
                && app.display.channel_names[i].chars().count() < NAME_MAX =>
        {
            app.display.channel_names[i].push(c);
            let _ = app.display.save();
        }
        _ => {}
    }
}

fn save_adjust(app: &mut App, field: usize, dir: i64) {
    adjust(&mut app.display, field, dir);
    app.status = match app.display.save() {
        Ok(()) => "Saved — the daemon applies it within ~1s.".to_string(),
        Err(e) => format!("Save failed: {e}"),
    };
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(f.area());

    let tab = match app.page {
        Page::Routing => 0,
        Page::Settings => 1,
    };
    let tabs = Tabs::new(vec![" Routing ", " Settings "])
        .select(tab)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .divider("");
    f.render_widget(tabs, chunks[0]);

    match app.page {
        Page::Routing => draw_routing(f, chunks[1], app),
        Page::Settings => draw_settings(f, chunks[1], app),
    }
}

fn draw_routing(f: &mut Frame, area: Rect, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(11), // channel panels
        Constraint::Min(3),     // selectable stream list
        Constraint::Length(1),  // help / status
    ])
    .split(area);

    // Four channel panels, each listing the apps routed to it.
    let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(chunks[0]);
    for (i, col) in cols.iter().enumerate() {
        let ch = Channel(i);
        let vol = app.snap.levels.volumes[i];
        let name = &app.display.channel_names[i];
        let label = if name.is_empty() {
            format!("CH{}", ch.human())
        } else {
            format!("CH{} {name}", ch.human())
        };
        let title = if app.snap.levels.mutes[i] {
            format!(" {label}  {vol}%  MUTE ")
        } else {
            format!(" {label}  {vol}% ")
        };

        let mut lines: Vec<Line> = Vec::new();
        for row in &app.snap.rows {
            if row.channel() == Some(ch) {
                match row {
                    Row::Live { label, .. } => lines.push(Line::from(label.clone())),
                    Row::Idle { app, .. } => lines.push(Line::styled(
                        format!("{app} (idle)"),
                        Style::new().fg(Color::DarkGray),
                    )),
                    Row::Mic { label, .. } => lines.push(Line::styled(
                        format!("mic: {label}"),
                        Style::new().fg(ACCENT[i]),
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
    let items: Vec<ListItem> = app
        .snap
        .rows
        .iter()
        .map(|r| ListItem::new(r.display()))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" Streams & mics — select, then 1-4 to assign "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut app.routing);

    let help = if app.status.is_empty() {
        "↑/↓ select · 1-4 assign · u unassign · r refresh · Tab settings · q quit".to_string()
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(Line::styled(help, Style::new().fg(Color::Gray))),
        chunks[2],
    );
}

/// A "Label            value" row for the settings list.
fn field_item<'a>(name: &str, value: String) -> ListItem<'a> {
    ListItem::new(format!("  {name:<18}{value}"))
}

fn draw_settings(f: &mut Frame, area: Rect, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area);

    let d = &app.display;
    let sel = app.settings.selected().unwrap_or(0);
    let mut items: Vec<ListItem> = vec![
        field_item("Dim after", format!("{} min", d.dim_after_secs / 60)),
        field_item("Full brightness", format!("{}%", d.full_brightness)),
        field_item("Dim brightness", format!("{}%", d.dim_brightness)),
    ];
    for i in 0..4 {
        let name = &d.channel_names[i];
        let mut val = if name.is_empty() {
            format!("(CH {})", i + 1)
        } else {
            name.clone()
        };
        if app.editing && sel == NAME_FIELD_BASE + i {
            val.push('▏'); // cursor
        }
        items.push(field_item(&format!("Channel {} name", i + 1), val));
    }
    // Action row: reload the panel backdrop from disk on demand.
    let bg_hint = match crate::state::background_path() {
        Some(p) => p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("set")
            .to_string(),
        None => "no file found".to_string(),
    };
    items.push(field_item("Reload background", format!("⏎  ({bg_hint})")));
    let list = List::new(items)
        .block(Block::bordered().title(" Panel display "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[0], &mut app.settings);

    let help = if !app.status.is_empty() {
        app.status.clone()
    } else if sel == REFRESH_FIELD {
        "↑/↓ select · Enter reload background · Tab routing · q quit".to_string()
    } else if sel >= NAME_FIELD_BASE {
        "↑/↓ select · Enter rename · Tab routing · q quit".to_string()
    } else {
        "↑/↓ select · ←/→ (or -/+) adjust · Tab routing · q quit".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::styled(help, Style::new().fg(Color::Gray))),
        chunks[1],
    );
}
