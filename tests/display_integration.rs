//! Tests that need a real (or virtual) X11 display, such as the one provided
//! by `xvfb-run`. They skip themselves when `DISPLAY` is not set, so a plain
//! `cargo test` on a headless machine still passes.
//!
//! Run them for real with:  xvfb-run -a cargo test --test display_integration -- --test-threads=1

use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use clicky::config::HotkeyChoice;
use clicky::input::{self, HotkeyEvent, SharedHotkeyChoice};
use clicky::screen;

fn has_display() -> bool {
    let available = std::env::var_os("DISPLAY").is_some();
    if !available {
        eprintln!("skipping: no DISPLAY (run under xvfb-run to exercise this test)");
    }
    available
}

fn xdotool(arguments: &[&str]) -> bool {
    Command::new("xdotool")
        .args(arguments)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn captures_the_screen_as_a_labelled_jpeg() {
    if !has_display() {
        return;
    }
    let screenshots = screen::capture_all_screens().expect("capture should work on an X11 display");
    assert!(!screenshots.is_empty());

    let first = &screenshots[0];
    assert!(first.is_cursor_screen, "the focus screen must come first");
    assert_eq!(&first.jpeg[..3], &[0xFF, 0xD8, 0xFF], "not a JPEG");
    assert!(first.width_in_pixels.max(first.height_in_pixels) <= screen::MAX_IMAGE_DIMENSION);
    assert!(first.label.contains(&format!(
        "image dimensions: {}x{} pixels",
        first.width_in_pixels, first.height_in_pixels
    )));
    assert!(first.monitor_rect.width > 0.0 && first.monitor_rect.height > 0.0);

    // The JPEG really decodes to the size we advertise.
    let decoded = image::load_from_memory(&first.jpeg).expect("valid jpeg");
    assert_eq!(
        (decoded.width(), decoded.height()),
        (first.width_in_pixels, first.height_in_pixels)
    );
}

/// Asks the X server where the pointer is, independently of our code.
fn pointer_position_according_to_x_server() -> Option<(f32, f32)> {
    let output = Command::new("xdotool")
        .arg("getmouselocation")
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let mut coordinates = text.split_whitespace().filter_map(|field| {
        field
            .strip_prefix("x:")
            .or_else(|| field.strip_prefix("y:"))?
            .parse::<f32>()
            .ok()
    });
    Some((coordinates.next()?, coordinates.next()?))
}

#[test]
fn reports_the_same_mouse_position_as_the_x_server() {
    if !has_display() {
        return;
    }
    let Some(expected) = pointer_position_according_to_x_server() else {
        eprintln!("skipping: xdotool is not installed");
        return;
    };
    let position = input::cursor_position().expect("cursor position");
    assert_eq!((position.x, position.y), expected);
}

#[test]
fn the_push_to_talk_shortcut_fires_on_real_key_presses() {
    if !has_display() || !xdotool(&["keyup", "ctrl", "alt"]) {
        return;
    }
    let (sender, receiver) = mpsc::channel();
    input::spawn_hotkey_listener(
        SharedHotkeyChoice::new(HotkeyChoice::CtrlAlt),
        move |event| {
            let _ = sender.send(event);
        },
    )
    .expect("hotkey listener should start on an X11 display");

    // Hold Ctrl+Alt long enough to pass the debounce.
    assert!(xdotool(&["keydown", "ctrl", "keydown", "alt"]));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(2)),
        Ok(HotkeyEvent::Pressed)
    );
    // No repeat events while held.
    assert!(receiver.recv_timeout(Duration::from_millis(300)).is_err());
    assert!(xdotool(&["keyup", "ctrl", "keyup", "alt"]));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(2)),
        Ok(HotkeyEvent::Released)
    );

    // A quick Ctrl+Alt+T chord (like a terminal shortcut) must NOT start recording.
    assert!(xdotool(&["key", "ctrl+alt+t"]));
    assert!(receiver.recv_timeout(Duration::from_millis(500)).is_err());
}
