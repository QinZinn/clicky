//! Small 2-D helpers shared by pointing and the overlay.
//!
//! All values are in "logical points" with the origin at the top-left of the
//! virtual desktop, the same space the UI toolkit uses to place windows.
//!
//! `x` and `y` are kept as the field names of `Point` and `Rect` on purpose:
//! they are the universal names for screen coordinates (egui and the OS
//! graphics APIs use them too), and `point.x` is clearer than any longer name.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x_position: f32, y_position: f32) -> Self {
        Self {
            x: x_position,
            y: y_position,
        }
    }

    pub fn distance_to(self, other: Point) -> f32 {
        (other.x - self.x).hypot(other.y - self.y)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x_position: f32, y_position: f32, width: f32, height: f32) -> Self {
        Self {
            x: x_position,
            y: y_position,
            width,
            height,
        }
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x
            && point.x < self.x + self.width
            && point.y >= self.y
            && point.y < self.y + self.height
    }

    /// Nearest point inside the rectangle.
    pub fn clamp(&self, point: Point) -> Point {
        Point::new(
            point.x.clamp(self.x, self.x + self.width),
            point.y.clamp(self.y, self.y + self.height),
        )
    }

    pub fn to_local(&self, point: Point) -> Point {
        Point::new(point.x - self.x, point.y - self.y)
    }
}

/// Maps a coordinate in a screenshot (pixels, origin top-left) onto the
/// virtual desktop. The point is clamped to the screenshot first, because
/// models sometimes answer slightly out of bounds.
pub fn screenshot_point_to_desktop(
    point_in_screenshot: Point,
    screenshot_size_in_pixels: (u32, u32),
    monitor_rect: Rect,
) -> Point {
    let screenshot_width = screenshot_size_in_pixels.0.max(1) as f32;
    let screenshot_height = screenshot_size_in_pixels.1.max(1) as f32;

    let clamped_x = point_in_screenshot.x.clamp(0.0, screenshot_width);
    let clamped_y = point_in_screenshot.y.clamp(0.0, screenshot_height);

    Point::new(
        monitor_rect.x + clamped_x * (monitor_rect.width / screenshot_width),
        monitor_rect.y + clamped_y * (monitor_rect.height / screenshot_height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_screenshot_pixels_to_desktop_points() {
        // A 1280x831 screenshot of a 1512x982-point display at the origin.
        let monitor = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let desktop = screenshot_point_to_desktop(Point::new(640.0, 415.5), (1280, 831), monitor);
        assert!((desktop.x - 756.0).abs() < 0.01);
        assert!((desktop.y - 491.0).abs() < 0.01);
    }

    #[test]
    fn secondary_monitor_offsets_are_applied() {
        // Second monitor to the right and slightly lower.
        let monitor = Rect::new(1920.0, 120.0, 1920.0, 1080.0);
        let desktop = screenshot_point_to_desktop(Point::new(0.0, 0.0), (1280, 720), monitor);
        assert_eq!(desktop, Point::new(1920.0, 120.0));
        let far_corner =
            screenshot_point_to_desktop(Point::new(1280.0, 720.0), (1280, 720), monitor);
        assert_eq!(far_corner, Point::new(3840.0, 1200.0));
    }

    #[test]
    fn out_of_bounds_answers_are_clamped() {
        let monitor = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let desktop = screenshot_point_to_desktop(Point::new(-50.0, 9999.0), (1000, 500), monitor);
        assert_eq!(desktop, Point::new(0.0, 500.0));
    }

    #[test]
    fn rect_clamp_and_local_coordinates() {
        let rect = Rect::new(100.0, 50.0, 200.0, 100.0);
        assert!(rect.contains(Point::new(150.0, 60.0)));
        assert!(!rect.contains(Point::new(300.0, 60.0)));
        assert_eq!(rect.clamp(Point::new(0.0, 500.0)), Point::new(100.0, 150.0));
        assert_eq!(
            rect.to_local(Point::new(120.0, 70.0)),
            Point::new(20.0, 20.0)
        );
    }
}
