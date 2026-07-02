//! GUI configuration window (egui/eframe). A mouse-driven alternative to the
//! terminal TUI. Reads/writes the same JSON files the daemon already reacts to,
//! so it runs alongside `run` just like `tui` does — no IPC, no daemon changes.
#![cfg(feature = "gui")]

use crate::mix::Channel;
use crate::pw;
use crate::state::{Bindings, DisplayConfig, Levels, OutputConfig};
use anyhow::{Context, Result};
use eframe::egui;
use std::collections::HashSet;
use std::time::Instant;

// ── accent colours (same as tui.rs and screen.rs) ──────────────────────────

const ACCENT: [egui::Color32; 4] = [
    egui::Color32::from_rgb(86, 156, 255),  // blue
    egui::Color32::from_rgb(95, 205, 140),  // green
    egui::Color32::from_rgb(214, 162, 86),  // amber
    egui::Color32::from_rgb(190, 130, 240), // violet
];

// ── row types (flattened from tui.rs's Row enum) ───────────────────────────

struct StreamRow {
    index: u32,
    app: String,
    label: String,
    channel: Option<Channel>,
    is_idle: bool,
}

struct MicRow {
    name: String,
    label: String,
    channel: Option<Channel>,
}

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

// ── snapshot helpers (mirror tui::snapshot) ────────────────────────────────

fn build_stream_rows() -> Vec<StreamRow> {
    let streams = pw::app_streams().unwrap_or_default();
    let bindings = Bindings::load().unwrap_or_default();
    let live_apps: HashSet<&str> = streams.iter().map(|s| s.app.as_str()).collect();

    let mut rows: Vec<StreamRow> = streams
        .iter()
        .map(|s| StreamRow {
            index: s.index,
            app: s.app.clone(),
            label: s.label(),
            channel: pw::channel_of_sink(&s.sink),
            is_idle: false,
        })
        .collect();

    for ch in Channel::ALL {
        for app in bindings.apps_for_channel(ch) {
            if !live_apps.contains(app.as_str()) {
                rows.push(StreamRow {
                    index: 0,
                    label: format!("{app} (idle)"),
                    app,
                    channel: Some(ch),
                    is_idle: true,
                });
            }
        }
    }
    rows
}

fn build_mic_rows() -> Vec<MicRow> {
    let bindings = Bindings::load().unwrap_or_default();
    let sources = pw::list_sources().unwrap_or_default();
    let present: HashSet<&str> = sources.iter().map(|s| s.name.as_str()).collect();

    let mut rows: Vec<MicRow> = sources
        .iter()
        .map(|s| MicRow {
            name: s.name.clone(),
            label: s.label().to_string(),
            channel: bindings.channel_for_mic(&s.name),
        })
        .collect();

    for (name, &ch) in &bindings.mic_by_source {
        if !present.contains(name.as_str()) {
            rows.push(MicRow {
                name: name.clone(),
                label: format!("{name} (disconnected)"),
                channel: Some(Channel(ch)),
            });
        }
    }
    rows
}

// ── assign / unassign (mirror tui.rs) ──────────────────────────────────────

fn assign_stream(row: &StreamRow, ch: Channel) -> Result<()> {
    if !row.is_idle {
        pw::move_stream(row.index, ch).context("moving stream")?;
    }
    let mut bindings = Bindings::load().unwrap_or_default();
    bindings.set(&row.app, ch);
    bindings.save()
}

fn unassign_stream(row: &StreamRow) -> Result<()> {
    if !row.is_idle {
        let default = pw::default_sink().context("getting default sink")?;
        pw::move_to_sink(row.index, &default).context("moving to default sink")?;
    }
    let mut bindings = Bindings::load().unwrap_or_default();
    bindings.remove(&row.app);
    bindings.save()
}

fn assign_mic(row: &MicRow, ch: Channel) -> Result<()> {
    let mut bindings = Bindings::load().unwrap_or_default();
    bindings.set_mic(ch, &row.name);
    bindings.save()
}

fn unassign_mic(row: &MicRow) -> Result<()> {
    let mut bindings = Bindings::load().unwrap_or_default();
    bindings.remove_mic(&row.name);
    bindings.save()
}

// ── the app state ──────────────────────────────────────────────────────────

struct BeacnGui {
    tab: Tab,
    streams: Vec<StreamRow>,
    mics: Vec<MicRow>,
    levels: Levels,
    display: DisplayConfig,
    output: OutputConfig,
    outputs: Vec<pw::Output>,
    backgrounds: Vec<String>,
    filter: String,
    status: String,
    last_refresh: Instant,
}

impl Default for BeacnGui {
    fn default() -> Self {
        Self {
            tab: Tab::Routing,
            streams: build_stream_rows(),
            mics: build_mic_rows(),
            levels: Levels::load().unwrap_or_default(),
            display: DisplayConfig::load().unwrap_or_default(),
            output: OutputConfig::load().unwrap_or_default(),
            outputs: pw::list_outputs().unwrap_or_default(),
            backgrounds: crate::state::list_backgrounds(),
            filter: String::new(),
            status: String::new(),
            last_refresh: Instant::now(),
        }
    }
}

impl BeacnGui {
    fn refresh(&mut self) {
        self.streams = build_stream_rows();
        self.mics = build_mic_rows();
        self.levels = Levels::load().unwrap_or_default();
        self.output = OutputConfig::load().unwrap_or_default();
        self.outputs = pw::list_outputs().unwrap_or_default();
        self.backgrounds = crate::state::list_backgrounds();
        self.last_refresh = Instant::now();
    }
}

// ── eframe::App impl ───────────────────────────────────────────────────────

impl eframe::App for BeacnGui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Refresh PipeWire state ~once per second (same cadence as TUI).
        if self.last_refresh.elapsed().as_millis() > 750 {
            self.refresh();
        }

        // Use the entire available area with a vertical layout.
        ui.vertical(|ui| {
            // ── Tab bar ──
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Routing, "🎚 Routing");
                ui.selectable_value(&mut self.tab, Tab::Settings, "⚙ Settings");
            });
            ui.separator();

            // ── Main content ──
            match self.tab {
                Tab::Routing => {
                    let assignments = routing_data(self);
                    routing_ui(ui, self, &assignments);
                }
                Tab::Settings => {
                    settings_ui(ui, self);
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

        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(750));
    }
}

// ── Routing data: precompute what the UI needs (avoid borrow issues) ───────

struct RoutingData {
    /// (label, idle) per channel (indexed by channel order).
    channel_items: [Vec<(String, bool)>; 4],
    /// Unassigned streams, sorted by label.
    unassigned_streams: Vec<usize>,
    /// Unassigned mics, sorted by label.
    unassigned_mics: Vec<usize>,
}

fn routing_data(s: &BeacnGui) -> RoutingData {
    let channel_items: [Vec<(String, bool)>; 4] = std::array::from_fn(|i| {
        let ch = Channel(i);
        let mut items: Vec<(String, bool)> = s
            .streams
            .iter()
            .filter(|r| r.channel == Some(ch))
            .map(|r| (r.label.clone(), r.is_idle))
            .chain(
                s.mics
                    .iter()
                    .filter(|m| m.channel == Some(ch))
                    .map(|m| (m.label.clone(), false)),
            )
            .collect();
        items.sort_by_key(|a| a.0.to_lowercase());
        items
    });

    let needle = s.filter.to_lowercase();
    let matches = |label: &str| needle.is_empty() || label.to_lowercase().contains(&needle);

    let mut unassigned_streams: Vec<usize> = s
        .streams
        .iter()
        .enumerate()
        .filter(|(_, r)| r.channel.is_none() && matches(&r.label))
        .map(|(i, _)| i)
        .collect();
    unassigned_streams.sort_by_key(|&i| s.streams[i].label.to_lowercase());

    let mut unassigned_mics: Vec<usize> = s
        .mics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.channel.is_none() && matches(&m.label))
        .map(|(i, _)| i)
        .collect();
    unassigned_mics.sort_by_key(|&i| s.mics[i].label.to_lowercase());

    RoutingData {
        channel_items,
        unassigned_streams,
        unassigned_mics,
    }
}

// ── Routing tab ────────────────────────────────────────────────────────────

fn routing_ui(ui: &mut egui::Ui, s: &mut BeacnGui, data: &RoutingData) {
    // ── four channel columns ──
    ui.columns(4, |cols| {
        for i in 0..4 {
            cols[i].vertical_centered(|ui| {
                ui.heading(egui::RichText::new(format!("CH{}", i + 1)).color(ACCENT[i]));
                let name = s.display.channel_label(i);
                if !name.starts_with("CH ") {
                    ui.label(egui::RichText::new(&name).small());
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
                    let mut unassign_label: Option<String> = None;
                    for (label, idle) in &data.channel_items[i] {
                        ui.horizontal(|ui| {
                            let mut text = egui::RichText::new(label.as_str()).size(12.0);
                            if *idle {
                                text = text.color(egui::Color32::DARK_GRAY);
                            }
                            ui.label(text);
                            if ui.small_button("✕").clicked() {
                                unassign_label = Some(label.clone());
                            }
                        });
                    }
                    if let Some(label) = unassign_label {
                        let ch = Channel(i);
                        s.status = match unassign_by_channel_label(s, ch, &label) {
                            Ok(()) => format!("Unassigned {label}"),
                            Err(e) => format!("Unassign failed: {e}"),
                        };
                        s.refresh();
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
        if ui.button("✕").clicked() {
            s.filter.clear();
        }
    });

    ui.add_space(4.0);

    // ── Two side-by-side lists: Apps and Mics ──
    ui.columns(2, |cols| {
        // ── Apps ──
        cols[0].heading("Apps");
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(&mut cols[0], |ui| {
                if data.unassigned_streams.is_empty() {
                    ui.label(
                        egui::RichText::new("(none)")
                            .color(egui::Color32::DARK_GRAY)
                            .italics(),
                    );
                }
                for &idx in &data.unassigned_streams {
                    let label = s.streams[idx].label.clone();
                    let app = s.streams[idx].app.clone();
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&label).size(12.0));
                        for ch_i in 0..4 {
                            if ui.small_button(format!("CH{}", ch_i + 1)).clicked() {
                                let ch = Channel(ch_i);
                                s.status = match assign_stream_by_idx(s, idx, ch) {
                                    Ok(()) => format!("Assigned {app} → CH{}", ch_i + 1),
                                    Err(e) => {
                                        format!("Assign failed: {e}")
                                    }
                                };
                                s.refresh();
                            }
                        }
                    });
                }
            });

        // ── Mics ──
        cols[1].heading("Mics");
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(&mut cols[1], |ui| {
                if data.unassigned_mics.is_empty() {
                    ui.label(
                        egui::RichText::new("(none)")
                            .color(egui::Color32::DARK_GRAY)
                            .italics(),
                    );
                }
                for &idx in &data.unassigned_mics {
                    let label = s.mics[idx].label.clone();
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&label).size(12.0));
                        for ch_i in 0..4 {
                            if ui.small_button(format!("CH{}", ch_i + 1)).clicked() {
                                let ch = Channel(ch_i);
                                s.status = match assign_mic_by_idx(s, idx, ch) {
                                    Ok(()) => format!("Assigned mic {label} → CH{}", ch_i + 1),
                                    Err(e) => {
                                        format!("Assign failed: {e}")
                                    }
                                };
                                s.refresh();
                            }
                        }
                    });
                }
            });
    });
}

fn assign_stream_by_idx(s: &mut BeacnGui, idx: usize, ch: Channel) -> Result<()> {
    assign_stream(&s.streams[idx], ch)
}

fn assign_mic_by_idx(s: &mut BeacnGui, idx: usize, ch: Channel) -> Result<()> {
    assign_mic(&s.mics[idx], ch)
}

fn unassign_by_channel_label(s: &mut BeacnGui, ch: Channel, label: &str) -> Result<()> {
    if let Some(stream) = s
        .streams
        .iter()
        .find(|r| r.channel == Some(ch) && r.label == label)
    {
        return unassign_stream(stream);
    }
    if let Some(mic) = s
        .mics
        .iter()
        .find(|m| m.channel == Some(ch) && m.label == label)
    {
        return unassign_mic(mic);
    }
    Ok(())
}

// ── Settings tab ───────────────────────────────────────────────────────────

fn settings_ui(ui: &mut egui::Ui, s: &mut BeacnGui) {
    egui::ScrollArea::vertical().show(ui, |ui| {
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
                                s.output.sink = None;
                                let _ = s.output.save();
                                s.status = "Output → system default (daemon switches within ~1s)."
                                    .to_string();
                                s.refresh();
                            }
                            for out in &s.outputs.clone() {
                                if ui
                                    .selectable_label(
                                        s.output.sink.as_deref() == Some(&out.name),
                                        out.label(),
                                    )
                                    .clicked()
                                {
                                    s.output.sink = Some(out.name.clone());
                                    let _ = s.output.save();
                                    s.status = format!(
                                        "Output → {} (daemon switches within ~1s).",
                                        out.label()
                                    );
                                    s.refresh();
                                }
                            }
                        });
                });
                ui.end_row();

                // ── Channel names ──
                for i in 0..4 {
                    ui.label(format!("Channel {} name", i + 1));
                    let changed = ui
                        .add(
                            egui::TextEdit::singleline(&mut s.display.channel_names[i])
                                .desired_width(200.0)
                                .char_limit(16)
                                .hint_text(format!("CH {}", i + 1)),
                        )
                        .lost_focus()
                        && ui.input(|inp| inp.key_pressed(egui::Key::Enter));
                    if changed {
                        let _ = s.display.save();
                        s.status =
                            format!("Channel {} name saved (daemon applies within ~1s).", i + 1);
                    }
                    ui.end_row();
                }

                // ── Background image ──
                ui.label("Background");
                ui.horizontal(|ui| {
                    let current_label = s.display.background_file.as_deref().unwrap_or("(off)");

                    let backgrounds = s.backgrounds.clone();
                    egui::ComboBox::from_id_salt("background_image")
                        .selected_text(current_label)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(s.display.background_file.is_none(), "(off)")
                                .clicked()
                            {
                                s.display.background_file = None;
                                let _ = s.display.save();
                                s.status =
                                    "Background → (off) (daemon applies within ~1s).".to_string();
                            }
                            for bg in &backgrounds {
                                if ui
                                    .selectable_label(
                                        s.display.background_file.as_deref() == Some(bg),
                                        bg,
                                    )
                                    .clicked()
                                {
                                    s.display.background_file = Some(bg.clone());
                                    let _ = s.display.save();
                                    s.status =
                                        format!("Background → {bg} (daemon applies within ~1s).");
                                }
                            }
                        });
                    if ui.button("🔄 Reload").clicked() && s.display.background_file.is_some() {
                        s.display.background_generation =
                            s.display.background_generation.wrapping_add(1);
                        let _ = s.display.save();
                        s.status =
                            "Reloading background — daemon applies it within ~1s.".to_string();
                    }
                });
                ui.end_row();
            });
    });

    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(
            "Changes are saved immediately. The daemon picks them up within ~1 second.",
        )
        .color(egui::Color32::GRAY)
        .small(),
    );
}
