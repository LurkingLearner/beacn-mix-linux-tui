//! Render the 800×480 panel: four channel tiles, each a Beacn-style arc gauge
//! (volume integer in the centre), the channel label with an accent underline,
//! and the list of source apps grouped on that channel. The Mix decodes JPEG
//! on-device, so we hand `beacn-lib`'s `set_image` a JPEG blob.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{anyhow, Result};
use image::imageops::FilterType;
use image::{Rgb, RgbImage};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_polygon_mut, draw_text_mut,
};
use imageproc::point::Point;
use imageproc::rect::Rect;
use std::path::Path;

const W: u32 = 800;
const H: u32 = 480;
const COLS: u32 = 4;
const COL_W: u32 = W / COLS; // 200

const BG: Rgb<u8> = Rgb([18, 20, 26]);
const FG: Rgb<u8> = Rgb([235, 238, 245]);
const DIM: Rgb<u8> = Rgb([120, 126, 140]);
const TRACK: Rgb<u8> = Rgb([40, 44, 54]);
const MUTE: Rgb<u8> = Rgb([224, 80, 80]);
const MUTE_ARC: Rgb<u8> = Rgb([120, 62, 66]);

/// How much to darken a background image so the overlays stay legible (0 = black,
/// 1 = untouched).
const SCRIM: f32 = 0.45;

/// Per-channel accent colour (left→right).
const ACCENT: [Rgb<u8>; 4] = [
    Rgb([86, 156, 255]),  // blue
    Rgb([95, 205, 140]),  // green
    Rgb([214, 162, 86]),  // amber
    Rgb([190, 130, 240]), // violet
];

// Gauge geometry: a ~270° arc, open at the bottom (gap centred on straight down).
const GAUGE_CY: i32 = 116;
const GAUGE_R: f32 = 64.0;
const GAUGE_TH: i32 = 14;
const GAUGE_START: f32 = 135.0;
const GAUGE_SWEEP: f32 = 270.0;
const VOLUME_MAX: f32 = 150.0;

// Label + source list below the gauge.
const LABEL_Y: i32 = 196;
const UNDERLINE_Y: i32 = 228;
const LIST_Y0: i32 = 244;
const LIST_LINE_H: i32 = 23;
const LIST_MAX_LINES: usize = 9;

static FONT_BYTES: &[u8] = include_bytes!("../assets/Rubik-SemiBold.ttf");

/// What to show for one channel.
pub struct ChannelView {
    /// Channel label (custom name, or "CH n").
    pub label: String,
    pub volume: u32,
    pub muted: bool,
    /// Source apps grouped on this channel (top-to-bottom). Empty = unbound.
    pub apps: Vec<String>,
}

/// Load a backdrop image, scaled to cover 800×480 and darkened for legibility.
/// Returns `None` (and logs) on a missing/undecodable file so the caller falls
/// back to the solid colour. Do this once and reuse it — decoding is not cheap.
pub fn load_background(path: &Path) -> Option<RgbImage> {
    match image::open(path) {
        Ok(img) => {
            let mut rgb = img.resize_to_fill(W, H, FilterType::Triangle).to_rgb8();
            for px in rgb.pixels_mut() {
                for c in px.0.iter_mut() {
                    *c = (*c as f32 * SCRIM) as u8;
                }
            }
            Some(rgb)
        }
        Err(e) => {
            log::warn!("could not load background {}: {e}", path.display());
            None
        }
    }
}

/// Render the four tiles into a JPEG suitable for `set_image(0, 0, ..)`. With a
/// `background` (already sized to 800×480) the gauges are drawn over it; without
/// one a solid colour is used.
pub fn render(views: &[ChannelView; 4], background: Option<&RgbImage>) -> Result<Vec<u8>> {
    let font = FontRef::try_from_slice(FONT_BYTES).map_err(|e| anyhow!("font load: {e}"))?;
    let mut img = match background {
        Some(bg) => bg.clone(),
        None => RgbImage::from_pixel(W, H, BG),
    };

    for (i, view) in views.iter().enumerate() {
        let x0 = i as u32 * COL_W;
        let accent = ACCENT[i];
        let cx = (x0 + COL_W / 2) as i32;

        // Column divider.
        if i > 0 {
            draw_filled_rect_mut(&mut img, Rect::at(x0 as i32, 0).of_size(2, H), TRACK);
        }

        draw_gauge(&mut img, &font, cx, view, accent);

        // Channel label + accent underline.
        centered(
            &mut img,
            &font,
            cx,
            LABEL_Y,
            23.0,
            accent,
            &truncate(&view.label, 14),
        );
        let uw = 66u32;
        draw_filled_rect_mut(
            &mut img,
            Rect::at(cx - (uw / 2) as i32, UNDERLINE_Y).of_size(uw, 4),
            accent,
        );

        draw_sources(&mut img, &font, cx, &view.apps);
    }

    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82);
    enc.encode_image(&img)?;
    Ok(buf)
}

/// The arc gauge: dim track, accent fill to the current level, a 100% headroom
/// pip, and either the volume integer or a mute icon in the centre.
fn draw_gauge(img: &mut RgbImage, font: &FontRef, cx: i32, view: &ChannelView, accent: Rgb<u8>) {
    let end = GAUGE_START + GAUGE_SWEEP;

    // Track (full sweep).
    draw_arc(
        img,
        (cx, GAUGE_CY),
        GAUGE_R,
        GAUGE_TH,
        GAUGE_START,
        end,
        TRACK,
    );

    // Fill (0..level).
    let frac = (view.volume as f32 / VOLUME_MAX).clamp(0.0, 1.0);
    if frac > 0.01 {
        let fill_color = if view.muted { MUTE_ARC } else { accent };
        let fill_end = GAUGE_START + frac * GAUGE_SWEEP;
        draw_arc(
            img,
            (cx, GAUGE_CY),
            GAUGE_R,
            GAUGE_TH,
            GAUGE_START,
            fill_end,
            fill_color,
        );
    }

    // 100% headroom pip, just outside the band.
    let pip = (GAUGE_START + (100.0 / VOLUME_MAX) * GAUGE_SWEEP).to_radians();
    let pr = GAUGE_R + GAUGE_TH as f32 / 2.0 + 5.0;
    draw_filled_circle_mut(
        img,
        (
            (cx as f32 + pr * pip.cos()).round() as i32,
            (GAUGE_CY as f32 + pr * pip.sin()).round() as i32,
        ),
        2,
        DIM,
    );

    // Centre: mute icon, or the volume integer.
    if view.muted {
        draw_mute_icon(img, cx, GAUGE_CY);
    } else {
        let s = format!("{}", view.volume);
        let px = 46.0;
        centered(
            img,
            font,
            cx,
            (GAUGE_CY as f32 - px * 0.62) as i32,
            px,
            FG,
            &s,
        );
    }
}

/// Draw a thick arc by stepping filled circles along its centreline (rounded caps).
fn draw_arc(
    img: &mut RgbImage,
    center: (i32, i32),
    r: f32,
    th: i32,
    start_deg: f32,
    end_deg: f32,
    color: Rgb<u8>,
) {
    let (cx, cy) = center;
    let span = (end_deg - start_deg).abs();
    let steps = (span * 1.5).ceil().max(1.0) as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let rad = (start_deg + (end_deg - start_deg) * t).to_radians();
        let x = (cx as f32 + r * rad.cos()).round() as i32;
        let y = (cy as f32 + r * rad.sin()).round() as i32;
        draw_filled_circle_mut(img, (x, y), th / 2, color);
    }
}

/// A small muted-speaker glyph (grey speaker + red slash) for the gauge centre.
fn draw_mute_icon(img: &mut RgbImage, cx: i32, cy: i32) {
    // Speaker back (rectangle) + cone (trapezoid widening right).
    draw_filled_rect_mut(img, Rect::at(cx - 16, cy - 7).of_size(8, 14), DIM);
    let cone = [
        Point::new(cx - 9, cy - 7),
        Point::new(cx + 3, cy - 16),
        Point::new(cx + 3, cy + 16),
        Point::new(cx - 9, cy + 7),
    ];
    draw_polygon_mut(img, &cone, DIM);
    // Slash with a dark casing so the red reads over the grey speaker.
    let (x0, y0, x1, y1) = (
        (cx - 18) as f32,
        (cy - 16) as f32,
        (cx + 16) as f32,
        (cy + 16) as f32,
    );
    stroke(img, x0, y0, x1, y1, 4, BG);
    stroke(img, x0, y0, x1, y1, 2, MUTE);
}

/// Draw a thick line by stepping filled circles along it.
fn stroke(img: &mut RgbImage, x0: f32, y0: f32, x1: f32, y1: f32, radius: i32, color: Rgb<u8>) {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let steps = len as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        draw_filled_circle_mut(
            img,
            ((x0 + dx * t) as i32, (y0 + dy * t) as i32),
            radius,
            color,
        );
    }
}

/// The small source-app list under the label, with a "+N more" overflow line.
fn draw_sources(img: &mut RgbImage, font: &FontRef, cx: i32, apps: &[String]) {
    if apps.is_empty() {
        centered(img, font, cx, LIST_Y0, 17.0, DIM, "—");
        return;
    }
    let mut y = LIST_Y0;
    let overflow = apps.len() > LIST_MAX_LINES;
    let shown = if overflow {
        LIST_MAX_LINES - 1
    } else {
        apps.len()
    };
    for app in apps.iter().take(shown) {
        centered(img, font, cx, y, 17.0, DIM, &truncate(app, 20));
        y += LIST_LINE_H;
    }
    if overflow {
        centered(
            img,
            font,
            cx,
            y,
            17.0,
            DIM,
            &format!("+{} more", apps.len() - shown),
        );
    }
}

/// Draw text horizontally centred on `cx`, top at `y`.
fn centered(
    img: &mut RgbImage,
    font: &FontRef,
    cx: i32,
    y: i32,
    px: f32,
    color: Rgb<u8>,
    text: &str,
) {
    let scale = PxScale::from(px);
    let width = text_width(font, scale, text);
    let x = (cx as f32 - width / 2.0).max(2.0) as i32;
    draw_text_mut(img, color, x, y, scale, font, text);
}

/// Sum of glyph advances, for centring.
fn text_width(font: &FontRef, scale: PxScale, text: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    text.chars()
        .map(|c| scaled.h_advance(font.glyph_id(c)))
        .sum()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
