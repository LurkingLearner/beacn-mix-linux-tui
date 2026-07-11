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
use crate::routing::{self, Row};
use crate::state::{DisplayConfig, Levels, OutputConfig};
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
use std::io;
use std::time::Duration;

/// Per-channel accent, matching the on-device panel (`src/screen.rs`).
const ACCENT: [Color; 4] = [
    Color::Rgb(86, 156, 255),  // blue
    Color::Rgb(95, 205, 140),  // green
    Color::Rgb(214, 162, 86),  // amber
    Color::Rgb(190, 130, 240), // violet
];

/// Settings rows: dim-after, full brightness, dim brightness, output device,
/// 4 channel names, background-image, then its scrim toggle.
const SETTINGS_FIELDS: usize = 10;
/// Index of the output-device row (cycled with ←/→).
const OUTPUT_FIELD: usize = 3;
/// Index of the first channel-name row.
const NAME_FIELD_BASE: usize = 4;
/// Index of the background-image row (cycle files with ←/→, Enter to reload).
const BACKGROUND_FIELD: usize = NAME_FIELD_BASE + 4;
/// Index of the selected background's scrim toggle.
const SCRIM_FIELD: usize = BACKGROUND_FIELD + 1;
/// Max length of a custom channel name.
const NAME_MAX: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Routing,
    Settings,
}

/// Which region of the Routing page has the keyboard focus. The four channel
/// panels (which manage *assigned* items) and the Unassigned list sit in one
/// left-to-right ring: `CH1 · CH2 · CH3 · CH4 · Unassigned`. `←/→` step through
/// it; `↑/↓` move the selection within the focused region.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Channel(usize),
    Unassigned,
}

impl Focus {
    /// Step `dir` (±1) through the five focus positions, wrapping.
    fn step(self, dir: i64) -> Focus {
        let cur = match self {
            Focus::Channel(i) => i as i64,
            Focus::Unassigned => 4,
        };
        match (cur + dir).rem_euclid(5) {
            4 => Focus::Unassigned,
            n => Focus::Channel(n as usize),
        }
    }
}

/// One rendered line of the Unassigned list: either a bucket header (skipped
/// during selection) or a selectable item indexing into `Snapshot::rows`.
enum UnEntry {
    Header(&'static str),
    Item(usize),
}

struct Snapshot {
    rows: Vec<Row>,
    levels: Levels,
}

/// Read the current PipeWire streams + bindings + levels into a fresh snapshot.
/// The row-building itself is shared with the GUI (see `src/routing.rs`).
fn snapshot() -> Snapshot {
    Snapshot {
        rows: routing::rows(),
        levels: Levels::load().unwrap_or_default(),
    }
}

/// Indices into `snap.rows` of the rows currently assigned to channel `ch`
/// (live streams on it, idle bound apps, and bound mics), in snapshot order.
fn channel_rows(snap: &Snapshot, ch: usize) -> Vec<usize> {
    snap.rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.channel() == Some(Channel(ch)))
        .map(|(i, _)| i)
        .collect()
}

/// Build the Unassigned list: every row with no channel, bucketed `Apps`
/// (playback streams) then `Mics` (capture devices), each alphabetical and
/// narrowed by a case-insensitive substring `filter`. A bucket's header is
/// omitted when it has no matching items. `Idle` rows are always assigned, so
/// they never appear here.
fn unassigned_entries(snap: &Snapshot, filter: &str) -> Vec<UnEntry> {
    let needle = filter.to_lowercase();
    let matches = |label: &str| needle.is_empty() || label.to_lowercase().contains(&needle);

    let mut apps: Vec<(String, usize)> = Vec::new();
    let mut mics: Vec<(String, usize)> = Vec::new();
    for (i, row) in snap.rows.iter().enumerate() {
        if row.channel().is_some() {
            continue;
        }
        match row {
            Row::Live { label, .. } if matches(label) => apps.push((label.clone(), i)),
            Row::Mic { label, .. } if matches(label) => mics.push((label.clone(), i)),
            _ => {}
        }
    }
    let by_label =
        |a: &(String, usize), b: &(String, usize)| a.0.to_lowercase().cmp(&b.0.to_lowercase());
    apps.sort_by(by_label);
    mics.sort_by(by_label);

    let mut entries = Vec::new();
    if !apps.is_empty() {
        entries.push(UnEntry::Header("Apps"));
        entries.extend(apps.into_iter().map(|(_, i)| UnEntry::Item(i)));
    }
    if !mics.is_empty() {
        entries.push(UnEntry::Header("Mics"));
        entries.extend(mics.into_iter().map(|(_, i)| UnEntry::Item(i)));
    }
    entries
}

/// The combined-list index of the next selectable `Item` from `from`, stepping
/// `dir` (-1/0/+1) and skipping `Header` entries. `dir == 0` snaps a stale or
/// header selection onto the nearest item; returns `None` when there are no
/// items.
fn next_item(entries: &[UnEntry], from: Option<usize>, dir: i64) -> Option<usize> {
    let items: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, UnEntry::Item(_)))
        .map(|(i, _)| i)
        .collect();
    let last = *items.last()?;
    match from {
        None => Some(items[0]),
        Some(cur) => match items.iter().position(|&i| i == cur) {
            Some(p) => {
                let np = (p as i64 + dir).clamp(0, items.len() as i64 - 1) as usize;
                Some(items[np])
            }
            // `cur` landed on a header or fell out of range: snap to the first
            // item at or after it (else the last item).
            None => Some(items.into_iter().find(|&i| i >= cur).unwrap_or(last)),
        },
    }
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
    /// Which Routing region has focus (a channel panel or the Unassigned list).
    focus: Focus,
    /// Selection within each channel panel (index into that channel's rows).
    chan_state: [ListState; 4],
    /// Selection within the Unassigned list (index into its combined entries).
    unas_state: ListState,
    /// Live substring filter applied to the Unassigned list.
    filter: String,
    /// True while typing into the Unassigned filter.
    filtering: bool,
    settings: ListState,
    display: DisplayConfig,
    /// Chosen output device the channels feed (None = system default).
    output: OutputConfig,
    /// Available output devices, for the output-device cycle row.
    outputs: Vec<pw::Output>,
    /// Image file names under the config `backgrounds/` dir, for the background
    /// cycle row. Refreshed live so newly-dropped files appear.
    backgrounds: Vec<String>,
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

    let mut settings = ListState::default();
    settings.select(Some(0));
    let mut app = App {
        page: Page::Routing,
        snap: snapshot(),
        focus: Focus::Channel(0),
        chan_state: std::array::from_fn(|_| ListState::default()),
        unas_state: ListState::default(),
        filter: String::new(),
        filtering: false,
        settings,
        display: DisplayConfig::load().unwrap_or_default(),
        output: OutputConfig::load().unwrap_or_default(),
        outputs: pw::list_outputs().unwrap_or_default(),
        backgrounds: crate::state::list_backgrounds(),
        editing: false,
        status: String::new(),
    };

    loop {
        // Keep selections within bounds as streams come and go.
        clamp_channels(&mut app);
        normalize_unas(&mut app);
        clamp_selection(&mut app.settings, SETTINGS_FIELDS);

        terminal.draw(|f| draw(f, &mut app))?;

        // Block briefly for input; on timeout, refresh the routing snapshot —
        // but not mid-filter, so the list doesn't shift under the cursor.
        if !event::poll(Duration::from_millis(750))? {
            if app.page == Page::Routing && !app.filtering {
                app.snap = snapshot();
            } else if app.page == Page::Settings {
                // Pick up images dropped into backgrounds/ while the TUI is open.
                app.backgrounds = crate::state::list_backgrounds();
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

        // While filtering, all keys go to the Unassigned filter field.
        if app.filtering {
            handle_filter_key(&mut app, key.code);
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

/// Index into `snap.rows` of the row selected in the currently-focused region,
/// or `None` (empty pane, or a header is "selected").
fn selected_row_index(app: &App) -> Option<usize> {
    match app.focus {
        Focus::Channel(c) => {
            let rows = channel_rows(&app.snap, c);
            app.chan_state[c]
                .selected()
                .and_then(|s| rows.get(s).copied())
        }
        Focus::Unassigned => {
            let entries = unassigned_entries(&app.snap, &app.filter);
            match app.unas_state.selected().and_then(|s| entries.get(s)) {
                Some(UnEntry::Item(i)) => Some(*i),
                _ => None,
            }
        }
    }
}

/// Move the selection within the focused region by `dir` (±1), skipping headers
/// in the Unassigned list and clamping within a channel's rows.
fn move_selection(app: &mut App, dir: i64) {
    match app.focus {
        Focus::Channel(c) => {
            let len = channel_rows(&app.snap, c).len();
            if len == 0 {
                app.chan_state[c].select(None);
            } else {
                let cur = app.chan_state[c].selected().unwrap_or(0) as i64;
                let next = (cur + dir).clamp(0, len as i64 - 1) as usize;
                app.chan_state[c].select(Some(next));
            }
        }
        Focus::Unassigned => {
            let entries = unassigned_entries(&app.snap, &app.filter);
            let next = next_item(&entries, app.unas_state.selected(), dir);
            app.unas_state.select(next);
        }
    }
}

/// Re-point the channel selections within their (possibly changed) row counts.
fn clamp_channels(app: &mut App) {
    for c in 0..4 {
        let len = channel_rows(&app.snap, c).len();
        if len == 0 {
            app.chan_state[c].select(None);
        } else {
            let cur = app.chan_state[c].selected().unwrap_or(0).min(len - 1);
            app.chan_state[c].select(Some(cur));
        }
    }
}

/// Re-point the Unassigned selection onto a valid item after the list changes
/// (snapshot refresh, filter edit, assign/unassign).
fn normalize_unas(app: &mut App) {
    let entries = unassigned_entries(&app.snap, &app.filter);
    let sel = next_item(&entries, app.unas_state.selected(), 0);
    app.unas_state.select(sel);
}

fn handle_routing_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Left | KeyCode::Char('h') => {
            app.focus = app.focus.step(-1);
            app.status.clear();
        }
        KeyCode::Right | KeyCode::Char('l') => {
            app.focus = app.focus.step(1);
            app.status.clear();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_selection(app, -1);
            app.status.clear();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_selection(app, 1);
            app.status.clear();
        }
        // From the Unassigned pane, 1-4 assigns the selected source; from a
        // channel pane, it *moves* the selected item to another channel.
        KeyCode::Char(c @ '1'..='4') => {
            let ch = Channel(c as usize - '1' as usize);
            if let Some(row) = selected_row_index(app).map(|i| app.snap.rows[i].clone()) {
                app.status = match routing::assign(&row, ch) {
                    Ok(()) => format!("Assigned {} → CH{}", row.app(), ch.human()),
                    Err(e) => format!("Assign failed: {e}"),
                };
            }
            app.snap = snapshot();
        }
        KeyCode::Char('u') => {
            if let Some(row) = selected_row_index(app).map(|i| app.snap.rows[i].clone()) {
                app.status = match routing::unassign(&row) {
                    Ok(()) => format!("Unassigned {}", row.app()),
                    Err(e) => format!("Unassign failed: {e}"),
                };
            }
            app.snap = snapshot();
        }
        KeyCode::Char('/') => {
            app.focus = Focus::Unassigned;
            app.filtering = true;
            app.status = "Filter: type to narrow · Enter keep · Esc clear".to_string();
        }
        KeyCode::Char('r') => {
            app.snap = snapshot();
            app.status.clear();
        }
        _ => {}
    }
}

/// Text entry into the Unassigned filter (active while `app.filtering`).
fn handle_filter_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Enter => {
            app.filtering = false;
            app.status = "Filter applied · Esc clears it".to_string();
        }
        KeyCode::Esc => {
            app.filter.clear();
            app.filtering = false;
            app.status.clear();
        }
        KeyCode::Backspace => {
            app.filter.pop();
            normalize_unas(app);
        }
        KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => {
            app.filter.push(c);
            normalize_unas(app);
        }
        _ => {}
    }
}

fn handle_settings_key(app: &mut App, code: KeyCode) {
    let field = app.settings.selected().unwrap_or(0);
    let is_name = (NAME_FIELD_BASE..BACKGROUND_FIELD).contains(&field);
    let is_background = field == BACKGROUND_FIELD;
    let is_scrim = field == SCRIM_FIELD;
    let is_output = field == OUTPUT_FIELD;
    // Numeric (adjustable) rows are everything that isn't a name, output,
    // background, or scrim row.
    let is_numeric = !is_name && !is_background && !is_scrim && !is_output;
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
        // Output device cycles through the available sinks with ←/→.
        KeyCode::Left | KeyCode::Char('h') if is_output => cycle_output(app, -1),
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter if is_output => cycle_output(app, 1),
        // Background image cycles through backgrounds/ with ←/→; Enter reloads
        // the chosen file from disk (for an overwrite-in-place refresh).
        KeyCode::Left | KeyCode::Char('h') if is_background => cycle_background(app, -1),
        KeyCode::Right | KeyCode::Char('l') if is_background => cycle_background(app, 1),
        KeyCode::Enter if is_background => request_background_reload(app),
        KeyCode::Left
        | KeyCode::Char('h')
        | KeyCode::Right
        | KeyCode::Char('l')
        | KeyCode::Enter
            if is_scrim =>
        {
            toggle_background_scrim(app)
        }
        KeyCode::Enter if is_name => {
            app.editing = true;
            app.status = "Type a name · Backspace deletes · Enter/Esc done".to_string();
        }
        _ => {}
    }
}

/// Step the chosen backdrop through `[off] + backgrounds/` by `dir` (wrapping)
/// and persist it; the daemon swaps the panel image within ~1s. `off` (index 0)
/// is the solid colour; files follow in sorted order.
fn cycle_background(app: &mut App, dir: i64) {
    let files = &app.backgrounds;
    let n = files.len() as i64 + 1; // +1 for the "off" entry at index 0
    let cur = match &app.display.background_file {
        None => 0,
        Some(name) => files
            .iter()
            .position(|f| f == name)
            .map(|i| i as i64 + 1)
            .unwrap_or(0),
    };
    let next = (cur + dir).rem_euclid(n);
    app.display.background_file = if next == 0 {
        None
    } else {
        files.get((next - 1) as usize).cloned()
    };
    app.status = match app.display.save() {
        Ok(()) => {
            let label = app.display.background_file.as_deref().unwrap_or("(off)");
            format!("Background → {label} (daemon applies within ~1s).")
        }
        Err(e) => format!("Save failed: {e}"),
    };
}

/// Signal the daemon to reload the *currently chosen* backdrop from disk: bump the
/// generation counter and save `display.json`. Useful when the selected file was
/// overwritten in place (cycling already triggers a reload when the file changes).
fn request_background_reload(app: &mut App) {
    app.display.background_generation = app.display.background_generation.wrapping_add(1);
    app.status = match app.display.save() {
        Ok(()) => match crate::state::background_path_for(&app.display) {
            Some(_) => "Reloading background — the daemon applies it within ~1s.".to_string(),
            None => "No background selected — using solid colour.".to_string(),
        },
        Err(e) => format!("Save failed: {e}"),
    };
}

/// Persist the backdrop scrim preference. The daemon reloads the current image
/// when this value changes, so the panel updates without a restart.
fn toggle_background_scrim(app: &mut App) {
    app.display.background_scrim = !app.display.background_scrim;
    app.status = match app.display.save() {
        Ok(()) => format!(
            "Background scrim {} (daemon applies within ~1s).",
            if app.display.background_scrim {
                "enabled"
            } else {
                "disabled"
            }
        ),
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

/// Step the chosen output device by `dir` (wrapping) and persist it; the daemon
/// repoints all four channel loopbacks onto it within ~1s.
fn cycle_output(app: &mut App, dir: i64) {
    if app.outputs.is_empty() {
        app.status = "No output devices found.".to_string();
        return;
    }
    let n = app.outputs.len() as i64;
    let cur = app
        .output
        .sink
        .as_deref()
        .and_then(|s| app.outputs.iter().position(|o| o.name == s));
    let next = match cur {
        Some(i) => (i as i64 + dir).rem_euclid(n) as usize,
        // Nothing chosen yet: ← lands on the last device, → on the first.
        None if dir < 0 => (n - 1) as usize,
        None => 0,
    };
    app.output.sink = Some(app.outputs[next].name.clone());
    app.status = match app.output.save() {
        Ok(()) => format!(
            "Output → {} (daemon switches within ~1s).",
            app.outputs[next].label()
        ),
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

    // Four channel panels, each a selectable list of the items routed to it.
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

        let mut items: Vec<ListItem> = Vec::new();
        for row in &app.snap.rows {
            if row.channel() == Some(ch) {
                items.push(match row {
                    Row::Live { label, .. } => ListItem::new(label.clone()),
                    Row::Idle { app, .. } => ListItem::new(Line::styled(
                        format!("{app} (idle)"),
                        Style::new().fg(Color::DarkGray),
                    )),
                    Row::Mic { label, .. } => ListItem::new(Line::styled(
                        format!("mic: {label}"),
                        Style::new().fg(ACCENT[i]),
                    )),
                });
            }
        }
        let empty = items.is_empty();
        if empty {
            items.push(ListItem::new(Line::styled(
                "—",
                Style::new().fg(Color::DarkGray),
            )));
        }

        let focused = app.focus == Focus::Channel(i);
        let border_style = if focused {
            Style::new().fg(ACCENT[i]).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(ACCENT[i]).add_modifier(Modifier::DIM)
        };
        let mut list =
            List::new(items).block(Block::bordered().title(title).border_style(border_style));
        if focused && !empty {
            list = list
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▶ ");
        }
        f.render_stateful_widget(list, *col, &mut app.chan_state[i]);
    }

    // The Unassigned list: bucketed Apps / Mics, narrowed by the filter.
    let entries = unassigned_entries(&app.snap, &app.filter);
    let items: Vec<ListItem> = entries
        .iter()
        .map(|e| match e {
            UnEntry::Header(h) => ListItem::new(Line::styled(
                (*h).to_string(),
                Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD),
            )),
            UnEntry::Item(idx) => {
                let row = &app.snap.rows[*idx];
                match row {
                    Row::Mic { label, .. } => ListItem::new(format!("  mic: {label}")),
                    _ => ListItem::new(format!("  {}", row.name())),
                }
            }
        })
        .collect();
    let title = if app.filtering || !app.filter.is_empty() {
        let cursor = if app.filtering { "▏" } else { "" };
        format!(" Unassigned  filter: {}{} ", app.filter, cursor)
    } else {
        " Unassigned — select, then 1-4 to assign ".to_string()
    };
    let focused = app.focus == Focus::Unassigned;
    let border_style = if focused {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    };
    let mut list =
        List::new(items).block(Block::bordered().title(title).border_style(border_style));
    if focused {
        list = list
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ ");
    }
    f.render_stateful_widget(list, chunks[1], &mut app.unas_state);

    let help = if !app.status.is_empty() {
        app.status.clone()
    } else if app.filtering {
        "type to filter · Enter keep · Esc clear".to_string()
    } else {
        "←/→ pane · ↑/↓ select · 1-4 assign/move · u unassign · / filter · r refresh · Tab settings · q quit".to_string()
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
    // Friendly label for the currently-chosen output device.
    let output_label = match &app.output.sink {
        Some(name) => app
            .outputs
            .iter()
            .find(|o| &o.name == name)
            .map(|o| o.label().to_string())
            .unwrap_or_else(|| format!("{name} (not present)")),
        None => "(system default)".to_string(),
    };
    let mut items: Vec<ListItem> = vec![
        field_item("Dim after", format!("{} min", d.dim_after_secs / 60)),
        field_item("Full brightness", format!("{}%", d.full_brightness)),
        field_item("Dim brightness", format!("{}%", d.dim_brightness)),
        field_item("Output device", format!("◂ {output_label} ▸")),
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
    // Background row: cycle through images in the config backgrounds/ dir.
    let bg_label = match &d.background_file {
        Some(name) => name.clone(),
        None if app.backgrounds.is_empty() => "(no files in backgrounds/)".to_string(),
        None => "(off)".to_string(),
    };
    items.push(field_item("Background", format!("◂ {bg_label} ▸")));
    items.push(field_item(
        "Background scrim",
        if d.background_scrim {
            "on".to_string()
        } else {
            "off".to_string()
        },
    ));
    let list = List::new(items)
        .block(Block::bordered().title(" Panel display "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[0], &mut app.settings);

    let help = if !app.status.is_empty() {
        app.status.clone()
    } else if sel == BACKGROUND_FIELD {
        "↑/↓ select · ←/→ change background · Enter reload · Tab routing · q quit".to_string()
    } else if sel == OUTPUT_FIELD {
        "↑/↓ select · ←/→ change output device · Tab routing · q quit".to_string()
    } else if sel == SCRIM_FIELD {
        "↑/↓ select · ←/→ or Enter toggle scrim · Tab routing · q quit".to_string()
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

#[cfg(test)]
mod tests {
    //! Cover the pure list-partitioning logic that decides what shows up in the
    //! Unassigned pane and in what order — no PipeWire or terminal needed.

    use super::*;

    fn snap(rows: Vec<Row>) -> Snapshot {
        Snapshot {
            rows,
            levels: Levels::default(),
        }
    }

    fn live(app: &str, channel: Option<Channel>) -> Row {
        Row::Live {
            index: 0,
            app: app.to_string(),
            label: app.to_string(),
            channel,
        }
    }

    fn mic(label: &str, channel: Option<Channel>) -> Row {
        Row::Mic {
            name: format!("node.{label}"),
            label: label.to_string(),
            channel,
            present: true,
        }
    }

    /// Render the entries to a readable form: headers as `# Name`, items as the
    /// row's display name.
    fn labels(entries: &[UnEntry], rows: &[Row]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                UnEntry::Header(h) => format!("# {h}"),
                UnEntry::Item(i) => rows[*i].name().to_string(),
            })
            .collect()
    }

    #[test]
    fn excludes_assigned_rows_and_buckets_apps_then_mics() {
        let s = snap(vec![
            live("Spotify", None),
            live("Firefox", Some(Channel(0))), // assigned → excluded
            Row::Idle {
                app: "Discord".into(),
                channel: Channel(1),
            }, // idle is always assigned → excluded
            mic("Webcam", None),
            mic("Yeti", Some(Channel(2))), // assigned → excluded
            live("Chrome", None),
        ]);
        // Apps before Mics, each alphabetical; assigned/idle rows absent.
        assert_eq!(
            labels(&unassigned_entries(&s, ""), &s.rows),
            vec!["# Apps", "Chrome", "Spotify", "# Mics", "Webcam"]
        );
    }

    #[test]
    fn filter_is_case_insensitive_and_drops_empty_group_headers() {
        let s = snap(vec![
            live("Spotify", None),
            live("Chrome", None),
            mic("Webcam", None),
        ]);
        // "CH" matches only Chrome → Apps header, no Mics header.
        assert_eq!(
            labels(&unassigned_entries(&s, "CH"), &s.rows),
            vec!["# Apps", "Chrome"]
        );
        // "web" matches only the mic → Mics header, no Apps header.
        assert_eq!(
            labels(&unassigned_entries(&s, "web"), &s.rows),
            vec!["# Mics", "Webcam"]
        );
        // No matches → empty list.
        assert!(unassigned_entries(&s, "zzz").is_empty());
    }

    #[test]
    fn next_item_skips_headers_and_clamps() {
        let s = snap(vec![
            live("Spotify", None),
            live("Chrome", None),
            mic("Webcam", None),
        ]);
        let e = unassigned_entries(&s, "");
        // Entries: [#Apps, Chrome, Spotify, #Mics, Webcam] at indices 0..=4.
        // No selection → first item (index 1, "Chrome").
        assert_eq!(next_item(&e, None, 1), Some(1));
        // From the last app item, stepping down skips the Mics header onto Webcam.
        assert_eq!(next_item(&e, Some(2), 1), Some(4));
        // Stepping up from the first item clamps in place.
        assert_eq!(next_item(&e, Some(1), -1), Some(1));
        // A stale selection on a header snaps to the next item.
        assert_eq!(next_item(&e, Some(3), 0), Some(4));
    }
}
