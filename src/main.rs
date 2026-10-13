use clicky::app::{self, ClickyApp};
use eframe::egui;

type PixelPoint = (f32, f32);

/// Which side of the line `line_start -> line_end` a point is on (the sign of
/// the cross product). Zero means exactly on the line.
fn side_of_line(line_start: PixelPoint, line_end: PixelPoint, point: PixelPoint) -> f32 {
    (line_end.0 - line_start.0) * (point.1 - line_start.1)
        - (line_end.1 - line_start.1) * (point.0 - line_start.0)
}

fn is_inside_triangle(corners: &[PixelPoint; 3], point: PixelPoint) -> bool {
    let sides = [
        side_of_line(corners[0], corners[1], point),
        side_of_line(corners[1], corners[2], point),
        side_of_line(corners[2], corners[0], point),
    ];
    sides.iter().all(|side| *side >= 0.0) || sides.iter().all(|side| *side <= 0.0)
}

/// Signed-distance test for a square with rounded corners centred at `centre`.
fn is_inside_rounded_square(
    point: PixelPoint,
    centre: PixelPoint,
    half_side: f32,
    corner_radius: f32,
) -> bool {
    let inner_half_side = half_side - corner_radius;
    let horizontal_overshoot = (point.0 - centre.0).abs() - inner_half_side;
    let vertical_overshoot = (point.1 - centre.1).abs() - inner_half_side;
    let distance_outside_inner_square = horizontal_overshoot
        .max(0.0)
        .hypot(vertical_overshoot.max(0.0))
        + horizontal_overshoot.max(vertical_overshoot).min(0.0);
    distance_outside_inner_square <= corner_radius
}

/// A 64x64 window/taskbar icon: the blue arrow on a dark rounded square.
fn window_icon() -> egui::IconData {
    const ICON_SIZE: u32 = 64;
    const ARROW_BLUE: [u8; 4] = [0x33, 0x80, 0xFF, 0xFF];
    const BACKGROUND: [u8; 4] = [0x16, 0x1a, 0x21, 0xFF];
    const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

    // Arrow corners in the same proportions as the on-screen cursor.
    let arrow_centre: PixelPoint = (32.0, 34.0);
    let arrow_side = 34.0_f32;
    let arrow_height = arrow_side * 3.0_f32.sqrt() / 2.0;
    let arrow_corners: [PixelPoint; 3] = [
        (arrow_centre.0, arrow_centre.1 - arrow_height / 1.5),
        (
            arrow_centre.0 - arrow_side / 2.0,
            arrow_centre.1 + arrow_height / 3.0,
        ),
        (
            arrow_centre.0 + arrow_side / 2.0,
            arrow_centre.1 + arrow_height / 3.0,
        ),
    ];

    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for row in 0..ICON_SIZE {
        for column in 0..ICON_SIZE {
            let pixel_centre = (column as f32 + 0.5, row as f32 + 0.5);
            let pixel = if is_inside_triangle(&arrow_corners, pixel_centre) {
                ARROW_BLUE
            } else if is_inside_rounded_square(pixel_centre, (32.0, 32.0), 32.0, 8.0) {
                BACKGROUND
            } else {
                TRANSPARENT
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    egui::IconData {
        rgba,
        width: ICON_SIZE,
        height: ICON_SIZE,
    }
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("clicky=info,warn")),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Clicky")
            .with_app_id("clicky")
            .with_inner_size([460.0, 740.0])
            .with_min_inner_size([380.0, 520.0])
            .with_icon(std::sync::Arc::new(window_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "Clicky",
        options,
        Box::new(|creation_context| {
            app::apply_theme(&creation_context.egui_ctx);
            Ok(Box::new(ClickyApp::launch(creation_context)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel_at(icon: &egui::IconData, column: u32, row: u32) -> [u8; 4] {
        let start = ((row * icon.width + column) * 4) as usize;
        icon.rgba[start..start + 4].try_into().unwrap()
    }

    #[test]
    fn the_icon_is_a_blue_arrow_on_a_dark_rounded_square() {
        let icon = window_icon();
        assert_eq!((icon.width, icon.height), (64, 64));
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);

        assert_eq!(
            pixel_at(&icon, 32, 36),
            [0x33, 0x80, 0xFF, 0xFF],
            "inside the arrow"
        );
        assert_eq!(
            pixel_at(&icon, 6, 32),
            [0x16, 0x1a, 0x21, 0xFF],
            "dark square beside the arrow"
        );
        assert_eq!(
            pixel_at(&icon, 0, 0),
            [0, 0, 0, 0],
            "the rounded corner is transparent"
        );
    }
}
