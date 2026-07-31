//! Programmatic tray icons: one cloud silhouette, five states.
//!
//! Icons are rasterized at runtime with tiny-skia and handed to the
//! StatusNotifier host as ARGB32 pixmaps. Drawing them ourselves (instead of
//! referencing theme icon names) guarantees the same identity on every
//! desktop, panel theme, and distro without installing icon files.

use ksni::Icon;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

/// Sync state as far as the icon is concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Ok,
    Syncing,
    Paused,
    AuthRequired,
    Error,
}

/// Panel icons are usually shown at 22–24 px; 48 px covers HiDPI hosts.
const SIZES: [u32; 2] = [22, 48];

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 0xFF)
}

// Cloud body — light neutral that reads on the dark panels virtually all
// desktops use for their tray area.
fn cloud_color() -> Color {
    rgb(0xE8, 0xED, 0xF3)
}

fn glyph_color() -> Color {
    rgb(0xFF, 0xFF, 0xFF)
}

fn badge_color(state: IconState) -> Color {
    match state {
        IconState::Ok => rgb(0x57, 0xB1, 0x83),
        IconState::Syncing => rgb(0x5A, 0xA2, 0xDD),
        IconState::Paused => rgb(0x8E, 0x9C, 0xAC),
        IconState::AuthRequired => rgb(0xCF, 0xA0, 0x4A),
        IconState::Error => rgb(0xD0, 0x71, 0x6A),
    }
}

/// Render the icon for `state` at all panel sizes.
pub fn render(state: IconState) -> Vec<Icon> {
    SIZES.iter().map(|&s| render_at(state, s)).collect()
}

fn render_at(state: IconState, size: u32) -> Icon {
    let mut pixmap = Pixmap::new(size, size).expect("create pixmap");
    // All geometry below is authored on a 24×24 grid.
    let k = size as f32 / 24.0;
    let t = Transform::from_scale(k, k);

    draw_cloud(&mut pixmap, t);
    draw_badge(&mut pixmap, t, state);

    Icon {
        width: size as i32,
        height: size as i32,
        data: rgba_to_argb_network(pixmap.data()),
    }
}

/// Cloud silhouette: three overlapping circles on a rounded base bar.
fn draw_cloud(pixmap: &mut Pixmap, t: Transform) {
    let mut paint = Paint::default();
    paint.set_color(cloud_color());
    paint.anti_alias = true;

    let mut pb = PathBuilder::new();
    pb.push_circle(7.4, 12.4, 4.4);
    pb.push_circle(12.2, 9.8, 5.2);
    pb.push_circle(16.4, 12.8, 3.9);
    if let Some(rect) = Rect::from_ltrb(5.0, 12.0, 18.5, 17.2) {
        pb.push_rect(rect);
    }
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &paint, FillRule::Winding, t, None);
    }
}

/// Colored state badge in the lower-right corner, with a dark separation
/// ring so it stays legible on top of the cloud.
fn draw_badge(pixmap: &mut Pixmap, t: Transform, state: IconState) {
    const CX: f32 = 17.3;
    const CY: f32 = 17.0;
    const R: f32 = 5.3;

    let mut ring = Paint::default();
    ring.set_color(Color::from_rgba8(0x10, 0x15, 0x1C, 0xB0));
    ring.anti_alias = true;
    if let Some(p) = PathBuilder::from_circle(CX, CY, R + 1.1) {
        pixmap.fill_path(&p, &ring, FillRule::Winding, t, None);
    }

    let mut fill = Paint::default();
    fill.set_color(badge_color(state));
    fill.anti_alias = true;
    if let Some(p) = PathBuilder::from_circle(CX, CY, R) {
        pixmap.fill_path(&p, &fill, FillRule::Winding, t, None);
    }

    let mut glyph = Paint::default();
    glyph.set_color(glyph_color());
    glyph.anti_alias = true;
    let stroke = Stroke {
        width: 1.7,
        line_cap: tiny_skia::LineCap::Round,
        ..Stroke::default()
    };

    let mut pb = PathBuilder::new();
    match state {
        IconState::Ok => {
            // Checkmark.
            pb.move_to(CX - 2.3, CY + 0.2);
            pb.line_to(CX - 0.6, CY + 1.9);
            pb.line_to(CX + 2.4, CY - 1.7);
        }
        IconState::Syncing => {
            // Two opposing transfer arrows (up + down).
            pb.move_to(CX - 1.5, CY + 2.2);
            pb.line_to(CX - 1.5, CY - 2.2);
            pb.move_to(CX - 2.8, CY - 0.9);
            pb.line_to(CX - 1.5, CY - 2.4);
            pb.line_to(CX - 0.2, CY - 0.9);
            pb.move_to(CX + 1.5, CY - 2.2);
            pb.line_to(CX + 1.5, CY + 2.2);
            pb.move_to(CX + 0.2, CY + 0.9);
            pb.line_to(CX + 1.5, CY + 2.4);
            pb.line_to(CX + 2.8, CY + 0.9);
        }
        IconState::Paused => {
            pb.move_to(CX - 1.2, CY - 1.9);
            pb.line_to(CX - 1.2, CY + 1.9);
            pb.move_to(CX + 1.2, CY - 1.9);
            pb.line_to(CX + 1.2, CY + 1.9);
        }
        IconState::AuthRequired => {
            // Exclamation mark.
            pb.move_to(CX, CY - 2.4);
            pb.line_to(CX, CY + 0.6);
            pb.move_to(CX, CY + 2.4);
            pb.line_to(CX, CY + 2.5);
        }
        IconState::Error => {
            pb.move_to(CX - 1.9, CY - 1.9);
            pb.line_to(CX + 1.9, CY + 1.9);
            pb.move_to(CX + 1.9, CY - 1.9);
            pb.line_to(CX - 1.9, CY + 1.9);
        }
    }
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &glyph, &stroke, t, None);
    }
}

/// tiny-skia stores premultiplied RGBA bytes; StatusNotifier wants ARGB32 in
/// network byte order (A, R, G, B per pixel).
fn rgba_to_argb_network(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_states_at_all_sizes() {
        for state in [
            IconState::Ok,
            IconState::Syncing,
            IconState::Paused,
            IconState::AuthRequired,
            IconState::Error,
        ] {
            let icons = render(state);
            assert_eq!(icons.len(), SIZES.len());
            for (icon, &size) in icons.iter().zip(SIZES.iter()) {
                assert_eq!(icon.width as u32, size);
                assert_eq!(icon.data.len(), (size * size * 4) as usize);
                // Something must actually be drawn.
                assert!(icon.data.iter().any(|&b| b != 0));
            }
        }
    }

    #[test]
    fn states_produce_distinct_icons() {
        let ok = render(IconState::Ok);
        let err = render(IconState::Error);
        assert_ne!(ok[0].data, err[0].data);
    }

    #[test]
    fn pixels_are_argb_with_transparent_corners() {
        let icon = &render(IconState::Ok)[0];
        // Top-left corner is empty sky — fully transparent alpha byte first.
        assert_eq!(icon.data[0], 0);
    }
}
