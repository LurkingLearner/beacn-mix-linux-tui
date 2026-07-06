//! GUI configuration window (egui/eframe). A mouse-driven alternative to the
//! terminal TUI. Reads/writes the same JSON files the daemon already reacts to,
//! so it runs alongside `run` just like `tui` does — no IPC, no daemon changes.
#![cfg(feature = "gui")]

use crate::mix::Channel;
use crate::pw;
use crate::routing::{self, Row};
use crate::state::{DisplayConfig, Levels, OutputConfig};
use anyhow::Result;
use eframe::egui;
use std::time::{Duration, Instant};

// ── accent colours (same as tui.rs and screen.rs) ──────────────────────────

const ACCENT: [egui::Color32; 4] = [
    egui::Color32::from_rgb(86, 156, 255),  // blue
    egui::Color32::from_rgb(95, 205, 140),  // green
    egui::Color32::from_rgb(214, 162, 86),  // amber
    egui::Color32::from_rgb(190, 130, 240), // violet
];

/// How often to re-poll PipeWire / the config files while the window is
/// focused (same cadence as the TUI).
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Vertical space reserved below the tab content for the status bar.
const STATUS_BAR_RESERVE: f32 = 30.0;

// ── tabs ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Routing,
    Settings,
}

// ── the eframe App ─────────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 600.0])
            .with_title("Beacn Mix — Routing"),
        ..Default::default()
    };

    eframe::run_native(
        "Beacn Mix — Routing",
        options,
        Box::new(|_cc| Ok(Box::new(BeacnGui::default()))),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))?;

    Ok(())
}

// ── row display helpers ─────────────────────────────────────────────────────

/// The list label for a row, with the idle / disconnected annotations the GUI
/// shows inline (the shared [`Row`] keeps the raw name).
fn row_label(row: &Row) -> String {
    match row {
        Row::Live { label, .. } => label.clone(),
        Row::Idle { app, .. } => format!("{app} (idle)"),
        Row::Mic { label, present, .. } => {
            if *present {
                label.clone()
            } else {
                format!("{label} (disconnected)")
            }
        }
    }
}

// ── the app state ──────────────────────────────────────────────────────────

struct BeacnGui {
    tab: Tab,
    /// Snapshot of streams + mics + bindings (shared with the TUI).
    rows: Vec<Row>,
    levels: Levels,
    display: DisplayConfig,
    output: OutputConfig,
    outputs: Vec<pw::Output>,
    backgrounds: Vec<String>,
    filter: String,
    status: String,
    last_refresh: Instant,
    /// True while a channel-name text field held focus last frame. Blocks the
    /// periodic `display.json` reload from stomping the in-progress edit.
    name_edit_focused: bool,
}

impl Default for BeacnGui {
    fn default() -> Self {
        Self {
            tab: Tab::Routing,
            rows: routing::rows(),
            levels: Levels::load().unwrap_or_default(),
            display: DisplayConfig::load().unwrap_or_default(),
            output: OutputConfig::load().unwrap_or_default(),
            outputs: pw::list_outputs().unwrap_or_default(),
            backgrounds: crate::state::list_backgrounds(),
            filter: String::new(),
            status: String::new(),
            last_refresh: Instant::now(),
            name_edit_focused: false,
        }
    }
}

impl BeacnGui {
    /// Full refresh: re-snapshot PipeWire (spawns pactl) + re-read the config
    /// files. Only run on the Routing tab / after a change — never on a timer
    /// while the window is unfocused.
    fn refresh(&mut self) {
        self.rows = routing::rows();
        self.levels = Levels::load().unwrap_or_default();
        self.outputs = pw::list_outputs().unwrap_or_default();
        self.reload_config_files();
        self.last_refresh = Instant::now();
    }

    /// Cheap refresh for the Settings tab: config-file re-reads only, no
    /// subprocesses. Picks up external edits (e.g. from the TUI) so the GUI's
    /// next save doesn't clobber them.
    fn refresh_configs(&mut self) {
        self.reload_config_files();
        self.last_refresh = Instant::now();
    }

    fn reload_config_files(&mut self) {
        self.output = OutputConfig::load().unwrap_or_default();
        self.backgrounds = crate::state::list_backgrounds();
        // Don't reload the display config out from under an in-progress
        // channel-name edit — the text field is bound to this struct.
        if !self.name_edit_focused {
            self.display = DisplayConfig::load().unwrap_or_default();
        }
    }
}

// ── eframe::App impl ───────────────────────────────────────────────────────

impl eframe::App for BeacnGui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Poll only while the window has focus, and only spawn the pactl
        // subprocesses when the Routing tab (which shows their output) is up;
        // the Settings poll is just config-file re-reads.
        let focused = ui.ctx().input(|i| i.focused);
        if focused && self.last_refresh.elapsed() >= POLL_INTERVAL {
            match self.tab {
                Tab::Routing => self.refresh(),
                Tab::Settings => self.refresh_configs(),
            }
        }

        // Use the entire available area with a vertical layout.
        ui.vertical(|ui| {
            // ── Tab bar ──
            let prev_tab = self.tab;
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Routing, "Routing");
                ui.selectable_value(&mut self.tab, Tab::Settings, "⚙ Settings");
            });
            if self.tab != prev_tab {
                // Leaving Settings commits any in-progress channel-name edit
                // (its text field can't report `lost_focus` once the page
                // stops rendering).
                if prev_tab == Tab::Settings {
                    let _ = self.display.save();
                    self.name_edit_focused = false;
                }
                // Fresh streams/outputs for the incoming page.
                self.refresh();
            }
            ui.separator();

            // ── Main content (leave room for the status bar below) ──
            let body_height = (ui.available_height() - STATUS_BAR_RESERVE).max(60.0);
            match self.tab {
                Tab::Routing => {
                    let data = routing_data(self);
                    routing_ui(ui, self, &data, body_height);
                }
                Tab::Settings => {
                    settings_ui(ui, self, body_height);
                }
            }

            // ── Status bar ──
            ui.separator();
            ui.label(
                egui::RichText::new(&self.status)
                    .color(egui::Color32::GRAY)
                    .small(),
            );
        });

        // Schedule the next poll only when one will actually run.
        if focused {
            ui.ctx().request_repaint_after(POLL_INTERVAL);
        }
    }
}

// ── Routing data: precompute what the UI needs (avoid borrow issues) ───────

struct RoutingData {
    /// (row index, display label, idle) per channel, sorted by label.
    channel_items: [Vec<(usize, String, bool)>; 4],
    /// Unassigned streams as (row index, display label), sorted by label.
    unassigned_streams: Vec<(usize, String)>,
    /// Unassigned mics as (row index, display label), sorted by label.
    unassigned_mics: Vec<(usize, String)>,
}

/// A click collected during the frame; applied (and followed by a refresh)
/// only after all lists have finished rendering, so the row indices in
/// [`RoutingData`] stay valid for the whole frame.
enum Action {
    Assign(usize, Channel),
    Unassign(usize),
}

fn routing_data(s: &BeacnGui) -> RoutingData {
    let channel_items: [Vec<(usize, String, bool)>; 4] = std::array::from_fn(|i| {
        let ch = Channel(i);
        let mut items: Vec<(usize, String, bool)> = s
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.channel() == Some(ch))
            .map(|(idx, r)| (idx, row_label(r), matches!(r, Row::Idle { .. })))
            .collect();
        items.sort_by_key(|(_, label, _)| label.to_lowercase());
        items
    });

    let needle = s.filter.to_lowercase();
    let matches = |label: &str| needle.is_empty() || label.to_lowercase().contains(&needle);

    let mut unassigned_streams: Vec<(usize, String)> = Vec::new();
    let mut unassigned_mics: Vec<(usize, String)> = Vec::new();
    for (idx, row) in s.rows.iter().enumerate() {
        if row.channel().is_some() {
            continue;
        }
        let label = row_label(row);
        if !matches(&label) {
            continue;
        }
        match row {
            Row::Live { .. } => unassigned_streams.push((idx, label)),
            Row::Mic { .. } => unassigned_mics.push((idx, label)),
            Row::Idle { .. } => {} // idle rows are always assigned
        }
    }
    unassigned_streams.sort_by_key(|(_, label)| label.to_lowercase());
    unassigned_mics.sort_by_key(|(_, label)| label.to_lowercase());

    RoutingData {
        channel_items,
        unassigned_streams,
        unassigned_mics,
    }
}

// ── Routing tab ────────────────────────────────────────────────────────────

/// One list row: `label ... [buttons]` with the buttons pinned to the right
/// edge and the label truncating (with a hover tooltip for the full text) so
/// long stream titles can never push the buttons out of reach.
fn item_row(
    ui: &mut egui::Ui,
    label: &str,
    text: egui::RichText,
    buttons: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            buttons(ui);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(text).truncate())
                    .on_hover_text(label);
            });
        });
    });
}

fn routing_ui(ui: &mut egui::Ui, s: &mut BeacnGui, data: &RoutingData, max_height: f32) {
    let mut action: Option<Action> = None;

    // The whole tab body scrolls, so the filter field and the Apps/Mics lists
    // stay reachable at small window heights.
    egui::ScrollArea::vertical()
        .id_salt("routing_page")
        .max_height(max_height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // ── four channel columns ──
            ui.columns(4, |cols| {
                for i in 0..4 {
                    cols[i].vertical_centered(|ui| {
                        ui.heading(egui::RichText::new(format!("CH{}", i + 1)).color(ACCENT[i]));
                        let custom_name = &s.display.channel_names[i];
                        if !custom_name.is_empty() {
                            ui.label(egui::RichText::new(custom_name).small());
                        }
                        let vol = s.levels.volumes[i];
                        let muted = s.levels.mutes[i];
                        let vol_text = if muted {
                            egui::RichText::new(format!("{vol}%  MUTE"))
                                .color(egui::Color32::RED)
                                .strong()
                        } else {
                            egui::RichText::new(format!("{vol}%")).color(ACCENT[i])
                        };
                        ui.label(vol_text);

                        ui.separator();

                        if data.channel_items[i].is_empty() {
                            ui.label(
                                egui::RichText::new("—")
                                    .color(egui::Color32::DARK_GRAY)
                                    .italics(),
                            );
                        } else {
                            for (idx, label, idle) in &data.channel_items[i] {
                                let mut text = egui::RichText::new(label).size(12.0);
                                if *idle {
                                    text = text.color(egui::Color32::DARK_GRAY);
                                }
                                item_row(ui, label, text, |ui| {
                                    if ui.small_button("✖").clicked() {
                                        action = Some(Action::Unassign(*idx));
                                    }
                                });
                            }
                        }
                    });
                }
            });

            ui.add_space(8.0);
            ui.separator();

            // ── Filter field ──
            ui.horizontal(|ui| {
                ui.label("🔍");
                ui.add(
                    egui::TextEdit::singleline(&mut s.filter)
                        .hint_text("Filter apps and mics…")
                        .desired_width(300.0),
                );
                if ui.button("✖").clicked() {
                    s.filter.clear();
                }
            });

            ui.add_space(4.0);

            // ── Two side-by-side lists: Apps and Mics ──
            ui.columns(2, |cols| {
                cols[0].heading("Apps");
                assign_list(
                    &mut cols[0],
                    "apps_list",
                    &data.unassigned_streams,
                    &mut action,
                );

                cols[1].heading("Mics");
                assign_list(
                    &mut cols[1],
                    "mics_list",
                    &data.unassigned_mics,
                    &mut action,
                );
            });
        });

    // Apply any click after the lists have rendered: the indices in `data`
    // refer to `s.rows` as it was at the top of the frame, so mutate + refresh
    // only once nothing is iterating them anymore.
    if let Some(act) = action {
        match act {
            Action::Assign(idx, ch) => {
                let row = &s.rows[idx];
                let what = match row {
                    Row::Mic { .. } => format!("mic {}", row.app()),
                    _ => row.app().to_string(),
                };
                s.status = match routing::assign(row, ch) {
                    Ok(()) => format!("Assigned {what} → CH{}", ch.human()),
                    Err(e) => format!("Assign failed: {e}"),
                };
            }
            Action::Unassign(idx) => {
                let row = &s.rows[idx];
                let label = row_label(row);
                s.status = match routing::unassign(row) {
                    Ok(()) => format!("Unassigned {label}"),
                    Err(e) => format!("Unassign failed: {e}"),
                };
            }
        }
        s.refresh();
    }
}

/// A scrollable list of unassigned items, each with CH1..CH4 assign buttons
/// pinned to the right edge.
fn assign_list(
    ui: &mut egui::Ui,
    id_salt: &str,
    items: &[(usize, String)],
    action: &mut Option<Action>,
) {
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .max_height(300.0)
        .show(ui, |ui| {
            if items.is_empty() {
                ui.label(
                    egui::RichText::new("(none)")
                        .color(egui::Color32::DARK_GRAY)
                        .italics(),
                );
            }
            for (idx, label) in items {
                let text = egui::RichText::new(label).size(12.0);
                item_row(ui, label, text, |ui| {
                    // Right-to-left layout: add CH4 first so they read
                    // CH1..CH4 left-to-right.
                    for ch_i in (0..4).rev() {
                        if ui.small_button(format!("CH{}", ch_i + 1)).clicked() {
                            *action = Some(Action::Assign(*idx, Channel(ch_i)));
                        }
                    }
                });
            }
        });
}

// ── Settings tab ───────────────────────────────────────────────────────────

fn settings_ui(ui: &mut egui::Ui, s: &mut BeacnGui, max_height: f32) {
    // Selections made inside the combo-box closures, applied afterwards so the
    // closures only need shared borrows (no per-frame Vec clones).
    let mut chosen_output: Option<Option<String>> = None;
    let mut chosen_background: Option<Option<String>> = None;
    let mut any_name_focused = false;

    egui::ScrollArea::vertical()
        .id_salt("settings_page")
        .max_height(max_height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("settings_grid")
                .num_columns(2)
                .spacing([40.0, 12.0])
                .striped(true)
                .show(ui, |ui| {
                    // ── Dim after ──
                    ui.label("Dim after");
                    let mut mins = (s.display.dim_after_secs / 60) as i32;
                    if ui
                        .add(
                            egui::Slider::new(&mut mins, 1..=120)
                                .text("minutes")
                                .step_by(1.0),
                        )
                        .changed()
                    {
                        s.display.dim_after_secs = (mins as u64) * 60;
                        let _ = s.display.save();
                        s.status = format!("Dim after → {} min (daemon applies within ~1s).", mins);
                    }
                    ui.end_row();

                    // ── Full brightness ──
                    ui.label("Full brightness");
                    let mut fb = s.display.full_brightness as i32;
                    if ui
                        .add(egui::Slider::new(&mut fb, 5..=100).suffix("%").step_by(5.0))
                        .changed()
                    {
                        s.display.full_brightness = fb as u8;
                        let _ = s.display.save();
                        s.status = format!("Full brightness → {fb}% (daemon applies within ~1s).");
                    }
                    ui.end_row();

                    // ── Dim brightness ──
                    ui.label("Dim brightness");
                    let mut db = s.display.dim_brightness as i32;
                    if ui
                        .add(egui::Slider::new(&mut db, 1..=50).suffix("%").step_by(1.0))
                        .changed()
                    {
                        s.display.dim_brightness = db as u8;
                        let _ = s.display.save();
                        s.status = format!("Dim brightness → {db}% (daemon applies within ~1s).");
                    }
                    ui.end_row();

                    // ── Output device ──
                    ui.label("Output device");
                    ui.horizontal(|ui| {
                        let current_label = match &s.output.sink {
                            Some(name) => s
                                .outputs
                                .iter()
                                .find(|o| &o.name == name)
                                .map(|o| o.label().to_string())
                                .unwrap_or_else(|| format!("{name} (not present)")),
                            None => "(system default)".to_string(),
                        };

                        egui::ComboBox::from_id_salt("output_device")
                            .selected_text(&current_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(s.output.sink.is_none(), "(system default)")
                                    .clicked()
                                {
                                    chosen_output = Some(None);
                                }
                                for out in &s.outputs {
                                    if ui
                                        .selectable_label(
                                            s.output.sink.as_deref() == Some(&out.name),
                                            out.label(),
                                        )
                                        .clicked()
                                    {
                                        chosen_output = Some(Some(out.name.clone()));
                                    }
                                }
                            });
                    });
                    ui.end_row();

                    // ── Channel names ──
                    for i in 0..4 {
                        ui.label(format!("Channel {} name", i + 1));
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut s.display.channel_names[i])
                                .desired_width(200.0)
                                .char_limit(16)
                                .hint_text(format!("CH {}", i + 1)),
                        );
                        if resp.has_focus() {
                            any_name_focused = true;
                        }
                        // Commit on any loss of focus (Enter, Tab, clicking
                        // away, ...) — never silently discard the edit.
                        if resp.lost_focus() {
                            let _ = s.display.save();
                            s.status = format!(
                                "Channel {} name saved (daemon applies within ~1s).",
                                i + 1
                            );
                        }
                        ui.end_row();
                    }

                    // ── Background image ──
                    ui.label("Background");
                    ui.horizontal(|ui| {
                        let current_label = s.display.background_file.as_deref().unwrap_or("(off)");

                        egui::ComboBox::from_id_salt("background_image")
                            .selected_text(current_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(s.display.background_file.is_none(), "(off)")
                                    .clicked()
                                {
                                    chosen_background = Some(None);
                                }
                                for bg in &s.backgrounds {
                                    if ui
                                        .selectable_label(
                                            s.display.background_file.as_deref() == Some(bg),
                                            bg,
                                        )
                                        .clicked()
                                    {
                                        chosen_background = Some(Some(bg.clone()));
                                    }
                                }
                            });
                        if ui.button("🔄 Reload").clicked() && s.display.background_file.is_some()
                        {
                            s.display.background_generation =
                                s.display.background_generation.wrapping_add(1);
                            let _ = s.display.save();
                            s.status =
                                "Reloading background — daemon applies it within ~1s.".to_string();
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "Changes are saved immediately. The daemon picks them up within ~1 second.",
                )
                .color(egui::Color32::GRAY)
                .small(),
            );
        });

    s.name_edit_focused = any_name_focused;

    if let Some(sink) = chosen_output {
        let label = match &sink {
            None => "system default".to_string(),
            Some(name) => s
                .outputs
                .iter()
                .find(|o| &o.name == name)
                .map(|o| o.label().to_string())
                .unwrap_or_else(|| name.clone()),
        };
        s.output.sink = sink;
        let _ = s.output.save();
        s.status = format!("Output → {label} (daemon switches within ~1s).");
        s.refresh();
    }

    if let Some(bg) = chosen_background {
        let label = bg.as_deref().unwrap_or("(off)").to_string();
        s.display.background_file = bg;
        let _ = s.display.save();
        s.status = format!("Background → {label} (daemon applies within ~1s).");
    }
}
