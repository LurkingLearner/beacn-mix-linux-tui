//! Render the 800×480 panel image: four channel tiles, each showing the bound
//! app name, a level bar, the volume %, and a mute indicator. The Mix decodes
//! JPEG on-device, so we hand `beacn-lib`'s `set_image` a JPEG blob.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{anyhow, Result};
use image::{Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut};
use imageproc::rect::Rect;

const W: u32 = 800;
const H: u32 = 480;
const COLS: u32 = 4;
const COL_W: u32 = W / COLS; // 200

const BG: Rgb<u8> = Rgb([18, 20, 26]);
const FG: Rgb<u8> = Rgb([235, 238, 245]);
const DIM: Rgb<u8> = Rgb([120, 126, 140]);
const TRACK: Rgb<u8> = Rgb([40, 44, 54]);
const MUTE: Rgb<u8> = Rgb([224, 80, 80]);

/// Per-channel accent colour (left→right).
const ACCENT: [Rgb<u8>; 4] = [
    Rgb([86, 156, 255]),  // blue
    Rgb([95, 205, 140]),  // green
    Rgb([214, 162, 86]),  // amber
    Rgb([190, 130, 240]), // violet
];

static FONT_BYTES: &[u8] = include_bytes!("../assets/Rubik-SemiBold.ttf");

/// What to show for one channel.
pub struct ChannelView {
    pub name: String,
    pub volume: u32,
    pub muted: bool,
}

/// Render the four tiles into a JPEG suitable for `set_image(0, 0, ..)`.
pub fn render(views: &[ChannelView; 4]) -> Result<Vec<u8>> {
    let font = FontRef::try_from_slice(FONT_BYTES).map_err(|e| anyhow!("font load: {e}"))?;
    let mut img = RgbImage::from_pixel(W, H, BG);

    for (i, view) in views.iter().enumerate() {
        let x0 = i as u32 * COL_W;
        let accent = ACCENT[i];
        let cx = x0 + COL_W / 2;

        // Column divider.
        if i > 0 {
            draw_filled_rect_mut(&mut img, Rect::at(x0 as i32, 0).of_size(2, H), TRACK);
        }

        // Header: CH n
        centered(
            &mut img,
            &font,
            cx,
            18,
            30.0,
            accent,
            &format!("CH {}", i + 1),
        );

        // App name (truncated to fit the column).
        let name = truncate(&view.name, 13);
        centered(&mut img, &font, cx, 58, 26.0, FG, &name);

        // Vertical level bar.
        let bar_w = 56;
        let bar_h = 250u32;
        let bar_x = (cx - bar_w / 2) as i32;
        let bar_y = 120i32;
        draw_filled_rect_mut(
            &mut img,
            Rect::at(bar_x, bar_y).of_size(bar_w, bar_h),
            TRACK,
        );

        let frac = (view.volume as f32 / 150.0).clamp(0.0, 1.0);
        let fill_h = (bar_h as f32 * frac) as u32;
        if fill_h > 0 {
            let fill_color = if view.muted {
                Rgb([70, 50, 54])
            } else {
                accent
            };
            let fill_y = bar_y + (bar_h - fill_h) as i32;
            draw_filled_rect_mut(
                &mut img,
                Rect::at(bar_x, fill_y).of_size(bar_w, fill_h),
                fill_color,
            );
        }
        // 100% reference tick.
        let tick_y = bar_y + (bar_h as f32 * (1.0 - 100.0 / 150.0)) as i32;
        draw_filled_rect_mut(&mut img, Rect::at(bar_x, tick_y).of_size(bar_w, 2), DIM);

        // Volume % and mute state.
        let pct_color = if view.muted { DIM } else { FG };
        centered(
            &mut img,
            &font,
            cx,
            388,
            32.0,
            pct_color,
            &format!("{}%", view.volume),
        );
        if view.muted {
            centered(&mut img, &font, cx, 430, 26.0, MUTE, "MUTE");
        }
    }

    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82);
    enc.encode_image(&img)?;
    Ok(buf)
}

/// Draw text horizontally centred on `cx`, top at `y`.
fn centered(
    img: &mut RgbImage,
    font: &FontRef,
    cx: u32,
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
