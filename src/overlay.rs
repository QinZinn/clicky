//! The blue cursor that flies to whatever the model points at.
//!
//! [`PointerFlight`] is a pure animation: give it the time and it says where
//! the cursor is. That keeps it testable without a window. [`paint`] draws one
//! frame with egui.
//!
//! Choreography (ported from the original macOS app): the cursor takes off
//! from the real mouse position, flies along a quadratic arc to the target
//! (growing a little mid-flight), a speech bubble types out next to it, it
//! holds for a few seconds, then flies back to the mouse and disappears.

use eframe::egui;

use crate::engine::PointRequest;
use crate::geometry::{Point, Rect};

pub const CURSOR_BLUE: (u8, u8, u8) = (0x33, 0x80, 0xFF);

/// Resting angle of the arrow once it has arrived (a classic cursor tilt).
const RESTING_ROTATION_DEGREES: f32 = -35.0;
/// Seconds per character while the bubble types out.
const SECONDS_PER_CHARACTER: f32 = 0.045;
const BUBBLE_HOLD_SECONDS: f32 = 3.0;
const BUBBLE_FADE_SECONDS: f32 = 0.5;
/// Keep the cursor this far from the monitor edges.
const EDGE_PADDING: f32 = 20.0;
/// Sit beside the element rather than on top of it.
const TARGET_OFFSET: Point = Point::new(8.0, 12.0);
/// Where the cursor lands relative to the real mouse pointer when returning.
const RETURN_OFFSET: Point = Point::new(14.0, 14.0);

const FALLBACK_PHRASES: [&str; 6] = [
    "right here!",
    "this one!",
    "over here!",
    "click this!",
    "here it is!",
    "found it!",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Outbound,
    Pointing,
    Returning,
    Finished,
}

/// Everything needed to draw one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FlightFrame {
    /// Cursor centre, relative to the monitor's top-left corner.
    pub position: Point,
    pub rotation_degrees: f32,
    pub scale: f32,
    /// 0.0..=1.0, for the final fade-out.
    pub cursor_opacity: f32,
    pub bubble_text: String,
    pub bubble_opacity: f32,
    pub finished: bool,
}

#[derive(Clone, Debug)]
pub struct PointerFlight {
    monitor_rect: Rect,
    start: Point,
    target: Point,
    bubble_phrase: String,
    phase: Phase,
    phase_started_at: f32,
    outbound_seconds: f32,
    return_to: Point,
    return_seconds: f32,
}

fn flight_seconds(from: Point, to: Point, minimum: f32) -> f32 {
    (from.distance_to(to) / 800.0).clamp(minimum, 1.4)
}

/// Smoothstep ease-in-out.
fn ease(progress: f32) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    progress * progress * (3.0 - 2.0 * progress)
}

impl PointerFlight {
    /// `now` is any monotonic clock in seconds; `cursor` is the real mouse
    /// position on the desktop, if known.
    pub fn new(request: &PointRequest, cursor: Option<Point>, now: f32) -> Self {
        let monitor_rect = request.monitor_rect;
        let local_bounds = Rect::new(0.0, 0.0, monitor_rect.width, monitor_rect.height);

        let start = match cursor {
            // The mouse may be on another monitor: start from the nearest edge.
            Some(cursor) => local_bounds.clamp(monitor_rect.to_local(cursor)),
            None => Point::new(monitor_rect.width / 2.0, monitor_rect.height / 2.0),
        };

        let local_target = monitor_rect.to_local(request.target);
        let target = clamp_with_padding(
            Point::new(
                local_target.x + TARGET_OFFSET.x,
                local_target.y + TARGET_OFFSET.y,
            ),
            local_bounds,
        );

        let label = request.label.trim();
        let bubble_phrase = if label.is_empty() || label.eq_ignore_ascii_case("none") {
            let index = (request.target.x.abs() as usize + request.target.y.abs() as usize)
                % FALLBACK_PHRASES.len();
            FALLBACK_PHRASES[index].to_string()
        } else {
            label.to_string()
        };

        Self {
            monitor_rect,
            start,
            target,
            bubble_phrase,
            phase: Phase::Outbound,
            phase_started_at: now,
            outbound_seconds: flight_seconds(start, target, 0.6),
            return_to: start,
            return_seconds: 0.6,
        }
    }

    pub fn monitor_rect(&self) -> Rect {
        self.monitor_rect
    }

    fn typing_seconds(&self) -> f32 {
        self.bubble_phrase.chars().count() as f32 * SECONDS_PER_CHARACTER
    }

    fn pointing_seconds(&self) -> f32 {
        self.typing_seconds() + BUBBLE_HOLD_SECONDS + BUBBLE_FADE_SECONDS
    }

    /// Advances the animation to `now` and returns the frame to draw.
    /// `cursor` is the current mouse position, used to aim the return flight.
    pub fn frame(&mut self, now: f32, cursor: Option<Point>) -> FlightFrame {
        // Catch up through every phase boundary crossed since the last call,
        // so a long gap between frames cannot skip the bubble.
        loop {
            let elapsed = now - self.phase_started_at;
            match self.phase {
                Phase::Outbound if elapsed >= self.outbound_seconds => {
                    self.phase_started_at += self.outbound_seconds;
                    self.phase = Phase::Pointing;
                }
                Phase::Pointing if elapsed >= self.pointing_seconds() => {
                    self.phase_started_at += self.pointing_seconds();
                    self.begin_return(cursor);
                }
                Phase::Returning if elapsed >= self.return_seconds => {
                    self.phase_started_at += self.return_seconds;
                    self.phase = Phase::Finished;
                }
                _ => break,
            }
        }
        let elapsed = (now - self.phase_started_at).max(0.0);

        match self.phase {
            Phase::Outbound => {
                let progress = elapsed / self.outbound_seconds;
                self.flying_frame(self.start, self.target, progress, 1.0)
            }
            Phase::Pointing => {
                let typed = ((elapsed / SECONDS_PER_CHARACTER) as usize)
                    .min(self.bubble_phrase.chars().count());
                let fade_starts = self.typing_seconds() + BUBBLE_HOLD_SECONDS;
                let bubble_opacity = if elapsed < fade_starts {
                    (elapsed / 0.15).min(1.0)
                } else {
                    1.0 - ((elapsed - fade_starts) / BUBBLE_FADE_SECONDS).clamp(0.0, 1.0)
                };
                FlightFrame {
                    position: self.target,
                    rotation_degrees: RESTING_ROTATION_DEGREES,
                    scale: 1.0,
                    cursor_opacity: 1.0,
                    bubble_text: self.bubble_phrase.chars().take(typed).collect(),
                    bubble_opacity,
                    finished: false,
                }
            }
            Phase::Returning => {
                let progress = elapsed / self.return_seconds;
                // Fade out over the last 20% so it does not vanish abruptly.
                let opacity = if progress > 0.8 {
                    1.0 - (progress - 0.8) / 0.2
                } else {
                    1.0
                };
                self.flying_frame(self.target, self.return_to, progress, opacity)
            }
            Phase::Finished => FlightFrame {
                position: self.return_to,
                rotation_degrees: RESTING_ROTATION_DEGREES,
                scale: 1.0,
                cursor_opacity: 0.0,
                bubble_text: String::new(),
                bubble_opacity: 0.0,
                finished: true,
            },
        }
    }

    fn begin_return(&mut self, cursor: Option<Point>) {
        let local_bounds = Rect::new(0.0, 0.0, self.monitor_rect.width, self.monitor_rect.height);
        self.return_to = match cursor {
            Some(cursor) => {
                let local = self.monitor_rect.to_local(cursor);
                local_bounds.clamp(Point::new(
                    local.x + RETURN_OFFSET.x,
                    local.y + RETURN_OFFSET.y,
                ))
            }
            None => self.start,
        };
        self.return_seconds = flight_seconds(self.target, self.return_to, 0.5);
        self.phase = Phase::Returning;
    }

    fn flying_frame(&self, from: Point, to: Point, progress: f32, opacity: f32) -> FlightFrame {
        let progress = progress.clamp(0.0, 1.0);
        let eased = ease(progress);

        // Quadratic Bezier whose control point is lifted above the midpoint,
        // so the cursor swoops instead of travelling in a straight line.
        let distance = from.distance_to(to);
        let arc_height = (distance * 0.2).min(80.0);
        let control = Point::new((from.x + to.x) / 2.0, (from.y + to.y) / 2.0 - arc_height);

        let one_minus = 1.0 - eased;
        let position = Point::new(
            one_minus * one_minus * from.x
                + 2.0 * one_minus * eased * control.x
                + eased * eased * to.x,
            one_minus * one_minus * from.y
                + 2.0 * one_minus * eased * control.y
                + eased * eased * to.y,
        );

        // Face the direction of travel: the tangent of the curve.
        let tangent_x = 2.0 * one_minus * (control.x - from.x) + 2.0 * eased * (to.x - control.x);
        let tangent_y = 2.0 * one_minus * (control.y - from.y) + 2.0 * eased * (to.y - control.y);
        // The arrow's tip points up at 0 degrees, while atan2 gives 0 for "right".
        let rotation_degrees = tangent_y.atan2(tangent_x).to_degrees() + 90.0;

        FlightFrame {
            position,
            rotation_degrees,
            scale: 1.0 + (progress * std::f32::consts::PI).sin() * 0.3,
            cursor_opacity: opacity,
            bubble_text: String::new(),
            bubble_opacity: 0.0,
            finished: false,
        }
    }
}

fn clamp_with_padding(point: Point, bounds: Rect) -> Point {
    let padding = EDGE_PADDING
        .min(bounds.width / 2.0)
        .min(bounds.height / 2.0);
    Point::new(
        point
            .x
            .clamp(bounds.x + padding, bounds.x + bounds.width - padding),
        point
            .y
            .clamp(bounds.y + padding, bounds.y + bounds.height - padding),
    )
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Draws one frame. `origin` is the top-left of the overlay window's drawing area.
pub fn paint(painter: &egui::Painter, origin: egui::Pos2, frame: &FlightFrame) {
    use egui::{Color32, CornerRadius, FontId, Pos2, Shape, Stroke, Vec2};

    if frame.finished || frame.cursor_opacity <= 0.0 {
        return;
    }
    let (red, green, blue) = CURSOR_BLUE;
    let blue_color = Color32::from_rgb(red, green, blue);
    let centre = Pos2::new(origin.x + frame.position.x, origin.y + frame.position.y);
    let opacity = frame.cursor_opacity.clamp(0.0, 1.0);

    // Soft glow that swells during flight.
    let glow_boost = (frame.scale - 1.0) * 40.0;
    for (radius, strength) in [(20.0, 0.06), (14.0, 0.10), (9.0, 0.18)] {
        painter.circle_filled(
            centre,
            (radius + glow_boost) * frame.scale.max(1.0),
            blue_color.gamma_multiply(strength * opacity),
        );
    }

    // Equilateral triangle, same proportions as the original arrow.
    let size = 16.0 * frame.scale;
    let height = size * 3.0_f32.sqrt() / 2.0;
    let corners = [
        Vec2::new(0.0, -height / 1.5),
        Vec2::new(-size / 2.0, height / 3.0),
        Vec2::new(size / 2.0, height / 3.0),
    ];
    let (sin, cos) = frame.rotation_degrees.to_radians().sin_cos();
    let points: Vec<Pos2> = corners
        .iter()
        .map(|corner| {
            centre
                + Vec2::new(
                    corner.x * cos - corner.y * sin,
                    corner.x * sin + corner.y * cos,
                )
        })
        .collect();
    painter.add(Shape::convex_polygon(
        points,
        blue_color.gamma_multiply(opacity),
        Stroke::NONE,
    ));

    if !frame.bubble_text.is_empty() && frame.bubble_opacity > 0.0 {
        let bubble_opacity = frame.bubble_opacity.clamp(0.0, 1.0);
        let galley = painter.layout_no_wrap(
            frame.bubble_text.clone(),
            FontId::proportional(14.0),
            Color32::WHITE.gamma_multiply(bubble_opacity),
        );
        let padding = Vec2::new(10.0, 6.0);
        let size = galley.size() + padding * 2.0;
        let mut top_left = centre + Vec2::new(12.0, 16.0);

        // Keep the bubble on this monitor: flip to the other side of the
        // cursor if it would run off the edge.
        let bounds = painter.clip_rect();
        if top_left.x + size.x > bounds.right() {
            top_left.x = centre.x - 12.0 - size.x;
        }
        if top_left.y + size.y > bounds.bottom() {
            top_left.y = centre.y - 16.0 - size.y;
        }
        let bubble_rect = egui::Rect::from_min_size(top_left, size);
        painter.rect_filled(
            bubble_rect,
            CornerRadius::same(8),
            blue_color.gamma_multiply(bubble_opacity),
        );
        painter.galley(top_left + padding, galley, Color32::WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: Point) -> PointRequest {
        // A 1000x600 monitor whose top-left is at (200, 100) on the desktop.
        PointRequest {
            target,
            monitor_rect: Rect::new(200.0, 100.0, 1000.0, 600.0),
            label: "save button".into(),
        }
    }

    fn approx(value: f32, expected: f32) -> bool {
        (value - expected).abs() < 0.01
    }

    #[test]
    fn flight_starts_at_the_mouse_and_lands_beside_the_target() {
        // Mouse at desktop (300, 200) = local (100, 100); target desktop (700, 400) = local (500, 300).
        let mut flight = PointerFlight::new(
            &request(Point::new(700.0, 400.0)),
            Some(Point::new(300.0, 200.0)),
            0.0,
        );

        let start = flight.frame(0.0, None);
        assert!(
            approx(start.position.x, 100.0) && approx(start.position.y, 100.0),
            "{start:?}"
        );
        assert!(!start.finished);

        let landed = flight.frame(flight.outbound_seconds, None);
        // Local target + (8, 12) offset.
        assert!(
            approx(landed.position.x, 508.0) && approx(landed.position.y, 312.0),
            "{landed:?}"
        );
    }

    #[test]
    fn flies_in_an_arc_above_the_straight_line() {
        let mut flight = PointerFlight::new(
            &request(Point::new(1000.0, 100.0)),
            Some(Point::new(200.0, 100.0)),
            0.0,
        );
        let midway = flight.frame(flight.outbound_seconds / 2.0, None);
        // Start and end are on the same row (y=0 local + offset 12), the arc rises above it.
        assert!(
            midway.position.y < 12.0 - 10.0,
            "arc should lift the cursor: {midway:?}"
        );
        assert!(midway.scale > 1.25, "should swell mid-flight: {midway:?}");
    }

    #[test]
    fn faces_the_direction_of_travel() {
        // Flat rightward flight (tiny arc): rotation near 90 degrees at the start... check the
        // general sense instead: rightwards travel gives a rotation in the right half-plane.
        let mut right = PointerFlight::new(
            &request(Point::new(1100.0, 400.0)),
            Some(Point::new(250.0, 400.0)),
            0.0,
        );
        let late = right.frame(right.outbound_seconds * 0.9, None);
        assert!(
            (60.0..130.0).contains(&late.rotation_degrees),
            "{}",
            late.rotation_degrees
        );

        // Downward flight points the tip down (about 180 degrees).
        let mut down = PointerFlight::new(
            &request(Point::new(600.0, 690.0)),
            Some(Point::new(600.0, 110.0)),
            0.0,
        );
        let late = down.frame(down.outbound_seconds * 0.9, None);
        assert!(
            (140.0..220.0).contains(&late.rotation_degrees),
            "{}",
            late.rotation_degrees
        );
    }

    #[test]
    fn flight_time_scales_with_distance_but_is_clamped() {
        assert!(approx(
            flight_seconds(Point::new(0.0, 0.0), Point::new(10.0, 0.0), 0.6),
            0.6
        ));
        assert!(approx(
            flight_seconds(Point::new(0.0, 0.0), Point::new(5000.0, 0.0), 0.6),
            1.4
        ));
        assert!(approx(
            flight_seconds(Point::new(0.0, 0.0), Point::new(1000.0, 0.0), 0.6),
            1.25
        ));
    }

    #[test]
    fn the_bubble_types_out_holds_then_fades() {
        let mut flight = PointerFlight::new(
            &request(Point::new(700.0, 400.0)),
            Some(Point::new(300.0, 200.0)),
            0.0,
        );
        let arrival = flight.outbound_seconds;

        let before = flight.frame(arrival - 0.01, None);
        assert_eq!(before.bubble_text, "");

        let partway = flight.frame(arrival + 5.0 * SECONDS_PER_CHARACTER + 0.001, None);
        assert_eq!(partway.bubble_text, "save ");
        assert_eq!(partway.rotation_degrees, RESTING_ROTATION_DEGREES);

        let holding = flight.frame(arrival + flight.typing_seconds() + 1.0, None);
        assert_eq!(holding.bubble_text, "save button");
        assert!(approx(holding.bubble_opacity, 1.0));

        let fading = flight.frame(
            arrival + flight.typing_seconds() + BUBBLE_HOLD_SECONDS + BUBBLE_FADE_SECONDS / 2.0,
            None,
        );
        assert!(
            fading.bubble_opacity > 0.4 && fading.bubble_opacity < 0.6,
            "{}",
            fading.bubble_opacity
        );
    }

    #[test]
    fn returns_to_wherever_the_mouse_is_now_and_then_finishes() {
        let mut flight = PointerFlight::new(
            &request(Point::new(700.0, 400.0)),
            Some(Point::new(300.0, 200.0)),
            0.0,
        );
        let pointing_done = flight.outbound_seconds + flight.pointing_seconds();

        // The user moved the mouse to desktop (400, 300) = local (200, 200) while we pointed.
        let returning = flight.frame(pointing_done + 0.01, Some(Point::new(400.0, 300.0)));
        assert_eq!(returning.bubble_text, "");
        assert!(!returning.finished);

        let landed = flight.frame(
            pointing_done + flight.return_seconds - 0.001,
            Some(Point::new(400.0, 300.0)),
        );
        assert!(
            approx(landed.position.x, 214.0) && approx(landed.position.y, 214.0),
            "{landed:?}"
        );

        let done = flight.frame(pointing_done + flight.return_seconds + 0.1, None);
        assert!(done.finished);
        assert_eq!(done.cursor_opacity, 0.0);
    }

    #[test]
    fn a_long_gap_between_frames_still_walks_through_every_phase() {
        let mut flight = PointerFlight::new(
            &request(Point::new(700.0, 400.0)),
            Some(Point::new(300.0, 200.0)),
            0.0,
        );
        // Jump straight to the very end.
        assert!(flight.frame(60.0, None).finished);
    }

    #[test]
    fn targets_near_an_edge_are_kept_on_the_monitor() {
        // Desktop (1199, 699) is the monitor's bottom-right corner.
        let mut flight = PointerFlight::new(
            &request(Point::new(1199.0, 699.0)),
            Some(Point::new(300.0, 200.0)),
            0.0,
        );
        let landed = flight.frame(flight.outbound_seconds, None);
        assert!(
            approx(landed.position.x, 980.0) && approx(landed.position.y, 580.0),
            "{landed:?}"
        );
    }

    #[test]
    fn a_mouse_on_another_monitor_starts_the_flight_from_the_nearest_edge() {
        // Mouse far to the left of this monitor.
        let mut flight = PointerFlight::new(
            &request(Point::new(700.0, 400.0)),
            Some(Point::new(-500.0, 300.0)),
            0.0,
        );
        let start = flight.frame(0.0, None);
        assert!(
            approx(start.position.x, 0.0) && approx(start.position.y, 200.0),
            "{start:?}"
        );
    }

    #[test]
    fn unknown_mouse_position_starts_from_the_centre() {
        let mut flight = PointerFlight::new(&request(Point::new(700.0, 400.0)), None, 0.0);
        let start = flight.frame(0.0, None);
        assert!(approx(start.position.x, 500.0) && approx(start.position.y, 300.0));
    }

    #[test]
    fn missing_or_none_labels_get_a_friendly_phrase() {
        for label in ["", "  ", "none", "NONE"] {
            let mut request = request(Point::new(700.0, 400.0));
            request.label = label.into();
            let flight = PointerFlight::new(&request, None, 0.0);
            assert!(
                FALLBACK_PHRASES.contains(&flight.bubble_phrase.as_str()),
                "{label:?} -> {}",
                flight.bubble_phrase
            );
        }
    }
}
