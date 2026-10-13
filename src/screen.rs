//! Screenshots of every monitor, prepared for a vision model.
//!
//! Each screenshot is downscaled (models do not need 5K pixels and the upload
//! gets slow), JPEG-compressed, and described with a caption that states its
//! exact pixel size, because the model answers pointing requests in that
//! pixel space.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{DynamicImage, ExtendedColorType};
use xcap::Monitor;

use crate::geometry::Rect;
use crate::input;

/// Longest side of the image sent to the model.
pub const MAX_IMAGE_DIMENSION: u32 = 1280;
const JPEG_QUALITY: u8 = 80;

#[derive(Debug, thiserror::Error)]
pub enum ScreenError {
    #[error("No display was found to capture.")]
    NoDisplay,
    #[error(
        "Could not capture the screen: {0}. On macOS, allow screen recording for this app in \
         System Settings > Privacy & Security."
    )]
    Capture(String),
}

#[derive(Clone)]
pub struct Screenshot {
    pub jpeg: Vec<u8>,
    /// Caption sent next to the image, e.g. "screen 1 of 2 ... (image dimensions: 1280x720 pixels)".
    pub label: String,
    pub is_cursor_screen: bool,
    /// Where this monitor sits on the virtual desktop, in logical points
    /// (the units windows are positioned in).
    pub monitor_rect: Rect,
    pub width_in_pixels: u32,
    pub height_in_pixels: u32,
}

/// Scales (width, height) down so the longer side is at most `maximum`.
/// Never scales up.
pub fn fit_within(width: u32, height: u32, maximum: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= maximum || longest == 0 {
        return (width, height);
    }
    let scale = f64::from(maximum) / f64::from(longest);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// Converts the monitor geometry the OS reports into logical points.
///
/// macOS already reports points. Windows and Linux report physical pixels, so
/// divide by the display scale to match how windows are positioned.
pub fn native_rect_to_logical(
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    scale_factor: f32,
) -> Rect {
    let divisor = if cfg!(target_os = "macos") || scale_factor <= 0.0 {
        1.0
    } else {
        scale_factor
    };
    Rect::new(
        left as f32 / divisor,
        top as f32 / divisor,
        width as f32 / divisor,
        height as f32 / divisor,
    )
}

pub fn make_label(
    screen_index: usize,
    screen_count: usize,
    is_cursor_screen: bool,
    width: u32,
    height: u32,
) -> String {
    let description = if screen_count == 1 {
        "user's screen (cursor is here)".to_string()
    } else if is_cursor_screen {
        format!(
            "screen {} of {screen_count} — cursor is on this screen (primary focus)",
            screen_index + 1
        )
    } else {
        format!(
            "screen {} of {screen_count} — secondary screen",
            screen_index + 1
        )
    };
    format!("{description} (image dimensions: {width}x{height} pixels)")
}

fn encode_jpeg(image: image::RgbaImage) -> Result<Vec<u8>, String> {
    let rgb = DynamicImage::ImageRgba8(image).into_rgb8();
    let mut bytes = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| error.to_string())?;
    Ok(bytes.into_inner())
}

/// Captures every monitor. The one under the mouse cursor comes first.
/// Blocking: call from `spawn_blocking`.
pub fn capture_all_screens() -> Result<Vec<Screenshot>, ScreenError> {
    let monitors = Monitor::all().map_err(|error| ScreenError::Capture(error.to_string()))?;
    if monitors.is_empty() {
        return Err(ScreenError::NoDisplay);
    }
    let cursor = input::cursor_position();

    struct Candidate {
        monitor: Monitor,
        logical_rect: Rect,
        is_cursor_screen: bool,
    }

    let mut candidates: Vec<Candidate> = monitors
        .into_iter()
        .filter_map(|monitor| {
            let left = monitor.x().ok()?;
            let top = monitor.y().ok()?;
            let width = monitor.width().ok()?;
            let height = monitor.height().ok()?;
            let scale_factor = monitor.scale_factor().unwrap_or(1.0);
            let native_rect = Rect::new(left as f32, top as f32, width as f32, height as f32);
            let is_cursor_screen = match cursor {
                Some(point) => native_rect.contains(point),
                None => monitor.is_primary().unwrap_or(false),
            };
            Some(Candidate {
                logical_rect: native_rect_to_logical(left, top, width, height, scale_factor),
                is_cursor_screen,
                monitor,
            })
        })
        .collect();
    // Cursor screen first; keep the OS order otherwise.
    candidates.sort_by_key(|candidate| !candidate.is_cursor_screen);
    // If the cursor was on no monitor (it can sit in a gap), treat the first as focus.
    if !candidates
        .iter()
        .any(|candidate| candidate.is_cursor_screen)
        && let Some(first) = candidates.first_mut()
    {
        first.is_cursor_screen = true;
    }

    let screen_count = candidates.len();
    let mut screenshots = Vec::new();
    let mut last_error = None;
    for (index, candidate) in candidates.into_iter().enumerate() {
        let captured = match candidate.monitor.capture_image() {
            Ok(image) => image,
            Err(error) => {
                tracing::warn!("capturing monitor {index} failed: {error}");
                last_error = Some(error.to_string());
                continue;
            }
        };
        let (width, height) = fit_within(captured.width(), captured.height(), MAX_IMAGE_DIMENSION);
        let resized = if (width, height) == (captured.width(), captured.height()) {
            captured
        } else {
            imageops::resize(&captured, width, height, FilterType::Triangle)
        };
        let jpeg = match encode_jpeg(resized) {
            Ok(jpeg) => jpeg,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        screenshots.push(Screenshot {
            label: make_label(
                index,
                screen_count,
                candidate.is_cursor_screen,
                width,
                height,
            ),
            jpeg,
            is_cursor_screen: candidate.is_cursor_screen,
            monitor_rect: candidate.logical_rect,
            width_in_pixels: width,
            height_in_pixels: height,
        });
    }

    if screenshots.is_empty() {
        return Err(ScreenError::Capture(
            last_error.unwrap_or_else(|| "unknown error".into()),
        ));
    }
    Ok(screenshots)
}

/// Where the screenshot a model pointed at sits, for the overlay to use.
pub fn pick_screenshot(
    screenshots: &[Screenshot],
    screen_number: Option<usize>,
) -> Option<&Screenshot> {
    match screen_number {
        Some(number) if number >= 1 && number <= screenshots.len() => screenshots.get(number - 1),
        _ => screenshots
            .iter()
            .find(|screenshot| screenshot.is_cursor_screen)
            .or_else(|| screenshots.first()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscales_the_long_side_and_keeps_aspect_ratio() {
        assert_eq!(fit_within(2560, 1440, 1280), (1280, 720));
        assert_eq!(fit_within(1440, 2560, 1280), (720, 1280));
        assert_eq!(fit_within(3024, 1964, 1280), (1280, 831)); // a 14" MacBook Pro, as in the old docs
    }

    #[test]
    fn never_upscales() {
        assert_eq!(fit_within(800, 600, 1280), (800, 600));
        assert_eq!(fit_within(1280, 720, 1280), (1280, 720));
    }

    #[test]
    fn labels_say_which_screen_has_the_cursor_and_state_the_pixel_size() {
        assert_eq!(
            make_label(0, 1, true, 1280, 720),
            "user's screen (cursor is here) (image dimensions: 1280x720 pixels)"
        );
        assert!(make_label(0, 2, true, 1280, 720).contains("screen 1 of 2"));
        assert!(make_label(0, 2, true, 1280, 720).contains("primary focus"));
        assert!(make_label(1, 2, false, 1280, 720).contains("screen 2 of 2 — secondary screen"));
        assert!(
            make_label(1, 2, false, 1280, 720).ends_with("(image dimensions: 1280x720 pixels)")
        );
    }

    #[test]
    fn monitor_geometry_is_normalised_to_logical_points() {
        let rect = native_rect_to_logical(3840, 0, 3840, 2160, 2.0);
        if cfg!(target_os = "macos") {
            assert_eq!(rect, Rect::new(3840.0, 0.0, 3840.0, 2160.0));
        } else {
            assert_eq!(rect, Rect::new(1920.0, 0.0, 1920.0, 1080.0));
        }
        // A nonsensical scale factor must not divide by zero.
        assert_eq!(
            native_rect_to_logical(10, 20, 100, 50, 0.0),
            Rect::new(10.0, 20.0, 100.0, 50.0)
        );
    }

    fn fake_screenshot(is_cursor_screen: bool) -> Screenshot {
        Screenshot {
            jpeg: vec![],
            label: String::new(),
            is_cursor_screen,
            monitor_rect: Rect::default(),
            width_in_pixels: 1,
            height_in_pixels: 1,
        }
    }

    #[test]
    fn picks_the_numbered_screen_or_falls_back_to_the_cursor_screen() {
        let screenshots = vec![fake_screenshot(false), fake_screenshot(true)];
        assert!(std::ptr::eq(
            pick_screenshot(&screenshots, Some(1)).unwrap(),
            &screenshots[0]
        ));
        assert!(std::ptr::eq(
            pick_screenshot(&screenshots, None).unwrap(),
            &screenshots[1]
        ));
        // Out-of-range numbers fall back instead of failing.
        assert!(std::ptr::eq(
            pick_screenshot(&screenshots, Some(9)).unwrap(),
            &screenshots[1]
        ));
        assert!(std::ptr::eq(
            pick_screenshot(&screenshots, Some(0)).unwrap(),
            &screenshots[1]
        ));
        assert!(pick_screenshot(&[], None).is_none());
    }

    #[test]
    fn jpeg_encoding_produces_a_valid_smaller_file() {
        let image = image::RgbaImage::from_pixel(64, 48, image::Rgba([10, 120, 240, 255]));
        let jpeg = encode_jpeg(image).unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
        assert!(jpeg.len() < 64 * 48 * 3);
    }
}
