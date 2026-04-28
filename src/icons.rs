use eframe::egui;
use egui::{Color32, Painter, Pos2, Rect, Stroke, Vec2, vec2};

const ICON_COLOR: Color32 = Color32::from_gray(220);

/// Draw the speaker + sound waves icon (high volume) into `rect`.
pub fn sound_hi(painter: &Painter, rect: Rect) {
    draw_speaker(painter, rect);
    draw_waves(painter, rect, 3);
}

/// Draw the speaker only (low / mute) into `rect`.
pub fn sound_lo(painter: &Painter, rect: Rect) {
    // Speaker cone ends at x≈39.389 in the 75-unit SVG space; the remaining 47.5%
    // would be empty. Shift most, but not all, of that dead space rightward so the
    // icon sits closer to the slider without looking visually detached.
    let shift = rect.width() * (1.0 - 39.389 / 75.0) * 0.72;
    let shifted = Rect::from_min_size(egui::pos2(rect.min.x + shift, rect.min.y), rect.size());
    draw_speaker(painter, shifted);
}

/// Draw the speaker with an X (muted) into `rect`.
pub fn sound_muted(painter: &Painter, rect: Rect) {
    draw_speaker(painter, rect);
    let sw = stroke(rect);
    painter.line_segment([map(Pos2::new(47.0, 22.0), rect), map(Pos2::new(67.0, 58.0), rect)], sw);
    painter.line_segment([map(Pos2::new(67.0, 22.0), rect), map(Pos2::new(47.0, 58.0), rect)], sw);
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// Maps a point from the SVG coordinate space (0..75 x 0..75) into `rect`.
fn map(p: Pos2, rect: Rect) -> Pos2 {
    // The speaker shape occupies roughly x:6..40, y:14..63 in the 75×75 SVG grid.
    let svg = Vec2::new(75.0, 75.0);
    Pos2::new(
        rect.left() + p.x / svg.x * rect.width(),
        rect.top() + p.y / svg.y * rect.height(),
    )
}

fn stroke(rect: Rect) -> Stroke {
    // Scale stroke width proportionally to the icon size.
    Stroke::new(rect.width() * 5.0 / 75.0, ICON_COLOR)
}

fn draw_speaker(painter: &Painter, rect: Rect) {
    // Polygon: 39.389,13.769 → 22.235,28.606 → 6,28.606 → 6,47.699
    //          → 21.989,47.699 → 39.389,62.75 → close
    let points: Vec<Pos2> = [
        (39.389, 13.769),
        (22.235, 28.606),
        (6.0, 28.606),
        (6.0, 47.699),
        (21.989, 47.699),
        (39.389, 62.75),
    ]
    .iter()
    .map(|&(x, y)| map(Pos2::new(x, y), rect))
    .collect();

    painter.add(egui::Shape::convex_polygon(
        points,
        ICON_COLOR,
        stroke(rect),
    ));
}

/// Draw 1, 2, or 3 arcs representing sound waves.
fn draw_waves(painter: &Painter, rect: Rect, count: usize) {
    // Wave centres from the SVG (x start, y top, y bottom):
    // Small:  x=48, y=27.6..49     → arc radius ~10.7
    // Medium: x=55.1, y=20.5..56.1 → arc radius ~17.8
    // Large:  x=61.6, y=14..62.6   → arc radius ~24.3
    let waves: &[(f32, f32, f32)] = &[(48.0, 27.6, 49.0), (55.1, 20.5, 56.1), (61.6, 14.0, 62.6)];

    let sw = stroke(rect);

    for &(ax, y_top, y_bot) in waves.iter().take(count) {
        let cy = (y_top + y_bot) / 2.0;
        let r = (y_bot - y_top) / 2.0;

        // Approximate the arc with a cubic bezier.
        // Control-point offset for a ~180° arc: k = r * 4/3 * tan(π/4) ≈ r * 0.5523
        let k = r * 0.5523;

        let top = map(Pos2::new(ax, cy - r), rect);
        let bottom = map(Pos2::new(ax, cy + r), rect);
        let center = map(Pos2::new(ax, cy), rect);

        // Scale k into screen space (use height scale).
        let k_screen = k / 75.0 * rect.height();
        let right_offset = vec2(k_screen, 0.0);

        painter.add(egui::Shape::CubicBezier(egui::epaint::CubicBezierShape {
            points: [
                top,
                center + vec2(right_offset.x, -right_offset.x),
                center + vec2(right_offset.x, right_offset.x),
                bottom,
            ],
            closed: false,
            fill: Color32::TRANSPARENT,
            stroke: sw.into(),
        }));
    }
}

/// Returns the size to allocate for a speaker icon in a toolbar of the given height.
pub fn icon_size(height: f32) -> Vec2 {
    vec2(height * 0.78, height * 0.86)
}
