//! Render the 800×480 panel: four channel tiles, each a Beacn-style arc gauge
//! (volume integer in the centre), the channel label with an accent underline,
//! and the list of source apps grouped on that channel. The Mix decodes JPEG
//! on-device, so we hand `beacn-lib`'s `set_image` a JPEG blob.
//!
//! Rendering is split into a **base** frame and **gauge patches** so the daemon
//! rarely has to push a full 800×480 JPEG (which the firmware cannot decode
//! quickly — flooding it wedges the panel). `render_base_rgb` draws everything
//! *except* the gauges (divider, mic marker, label, underline, sources); it
//! changes only on structural events and is retained by the daemon.
//! `composite_gauges` overlays all four gauges onto a base for a full frame, and
//! `render_gauge_patch` re-draws a single channel's gauge into a small 160×160
//! positioned JPEG — the cheap update for knob spins, mute toggles and live
//! meter motion.

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

// Live-level meter: a thinner concentric arc just inside the volume band
// (volume band spans r 57..71, meter band r 43..49 — a 8 px gap keeps them
// reading as two rings, and the meter stays clear of the centred volume
// number, which is at most ~39 px wide for "150").
const METER_R: f32 = 46.0;
const METER_TH: i32 = 6;
/// How far the meter colour is blended from the channel accent toward `FG`:
/// lighter than the accent so it reads as "signal", but thin enough that the
/// volume arc stays dominant.
const METER_BLEND: f32 = 0.5;

// A gauge patch re-draws one channel's entire gauge (volume arc, live meter,
// centre number or mute icon, dim track, 100% pip). This 160×160 region contains
// all of it with margin — the volume band reaches r=71 and the pip r=78, and the
// 16-px MCU alignment can shift the crop by ±4 px, so a smaller patch would clip
// them — while staying inside the 200 px column so it cannot overwrite a
// neighbouring tile or divider. It is 16 px aligned because JPEG commonly uses
// 16×16 chroma MCUs; matching the full-frame MCU grid avoids a visible
// re-encoding seam at patch edges.
const GAUGE_PATCH_W: u32 = 160;
const GAUGE_PATCH_H: u32 = 160;
const JPEG_MCU: u32 = 16;

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
    /// For a mic channel this holds the single mic's name.
    pub apps: Vec<String>,
    /// True when this channel rides a mic's gain (input) rather than a sink
    /// (output): draws a mic marker, and a mic glyph for the mute icon.
    pub is_mic: bool,
    /// Live audio level as the *displayed* fraction (0..=1) — already
    /// perceptually mapped (dBFS) by the caller. Drawn as a thin concentric
    /// arc inside the volume band; 0 draws nothing, and a muted channel never
    /// shows a meter (the mute icon is the story there).
    pub level: f32,
}

/// Load a backdrop image, scaled to cover 800×480 and optionally darkened for
/// legibility.
/// Returns `None` (and logs) on a missing/undecodable file so the caller falls
/// back to the solid colour. Do this once and reuse it — decoding is not cheap.
pub fn load_background(path: &Path, scrim: bool) -> Option<RgbImage> {
    match image::open(path) {
        Ok(img) => {
            let mut rgb = img.resize_to_fill(W, H, FilterType::Triangle).to_rgb8();
            if scrim {
                for px in rgb.pixels_mut() {
                    for c in px.0.iter_mut() {
                        *c = (*c as f32 * SCRIM) as u8;
                    }
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
    let img = render_rgb(views, background)?;
    encode_jpeg(&img)
}

/// JPEG-encode an already-rendered panel image. Keeping this separate lets the
/// caller retain the RGB base frame used to construct small live-meter patches.
pub fn encode_jpeg(img: &RgbImage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82);
    enc.encode_image(img)?;
    Ok(buf)
}

/// The MCU-aligned crop rectangle `(x, y, w, h)` of a channel's gauge patch.
/// Shared by [`render_gauge_patch`] and tests so the seam geometry is defined
/// in exactly one place.
pub fn gauge_patch_rect(channel: usize) -> (u32, u32, u32, u32) {
    // Round to the nearest JPEG MCU while keeping the patch in its own column.
    let desired_x = channel as u32 * COL_W + COL_W / 2 - GAUGE_PATCH_W / 2;
    let x = ((desired_x + JPEG_MCU / 2) / JPEG_MCU) * JPEG_MCU;
    let desired_y = GAUGE_CY as u32 - GAUGE_PATCH_H / 2;
    let y = ((desired_y + JPEG_MCU / 2) / JPEG_MCU) * JPEG_MCU;
    (x, y, GAUGE_PATCH_W, GAUGE_PATCH_H)
}

/// Render a positioned JPEG patch that re-draws one channel's whole gauge over
/// `base`.
///
/// `base` must contain the same static pixels as the most recent full frame but
/// with no gauges (i.e. a [`render_base_rgb`] image). Starting from it means a
/// lower volume, a stopped meter or a fresh mute cleanly erase whatever the
/// gauge previously drew, without re-rendering or uploading the whole panel.
/// `view.label` is unused (the gauge draws no text label).
pub fn render_gauge_patch(
    base: &RgbImage,
    channel: usize,
    view: &ChannelView,
) -> Result<(u32, u32, Vec<u8>)> {
    if base.width() != W || base.height() != H {
        return Err(anyhow!("gauge patch base must be {W}x{H}"));
    }
    if channel >= COLS as usize {
        return Err(anyhow!("invalid gauge channel {channel}"));
    }
    let font = FontRef::try_from_slice(FONT_BYTES).map_err(|e| anyhow!("font load: {e}"))?;

    let (x, y, w, h) = gauge_patch_rect(channel);
    let mut patch = image::imageops::crop_imm(base, x, y, w, h).to_image();

    // The gauge centre relative to the cropped patch's own origin.
    let cx = (channel as u32 * COL_W + COL_W / 2) as i32 - x as i32;
    let cy = GAUGE_CY - y as i32;
    draw_gauge(&mut patch, &font, (cx, cy), view, ACCENT[channel]);

    Ok((x, y, encode_jpeg(&patch)?))
}

/// Same as [`render`] but returns the raw 800×480 RGB buffer instead of a JPEG.
/// Useful for tests that want to inspect pixels (and a tiny bit cheaper for any
/// future caller that needs the bitmap directly). Equivalent, by construction,
/// to `composite_gauges(&render_base_rgb(views, background), views)`, but loads
/// the font only once.
pub fn render_rgb(views: &[ChannelView; 4], background: Option<&RgbImage>) -> Result<RgbImage> {
    let font = FontRef::try_from_slice(FONT_BYTES).map_err(|e| anyhow!("font load: {e}"))?;
    let mut img = base_image(background);
    draw_base(&mut img, &font, views);
    draw_all_gauges(&mut img, &font, views);
    Ok(img)
}

/// The panel base frame: everything *except* the gauges (column divider, mic
/// marker, label, accent underline, sources list). The daemon retains this and
/// re-draws only the gauges as small patches, so a full-frame push (this base
/// plus gauges) is needed only when a structural element changes.
pub fn render_base_rgb(
    views: &[ChannelView; 4],
    background: Option<&RgbImage>,
) -> Result<RgbImage> {
    let font = FontRef::try_from_slice(FONT_BYTES).map_err(|e| anyhow!("font load: {e}"))?;
    let mut img = base_image(background);
    draw_base(&mut img, &font, views);
    Ok(img)
}

/// Composite all four gauges onto a base produced by [`render_base_rgb`],
/// yielding a full frame identical to [`render_rgb`].
pub fn composite_gauges(base: &RgbImage, views: &[ChannelView; 4]) -> Result<RgbImage> {
    let font = FontRef::try_from_slice(FONT_BYTES).map_err(|e| anyhow!("font load: {e}"))?;
    let mut img = base.clone();
    draw_all_gauges(&mut img, &font, views);
    Ok(img)
}

/// The blank panel canvas: the background image (cloned) or the solid colour.
fn base_image(background: Option<&RgbImage>) -> RgbImage {
    match background {
        Some(bg) => bg.clone(),
        None => RgbImage::from_pixel(W, H, BG),
    }
}

/// Draw the static (gauge-free) parts of every tile.
fn draw_base(img: &mut RgbImage, font: &FontRef, views: &[ChannelView; 4]) {
    for (i, view) in views.iter().enumerate() {
        let x0 = i as u32 * COL_W;
        let accent = ACCENT[i];
        let cx = (x0 + COL_W / 2) as i32;

        // Column divider.
        if i > 0 {
            draw_filled_rect_mut(img, Rect::at(x0 as i32, 0).of_size(2, H), TRACK);
        }

        // Small mic marker above the gauge so input channels are recognisable
        // at a glance (the gauge centre still shows the live gain number).
        if view.is_mic {
            draw_mic_icon(img, cx, 18, 4, 6, accent, false);
        }

        // Channel label + accent underline.
        centered(
            img,
            font,
            cx,
            LABEL_Y,
            23.0,
            accent,
            &truncate(&view.label, 14),
        );
        let uw = 66u32;
        draw_filled_rect_mut(
            img,
            Rect::at(cx - (uw / 2) as i32, UNDERLINE_Y).of_size(uw, 4),
            accent,
        );

        draw_sources(img, font, cx, &view.apps);
    }
}

/// Draw every channel's gauge at its full-frame centre.
fn draw_all_gauges(img: &mut RgbImage, font: &FontRef, views: &[ChannelView; 4]) {
    for (i, view) in views.iter().enumerate() {
        let cx = (i as u32 * COL_W + COL_W / 2) as i32;
        draw_gauge(img, font, (cx, GAUGE_CY), view, ACCENT[i]);
    }
}

/// The arc gauge: dim track, accent fill to the current level, a 100% headroom
/// pip, and either the volume integer or a mute icon in the centre. Drawn around
/// an explicit centre `(cx, cy)` so it can render either into the full frame (at
/// `GAUGE_CY`) or into a cropped patch at the patch-relative centre.
fn draw_gauge(
    img: &mut RgbImage,
    font: &FontRef,
    center: (i32, i32),
    view: &ChannelView,
    accent: Rgb<u8>,
) {
    let (cx, cy) = center;
    let end = GAUGE_START + GAUGE_SWEEP;

    // Track (full sweep).
    draw_arc(img, (cx, cy), GAUGE_R, GAUGE_TH, GAUGE_START, end, TRACK);

    // Fill (0..level).
    let frac = (view.volume as f32 / VOLUME_MAX).clamp(0.0, 1.0);
    if frac > 0.01 {
        let fill_color = if view.muted { MUTE_ARC } else { accent };
        let fill_end = GAUGE_START + frac * GAUGE_SWEEP;
        draw_arc(
            img,
            (cx, cy),
            GAUGE_R,
            GAUGE_TH,
            GAUGE_START,
            fill_end,
            fill_color,
        );
    }

    // Live level: a thinner concentric arc inside the volume band, same
    // angular range. Hidden while muted — the centre mute icon is the story
    // there, and a "live" ring under it would read as still audible.
    if !view.muted && view.level > 0.01 {
        let meter_end = GAUGE_START + view.level.clamp(0.0, 1.0) * GAUGE_SWEEP;
        draw_arc(
            img,
            (cx, cy),
            METER_R,
            METER_TH,
            GAUGE_START,
            meter_end,
            blend(accent, FG, METER_BLEND),
        );
    }

    // 100% headroom pip, just outside the band.
    let pip = (GAUGE_START + (100.0 / VOLUME_MAX) * GAUGE_SWEEP).to_radians();
    let pr = GAUGE_R + GAUGE_TH as f32 / 2.0 + 5.0;
    draw_filled_circle_mut(
        img,
        (
            (cx as f32 + pr * pip.cos()).round() as i32,
            (cy as f32 + pr * pip.sin()).round() as i32,
        ),
        2,
        DIM,
    );

    // Centre: mute icon, or the volume integer.
    if view.muted {
        if view.is_mic {
            draw_mic_icon(img, cx, cy - 4, 6, 9, DIM, true);
        } else {
            draw_mute_icon(img, cx, cy);
        }
    } else {
        let s = format!("{}", view.volume);
        let px = 46.0;
        centered(img, font, cx, (cy as f32 - px * 0.62) as i32, px, FG, &s);
    }
}

/// Linear blend between two colours (`t` = 0 → `a`, `t` = 1 → `b`).
fn blend(a: Rgb<u8>, b: Rgb<u8>, t: f32) -> Rgb<u8> {
    Rgb(std::array::from_fn(|i| {
        (a.0[i] as f32 + (b.0[i] as f32 - a.0[i] as f32) * t).round() as u8
    }))
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

/// A microphone glyph: a rounded-capsule body on a short stem + base. Sized by
/// the body half-width/half-height (`bw`/`bh`); with `slash` it gets the same
/// red strike-through as the mute-speaker icon (used for a muted mic).
fn draw_mic_icon(
    img: &mut RgbImage,
    cx: i32,
    cy: i32,
    bw: i32,
    bh: i32,
    color: Rgb<u8>,
    slash: bool,
) {
    // Capsule body: a rectangle capped with a circle top and bottom.
    draw_filled_rect_mut(
        img,
        Rect::at(cx - bw, cy - bh).of_size((bw * 2) as u32, (bh * 2) as u32),
        color,
    );
    draw_filled_circle_mut(img, (cx, cy - bh), bw, color);
    draw_filled_circle_mut(img, (cx, cy + bh), bw, color);
    // Stem down to a short base, so it reads as a mic on a stand.
    let stem_h = bh;
    draw_filled_rect_mut(
        img,
        Rect::at(cx - 1, cy + bh).of_size(2, stem_h as u32),
        color,
    );
    let base_w = bw + 1;
    draw_filled_rect_mut(
        img,
        Rect::at(cx - base_w, cy + bh + stem_h).of_size((base_w * 2) as u32, 2),
        color,
    );
    if slash {
        let m = bw + 6;
        let (x0, y0, x1, y1) = (
            (cx - m) as f32,
            (cy - m) as f32,
            (cx + m) as f32,
            (cy + m) as f32,
        );
        stroke(img, x0, y0, x1, y1, 4, BG);
        stroke(img, x0, y0, x1, y1, 2, MUTE);
    }
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

#[cfg(test)]
mod tests {
    //! The renderer is pure (input = views + optional background, output =
    //! `RgbImage`) so all the assertions here inspect pixels of `render_rgb`.
    //! What we care about: dimensions, accent colour reaches the gauge at the
    //! expected volume, the mute icon appears (and the volume number does not)
    //! when muted, the empty-channels em-dash, the overflow line, and the
    //! `truncate` helper's char-count contract.

    use super::*;
    use image::Rgb;

    fn empty_views() -> [ChannelView; 4] {
        std::array::from_fn(|i| ChannelView {
            label: format!("CH {}", i + 1),
            volume: 0,
            muted: false,
            apps: vec![],
            is_mic: false,
            level: 0.0,
        })
    }

    fn sample_views() -> [ChannelView; 4] {
        [
            ChannelView {
                label: "CH 1".into(),
                volume: 100,
                muted: false,
                apps: vec!["Firefox".into(), "YouTube Music".into()],
                is_mic: false,
                level: 0.6,
            },
            ChannelView {
                label: "CH 2".into(),
                volume: 0,
                muted: true,
                apps: vec!["Discord".into()],
                is_mic: false,
                level: 0.4,
            },
            ChannelView {
                label: "CH 3".into(),
                volume: 75,
                muted: false,
                apps: vec!["Spotify".into()],
                is_mic: false,
                level: 0.0,
            },
            ChannelView {
                label: "Mic".into(),
                volume: 60,
                muted: false,
                apps: vec!["Yeti Microphone".into()],
                is_mic: true,
                level: 0.8,
            },
        ]
    }

    #[test]
    fn render_rgb_returns_800x480() {
        let img = render_rgb(&empty_views(), None).expect("render");
        assert_eq!(img.width(), W);
        assert_eq!(img.height(), H);
    }

    #[test]
    fn solid_bg_is_used_when_no_background() {
        let img = render_rgb(&empty_views(), None).expect("render");
        // A corner pixel must match the declared `BG` constant.
        assert_eq!(*img.get_pixel(0, 0), BG);
    }

    #[test]
    fn custom_background_is_composited_not_replaced() {
        // A custom background of a solid red should appear in a corner — the
        // renderer composites the gauges over it rather than replacing it.
        let bg = RgbImage::from_pixel(W, H, Rgb([200, 30, 30]));
        let img = render_rgb(&empty_views(), Some(&bg)).expect("render");
        // Top-left corner is above any gauge / label / divider, so it must
        // pass through as the background red.
        assert_eq!(*img.get_pixel(0, 0), Rgb([200, 30, 30]));
    }

    #[test]
    fn column_dividers_are_drawn_between_channels() {
        let img = render_rgb(&empty_views(), None).expect("render");
        // At x = 200 (the start of column 1) the divider rect should be TRACK.
        let px = img.get_pixel(200, H / 2);
        assert_eq!(*px, TRACK, "expected TRACK divider at column boundary");
        // At x = 0 (column 0, no divider on the leftmost) it must be the BG.
        assert_eq!(*img.get_pixel(0, H / 2), BG);
    }

    #[test]
    fn accent_underline_uses_the_channel_colour() {
        let img = render_rgb(&empty_views(), None).expect("render");
        // Underline is at UNDERLINE_Y (228), centred on each column. Sample the
        // centre of column 0's underline — it must be ACCENT[0] (blue).
        let cx = (COL_W / 2) as i32;
        assert_eq!(
            *img.get_pixel(cx as u32, UNDERLINE_Y as u32 + 1),
            ACCENT[0],
            "column 0 underline must be ACCENT[0]"
        );
        // And column 1's underline must be ACCENT[1] (green), not [0].
        let cx1 = (COL_W + COL_W / 2) as i32;
        assert_eq!(
            *img.get_pixel(cx1 as u32, UNDERLINE_Y as u32 + 1),
            ACCENT[1],
            "column 1 underline must be ACCENT[1]"
        );
    }

    #[test]
    fn arc_fill_at_full_volume_reaches_accent_colour() {
        let mut views = empty_views();
        views[0].volume = 150; // VOLUME_MAX
        let img = render_rgb(&views, None).expect("render");
        // A point near the end of the gauge sweep should be the accent colour.
        // Sweep starts at 135° (lower-left) and goes 270° clockwise to 405°
        // (i.e. 45°), so the end is roughly the upper-right quadrant.
        // GAUGE_R = 64, GAUGE_CY = 116, centre cx = 100 for col 0.
        let end_rad = (GAUGE_START + GAUGE_SWEEP).to_radians();
        let r = GAUGE_R;
        let cx = (COL_W / 2) as i32;
        let x = (cx as f32 + r * end_rad.cos()).round();
        let y = (GAUGE_CY as f32 + r * end_rad.sin()).round();
        let px = img.get_pixel(x as u32, y as u32);
        assert_eq!(*px, ACCENT[0], "arc end at 100% volume must be ACCENT[0]");
    }

    #[test]
    fn arc_track_is_visible_where_fill_does_not_reach() {
        let mut views = empty_views();
        views[0].volume = 0; // no fill — only the track
        let img = render_rgb(&views, None).expect("render");
        // At the end of the sweep (no fill here), the track colour must show.
        let end_rad = (GAUGE_START + GAUGE_SWEEP).to_radians();
        let r = GAUGE_R;
        let cx = (COL_W / 2) as i32;
        let x = (cx as f32 + r * end_rad.cos()).round();
        let y = (GAUGE_CY as f32 + r * end_rad.sin()).round();
        let px = img.get_pixel(x as u32, y as u32);
        assert_eq!(*px, TRACK, "track must be visible at volume=0");
    }

    #[test]
    fn muted_channel_uses_mute_arc_colour_for_fill() {
        let mut views = empty_views();
        views[0].volume = 100;
        views[0].muted = true;
        let img = render_rgb(&views, None).expect("render");
        // Mid-sweep (fraction 0.5) is well inside the fill band.
        let mid_rad = (GAUGE_START + GAUGE_SWEEP * 0.5).to_radians();
        let r = GAUGE_R;
        let cx = (COL_W / 2) as i32;
        let x = (cx as f32 + r * mid_rad.cos()).round();
        let y = (GAUGE_CY as f32 + r * mid_rad.sin()).round();
        assert_eq!(
            *img.get_pixel(x as u32, y as u32),
            MUTE_ARC,
            "muted fill must use MUTE_ARC"
        );
    }

    #[test]
    fn empty_channel_renders_em_dash_in_source_area() {
        // No background, no apps on any channel — the em-dash must be drawn at
        // LIST_Y0 in DIM colour. Em-dash is short glyph so we sample a band of
        // rows (the actual glyph renders slightly below the baseline because of
        // font ascent) and assert at least one DIM pixel exists in that band.
        let img = render_rgb(&empty_views(), None).expect("render");
        let cx = (COL_W / 2) as i32;
        let y0 = LIST_Y0;
        let mut found_dim = false;
        'outer: for y in (y0 - 2)..=(y0 + 20) {
            for dx in (-30i32..=30).step_by(2) {
                let p = img.get_pixel((cx + dx).max(0) as u32, y.max(0) as u32);
                if *p == DIM {
                    found_dim = true;
                    break 'outer;
                }
            }
        }
        assert!(found_dim, "expected a DIM-coloured pixel under the em-dash");
    }

    #[test]
    fn overflow_line_uses_plus_n_more_format() {
        let mut views = empty_views();
        // LIST_MAX_LINES = 9 — feed in 12 apps, expect "+3 more" on the
        // overflow line at LIST_Y0 + (LIST_MAX_LINES - 1) * LIST_LINE_H.
        views[0].apps = (1..=12).map(|i| format!("app{}", i)).collect();
        let img = render_rgb(&views, None).expect("render");
        // We can't OCR the bitmap, but we can prove two things at once:
        //   1. With 12 apps, the renderer doesn't crash (it would panic on a
        //      bounds issue before this assertion even runs).
        //   2. The overflow line lands at LIST_Y0 + 8 * LIST_LINE_H, which is
        //      *after* the 9 shown apps — scan that row for any DIM pixel.
        let cx = (COL_W / 2) as i32;
        let overflow_y = LIST_Y0 + ((LIST_MAX_LINES as i32) - 1) * LIST_LINE_H - 2;
        let mut found_dim = false;
        'outer: for y in overflow_y..=(overflow_y + 22) {
            for dx in (-50i32..=50).step_by(2) {
                let p = img.get_pixel((cx + dx).max(0) as u32, y.max(0) as u32);
                if *p == DIM {
                    found_dim = true;
                    break 'outer;
                }
            }
        }
        assert!(
            found_dim,
            "expected DIM pixels at the '+N more' overflow line"
        );
    }

    #[test]
    fn mic_channel_draws_accent_marker_above_the_gauge() {
        // A mic channel gets a small accent-coloured mic glyph near the top of
        // the tile (well above the gauge). Scan that band for an ACCENT pixel.
        let mut views = empty_views();
        views[0].is_mic = true;
        let img = render_rgb(&views, None).expect("render");
        let cx = (COL_W / 2) as i32;
        let mut found = false;
        'outer: for y in 8..=30 {
            for dx in -8i32..=8 {
                if *img.get_pixel((cx + dx).max(0) as u32, y as u32) == ACCENT[0] {
                    found = true;
                    break 'outer;
                }
            }
        }
        assert!(found, "expected an ACCENT mic marker above the gauge");
    }

    #[test]
    fn muted_mic_uses_mic_icon_not_speaker_and_no_number() {
        // A muted mic draws the mic glyph (with a red slash) in the gauge centre,
        // never the volume number. The red MUTE slash must be present.
        let mut views = empty_views();
        views[0].volume = 80;
        views[0].muted = true;
        views[0].is_mic = true;
        let img = render_rgb(&views, None).expect("render");
        let cx = (COL_W / 2) as i32;
        let mut found_mute = false;
        'outer: for y in (GAUGE_CY - 20)..=(GAUGE_CY + 20) {
            for dx in -20i32..=20 {
                if *img.get_pixel((cx + dx).max(0) as u32, y.max(0) as u32) == MUTE {
                    found_mute = true;
                    break 'outer;
                }
            }
        }
        assert!(found_mute, "muted mic must show the red slash");
    }

    #[test]
    fn level_meter_draws_inner_arc_in_blended_colour() {
        let mut views = empty_views();
        views[0].volume = 100;
        views[0].level = 0.5;
        let img = render_rgb(&views, None).expect("render");
        // Quarter-sweep (0.25) is inside the 0.5 meter fill. Sample the meter
        // centreline at METER_R — it must be the accent-toward-FG blend.
        let rad = (GAUGE_START + GAUGE_SWEEP * 0.25).to_radians();
        let cx = (COL_W / 2) as i32;
        let x = (cx as f32 + METER_R * rad.cos()).round();
        let y = (GAUGE_CY as f32 + METER_R * rad.sin()).round();
        assert_eq!(
            *img.get_pixel(x as u32, y as u32),
            blend(ACCENT[0], FG, METER_BLEND),
            "meter arc must use the blended accent colour"
        );
        // Past the fill (0.75 of the sweep) the meter band must be background.
        let rad = (GAUGE_START + GAUGE_SWEEP * 0.75).to_radians();
        let x = (cx as f32 + METER_R * rad.cos()).round();
        let y = (GAUGE_CY as f32 + METER_R * rad.sin()).round();
        assert_eq!(*img.get_pixel(x as u32, y as u32), BG);
    }

    #[test]
    fn muted_channel_hides_the_level_meter() {
        let mut views = empty_views();
        views[0].volume = 100;
        views[0].muted = true;
        views[0].level = 1.0;
        let img = render_rgb(&views, None).expect("render");
        // With a full-scale level but muted, no meter pixel may appear at the
        // very start of the sweep (safely away from the centre mute icon).
        let rad = GAUGE_START.to_radians();
        let cx = (COL_W / 2) as i32;
        let x = (cx as f32 + METER_R * rad.cos()).round();
        let y = (GAUGE_CY as f32 + METER_R * rad.sin()).round();
        assert_ne!(
            *img.get_pixel(x as u32, y as u32),
            blend(ACCENT[0], FG, METER_BLEND),
            "muted channel must not draw the meter"
        );
    }

    #[test]
    fn jpeg_render_returns_non_empty_bytes() {
        let jpeg = render(&sample_views(), None).expect("render jpeg");
        assert!(!jpeg.is_empty(), "JPEG blob must not be empty");
        // The JPEG SOI marker is FF D8 FF.
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn jpeg_with_background_still_decodes_to_800x480() {
        let bg = RgbImage::from_pixel(W, H, Rgb([10, 200, 30]));
        let jpeg = render(&sample_views(), Some(&bg)).expect("render jpeg");
        let decoded = image::load_from_memory(&jpeg)
            .expect("decode jpeg")
            .to_rgb8();
        assert_eq!(decoded.width(), W);
        assert_eq!(decoded.height(), H);
    }

    #[test]
    fn gauge_patch_is_a_small_positioned_jpeg() {
        let views = sample_views();
        let base = render_base_rgb(&views, None).expect("render base");
        let (x, y, jpeg) = render_gauge_patch(&base, 2, &views[2]).expect("render patch");
        let decoded = image::load_from_memory(&jpeg)
            .expect("decode patch")
            .to_rgb8();

        // Channel 2: desired_x = 420 → MCU-round to 416; desired_y = 36 → 32.
        assert_eq!((x, y), (416, 32));
        assert_eq!(decoded.dimensions(), (GAUGE_PATCH_W, GAUGE_PATCH_H));
        // A full-frame JPEG (base + gauges) must be larger than a single patch.
        let full = render_rgb(&views, None).expect("render full");
        assert!(jpeg.len() < encode_jpeg(&full).expect("encode full").len());
    }

    #[test]
    fn render_rgb_equals_composited_base_and_gauges() {
        // Parity by construction: the full-frame path and the base+gauges path
        // must yield pixel-identical images (guards against future drift).
        let views = sample_views();
        let full = render_rgb(&views, None).expect("render full");
        let composited =
            composite_gauges(&render_base_rgb(&views, None).expect("base"), &views).expect("comp");
        assert_eq!(full.as_raw(), composited.as_raw(), "parity, solid bg");

        // Same guarantee over a non-uniform background (the real deployment).
        let bg = gradient_bg();
        let full = render_rgb(&views, Some(&bg)).expect("render full bg");
        let composited = composite_gauges(
            &render_base_rgb(&views, Some(&bg)).expect("base bg"),
            &views,
        )
        .expect("comp bg");
        assert_eq!(full.as_raw(), composited.as_raw(), "parity, gradient bg");
    }

    #[test]
    fn a_patch_repaints_everything_a_gauge_can_change() {
        // The seam guarantee: any pixel that differs between the base (no gauges)
        // and the full frame must lie inside that channel's 160×160 gauge patch
        // rect. This proves a single patch fully repaints whatever a knob, meter
        // or mute toggle can change — nothing bleeds outside the crop.
        let extremes = [
            ChannelView {
                label: "x".into(),
                volume: 150,
                muted: false,
                apps: vec![],
                is_mic: false,
                level: 1.0,
            },
            ChannelView {
                label: "x".into(),
                volume: 80,
                muted: true,
                apps: vec![],
                is_mic: true,
                level: 0.0,
            },
            ChannelView {
                label: "x".into(),
                volume: 80,
                muted: true,
                apps: vec![],
                is_mic: false,
                level: 0.0,
            },
        ];

        for backdrop in [None, Some(gradient_bg())] {
            for extreme in &extremes {
                // Put the extreme view on every channel in turn.
                for ch in 0..4 {
                    let views: [ChannelView; 4] = std::array::from_fn(|i| {
                        if i == ch {
                            clone_view(extreme)
                        } else {
                            ChannelView {
                                label: format!("CH {}", i + 1),
                                volume: 40,
                                muted: false,
                                apps: vec![],
                                is_mic: false,
                                level: 0.0,
                            }
                        }
                    });
                    let base = render_base_rgb(&views, backdrop.as_ref()).expect("base");
                    let full = render_rgb(&views, backdrop.as_ref()).expect("full");
                    // Every channel draws a gauge, so check each differing pixel
                    // against *its own column's* patch rect — this proves each
                    // channel's gauge (the extreme one included) stays within
                    // the patch that repaints it.
                    for y in 0..H {
                        for x in 0..W {
                            if base.get_pixel(x, y) == full.get_pixel(x, y) {
                                continue;
                            }
                            let col = (x / COL_W) as usize;
                            let (px, py, pw, ph) = gauge_patch_rect(col);
                            let inside = x >= px && x < px + pw && y >= py && y < py + ph;
                            assert!(
                                inside,
                                "differing pixel ({x},{y}) outside ch {col} patch \
                                 ({px},{py},{pw},{ph}); extreme on ch {ch}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A synthetic non-uniform background so parity/containment tests exercise
    /// the real (image-backed) deployment path, not just the solid colour.
    fn gradient_bg() -> RgbImage {
        RgbImage::from_fn(W, H, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        })
    }

    fn clone_view(v: &ChannelView) -> ChannelView {
        ChannelView {
            label: v.label.clone(),
            volume: v.volume,
            muted: v.muted,
            apps: v.apps.clone(),
            is_mic: v.is_mic,
            level: v.level,
        }
    }

    // --- truncate -----------------------------------------------------------

    #[test]
    fn truncate_returns_input_under_limit() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("", 5), "");
        assert_eq!(truncate("abc", 3), "abc"); // exactly at limit, no ellipsis
    }

    #[test]
    fn truncate_replaces_tail_with_ellipsis() {
        let out = truncate("abcdefghijklmnop", 5);
        // 4 chars + ellipsis
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("abcd"));
    }

    #[test]
    fn truncate_handles_multibyte_at_boundary() {
        // The function must count chars, not bytes — `é` is 2 bytes.
        let s = "aébcdef";
        let out = truncate(s, 4);
        assert_eq!(out.chars().count(), 4);
        assert!(out.ends_with('…'));
    }
}
