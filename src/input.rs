//! Global input: the push-to-talk shortcut and the mouse position.
//!
//! Both work by polling the OS ("is Ctrl held right now?") instead of
//! installing keyboard hooks. Polling needs no special event-loop access, so
//! it works while another application has focus.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use device_query::{DeviceQuery, DeviceState, Keycode};

use crate::config::HotkeyChoice;
use crate::geometry::Point;

// ---------------------------------------------------------------------------
// Mouse position
// ---------------------------------------------------------------------------

thread_local! {
    // DeviceState is not Send on Linux, so each thread keeps its own.
    static DEVICE_STATE: Option<DeviceState> = DeviceState::checked_new();
}

/// Current mouse position in the OS's native desktop coordinates (the same
/// space screen capture reports monitor positions in).
pub fn cursor_position() -> Option<Point> {
    DEVICE_STATE.with(|device_state| {
        device_state.as_ref().map(|device_state| {
            let (x_position, y_position) = device_state.get_mouse().coords;
            Point::new(x_position as f32, y_position as f32)
        })
    })
}

// ---------------------------------------------------------------------------
// Push-to-talk shortcut
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModifierSnapshot {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    /// A non-modifier key (a letter, Delete, ...) is held.
    pub other_key: bool,
}

impl ModifierSnapshot {
    pub fn from_keys(keys: &[Keycode]) -> Self {
        let mut snapshot = ModifierSnapshot::default();
        for key in keys {
            match key {
                Keycode::LControl | Keycode::RControl => snapshot.control = true,
                Keycode::LAlt | Keycode::RAlt | Keycode::LOption | Keycode::ROption => {
                    snapshot.alt = true
                }
                Keycode::LShift | Keycode::RShift => snapshot.shift = true,
                // Command / Windows key: neither part of our shortcuts nor a "real" key.
                Keycode::Command | Keycode::RCommand | Keycode::LMeta | Keycode::RMeta => {}
                _ => snapshot.other_key = true,
            }
        }
        snapshot
    }

    fn satisfies(&self, choice: HotkeyChoice) -> bool {
        match choice {
            HotkeyChoice::CtrlAlt => self.control && self.alt,
            HotkeyChoice::CtrlShift => self.control && self.shift,
            HotkeyChoice::AltShift => self.alt && self.shift,
            HotkeyChoice::Disabled => false,
        }
    }
}

/// Turns raw "which keys are down" snapshots into press/release events.
///
/// The shortcut only counts as pressed after it has been held for
/// `hold_delay` with no other key involved. That keeps ordinary shortcuts
/// that happen to include the same modifiers (Ctrl+Alt+T, Ctrl+Alt+Delete)
/// from starting a recording.
pub struct HotkeyDetector {
    hold_delay: Duration,
    held_since: Option<Instant>,
    is_active: bool,
}

impl HotkeyDetector {
    pub fn new(hold_delay: Duration) -> Self {
        Self {
            hold_delay,
            held_since: None,
            is_active: false,
        }
    }

    pub fn update(
        &mut self,
        now: Instant,
        choice: HotkeyChoice,
        modifiers: ModifierSnapshot,
    ) -> Option<HotkeyEvent> {
        let combination_is_down = modifiers.satisfies(choice);

        if self.is_active {
            // Once talking, extra keys do not interrupt; letting go of the
            // modifiers (or changing the shortcut) ends it.
            if !combination_is_down {
                self.is_active = false;
                self.held_since = None;
                return Some(HotkeyEvent::Released);
            }
            return None;
        }

        if combination_is_down && !modifiers.other_key {
            let held_since = *self.held_since.get_or_insert(now);
            if now.duration_since(held_since) >= self.hold_delay {
                self.is_active = true;
                return Some(HotkeyEvent::Pressed);
            }
        } else {
            self.held_since = None;
        }
        None
    }
}

/// Shared, live-updatable shortcut choice for the polling thread.
#[derive(Clone)]
pub struct SharedHotkeyChoice(Arc<AtomicU8>);

impl SharedHotkeyChoice {
    pub fn new(choice: HotkeyChoice) -> Self {
        Self(Arc::new(AtomicU8::new(Self::encode(choice))))
    }

    pub fn set(&self, choice: HotkeyChoice) {
        self.0.store(Self::encode(choice), Ordering::Relaxed);
    }

    pub fn get(&self) -> HotkeyChoice {
        match self.0.load(Ordering::Relaxed) {
            0 => HotkeyChoice::CtrlAlt,
            1 => HotkeyChoice::CtrlShift,
            2 => HotkeyChoice::AltShift,
            _ => HotkeyChoice::Disabled,
        }
    }

    fn encode(choice: HotkeyChoice) -> u8 {
        match choice {
            HotkeyChoice::CtrlAlt => 0,
            HotkeyChoice::CtrlShift => 1,
            HotkeyChoice::AltShift => 2,
            HotkeyChoice::Disabled => 3,
        }
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(15);
const HOLD_DELAY: Duration = Duration::from_millis(120);

/// Starts the polling thread. `on_event` is called from that thread.
/// Returns an error message if the OS gives us no way to read the keyboard
/// (for example no X11 display on Linux).
pub fn spawn_hotkey_listener(
    choice: SharedHotkeyChoice,
    on_event: impl Fn(HotkeyEvent) + Send + 'static,
) -> Result<(), String> {
    let (ready_sender, ready_receiver) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("clicky-hotkey".into())
        .spawn(move || {
            let Some(device_state) = DeviceState::checked_new() else {
                let _ = ready_sender.send(Err(
                    "Global shortcuts are unavailable here (no X11 display, or the OS \
                     denied keyboard access). Use the on-screen Hold to talk button."
                        .into(),
                ));
                return;
            };
            let _ = ready_sender.send(Ok(()));

            let mut detector = HotkeyDetector::new(HOLD_DELAY);
            loop {
                let modifiers = ModifierSnapshot::from_keys(&device_state.get_keys());
                if let Some(event) = detector.update(Instant::now(), choice.get(), modifiers) {
                    on_event(event);
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        })
        .map_err(|error| error.to_string())?;

    ready_receiver
        .recv()
        .map_err(|_| "the keyboard thread exited unexpectedly".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(control: bool, alt: bool, shift: bool, other_key: bool) -> ModifierSnapshot {
        ModifierSnapshot {
            control,
            alt,
            shift,
            other_key,
        }
    }

    #[test]
    fn fires_after_the_hold_delay_and_releases_when_modifiers_go() {
        let mut detector = HotkeyDetector::new(Duration::from_millis(100));
        let start = Instant::now();
        let held = modifiers(true, true, false, false);

        assert_eq!(detector.update(start, HotkeyChoice::CtrlAlt, held), None);
        assert_eq!(
            detector.update(
                start + Duration::from_millis(50),
                HotkeyChoice::CtrlAlt,
                held
            ),
            None
        );
        assert_eq!(
            detector.update(
                start + Duration::from_millis(100),
                HotkeyChoice::CtrlAlt,
                held
            ),
            Some(HotkeyEvent::Pressed)
        );
        // Still held: no repeat events.
        assert_eq!(
            detector.update(
                start + Duration::from_millis(500),
                HotkeyChoice::CtrlAlt,
                held
            ),
            None
        );
        assert_eq!(
            detector.update(
                start + Duration::from_millis(600),
                HotkeyChoice::CtrlAlt,
                modifiers(true, false, false, false)
            ),
            Some(HotkeyEvent::Released)
        );
    }

    #[test]
    fn quick_taps_and_combos_with_other_keys_do_not_trigger() {
        let mut detector = HotkeyDetector::new(Duration::from_millis(100));
        let start = Instant::now();

        // Ctrl+Alt+T pressed together: another key is involved from the start.
        let with_t = modifiers(true, true, false, true);
        for step in 0..20 {
            assert_eq!(
                detector.update(
                    start + Duration::from_millis(step * 20),
                    HotkeyChoice::CtrlAlt,
                    with_t
                ),
                None
            );
        }

        // A tap shorter than the delay, then released.
        let mut detector = HotkeyDetector::new(Duration::from_millis(100));
        let held = modifiers(true, true, false, false);
        assert_eq!(detector.update(start, HotkeyChoice::CtrlAlt, held), None);
        assert_eq!(
            detector.update(
                start + Duration::from_millis(40),
                HotkeyChoice::CtrlAlt,
                modifiers(false, false, false, false)
            ),
            None
        );
        // Holding again restarts the timer.
        assert_eq!(
            detector.update(
                start + Duration::from_millis(60),
                HotkeyChoice::CtrlAlt,
                held
            ),
            None
        );
        assert_eq!(
            detector.update(
                start + Duration::from_millis(120),
                HotkeyChoice::CtrlAlt,
                held
            ),
            None
        );
        assert_eq!(
            detector.update(
                start + Duration::from_millis(160),
                HotkeyChoice::CtrlAlt,
                held
            ),
            Some(HotkeyEvent::Pressed)
        );
    }

    #[test]
    fn extra_keys_pressed_mid_recording_do_not_cut_it_off() {
        let mut detector = HotkeyDetector::new(Duration::ZERO);
        let start = Instant::now();
        assert_eq!(
            detector.update(
                start,
                HotkeyChoice::CtrlAlt,
                modifiers(true, true, false, false)
            ),
            Some(HotkeyEvent::Pressed)
        );
        assert_eq!(
            detector.update(
                start,
                HotkeyChoice::CtrlAlt,
                modifiers(true, true, false, true)
            ),
            None
        );
    }

    #[test]
    fn each_shortcut_choice_checks_its_own_modifiers_and_disabled_never_fires() {
        let start = Instant::now();
        for (choice, down) in [
            (HotkeyChoice::CtrlShift, modifiers(true, false, true, false)),
            (HotkeyChoice::AltShift, modifiers(false, true, true, false)),
        ] {
            let mut detector = HotkeyDetector::new(Duration::ZERO);
            assert_eq!(
                detector.update(start, choice, down),
                Some(HotkeyEvent::Pressed)
            );
        }
        let mut detector = HotkeyDetector::new(Duration::ZERO);
        assert_eq!(
            detector.update(
                start,
                HotkeyChoice::CtrlAlt,
                modifiers(true, false, true, false)
            ),
            None
        );
        assert_eq!(
            detector.update(
                start,
                HotkeyChoice::Disabled,
                modifiers(true, true, true, false)
            ),
            None
        );
    }

    #[test]
    fn switching_the_shortcut_mid_recording_releases_it() {
        let mut detector = HotkeyDetector::new(Duration::ZERO);
        let start = Instant::now();
        let held = modifiers(true, true, false, false);
        assert_eq!(
            detector.update(start, HotkeyChoice::CtrlAlt, held),
            Some(HotkeyEvent::Pressed)
        );
        assert_eq!(
            detector.update(start, HotkeyChoice::Disabled, held),
            Some(HotkeyEvent::Released)
        );
    }

    #[test]
    fn maps_keycodes_to_modifiers_including_mac_option_and_ignores_command() {
        let snapshot =
            ModifierSnapshot::from_keys(&[Keycode::RControl, Keycode::LOption, Keycode::Command]);
        assert_eq!(snapshot, modifiers(true, true, false, false));
        let with_letter = ModifierSnapshot::from_keys(&[Keycode::LControl, Keycode::A]);
        assert!(with_letter.other_key);
    }

    #[test]
    fn shared_choice_round_trips() {
        let shared = SharedHotkeyChoice::new(HotkeyChoice::AltShift);
        assert_eq!(shared.get(), HotkeyChoice::AltShift);
        for choice in HotkeyChoice::ALL {
            shared.set(choice);
            assert_eq!(shared.get(), choice);
        }
    }
}
