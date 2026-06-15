//! Tests for physical keyboard lock-LED synchronization.

use std::cell::RefCell;
use std::rc::Rc;

use beewm::compositor::{
    KeyboardLedController, KeyboardLedState, KeyboardLeds, KeyboardStatus, LedDevice,
    LedDeviceRegistry,
};
use smithay::input::keyboard::{LedMapping, LedState, xkb};
use smithay::reexports::input::Led;

const CAPS_ON: KeyboardLedState = KeyboardLedState {
    caps_lock: true,
    num_lock: false,
    scroll_lock: false,
};

const NUM_ON: KeyboardLedState = KeyboardLedState {
    caps_lock: false,
    num_lock: true,
    scroll_lock: false,
};

/// Mock physical keyboard: records every LED state written to it.
#[derive(Clone)]
struct MockDevice {
    id: u32,
    writes: Rc<RefCell<Vec<(u32, KeyboardLedState)>>>,
}

impl MockDevice {
    fn new(id: u32, writes: &Rc<RefCell<Vec<(u32, KeyboardLedState)>>>) -> Self {
        Self {
            id,
            writes: writes.clone(),
        }
    }
}

impl PartialEq for MockDevice {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl LedDevice for MockDevice {
    fn set_leds(&mut self, state: KeyboardLedState) {
        self.writes.borrow_mut().push((self.id, state));
    }
}

/// Mock backend controller: records every state pushed through it.
struct MockController {
    pushes: Rc<RefCell<Vec<KeyboardLedState>>>,
}

impl KeyboardLedController for MockController {
    fn set_keyboard_leds(&mut self, state: KeyboardLedState) {
        self.pushes.borrow_mut().push(state);
    }
}

#[test]
fn led_state_conversion_flattens_missing_indicators() {
    // A keymap without an indicator reports None; the lock can never engage
    // there, so the LED must read as off.
    let smithay_state = LedState {
        num: None,
        caps: Some(true),
        scroll: Some(false),
    };
    assert_eq!(KeyboardLedState::from(smithay_state), CAPS_ON);

    assert_eq!(
        KeyboardLedState::from(LedState::default()),
        KeyboardLedState::default()
    );
}

#[test]
fn none_to_some_false_keymap_transition_is_not_a_change() {
    // Keymap reloads can change indicator availability without changing what
    // the hardware should show; the flattened states must compare equal so
    // the dedup logic skips the write.
    let before = KeyboardLedState::from(LedState {
        num: None,
        caps: Some(false),
        scroll: None,
    });
    let after = KeyboardLedState::from(LedState {
        num: Some(false),
        caps: Some(false),
        scroll: Some(false),
    });
    assert_eq!(before, after);
}

#[test]
fn libinput_flag_conversion_sets_only_active_locks() {
    assert_eq!(Led::from(KeyboardLedState::default()), Led::empty());
    assert_eq!(Led::from(CAPS_ON), Led::CAPSLOCK);
    assert_eq!(Led::from(NUM_ON), Led::NUMLOCK);
    assert_eq!(
        Led::from(KeyboardLedState {
            caps_lock: true,
            num_lock: true,
            scroll_lock: true,
        }),
        Led::CAPSLOCK | Led::NUMLOCK | Led::SCROLLLOCK,
    );
}

#[test]
fn apply_skips_unchanged_state() {
    let pushes = Rc::new(RefCell::new(Vec::new()));
    let mut leds = KeyboardLeds::new();
    leds.install_controller(Box::new(MockController {
        pushes: pushes.clone(),
    }));

    leds.apply(CAPS_ON);
    leds.apply(CAPS_ON);
    assert_eq!(*pushes.borrow(), vec![CAPS_ON]);

    leds.apply(NUM_ON);
    assert_eq!(*pushes.borrow(), vec![CAPS_ON, NUM_ON]);
}

#[test]
fn invalidate_forces_next_apply() {
    let pushes = Rc::new(RefCell::new(Vec::new()));
    let mut leds = KeyboardLeds::new();
    leds.install_controller(Box::new(MockController {
        pushes: pushes.clone(),
    }));

    leds.apply(CAPS_ON);
    // After a VT switch the hardware may have been rewritten behind our back:
    // the same state must be pushed again once the cache is invalidated.
    leds.invalidate();
    leds.apply(CAPS_ON);
    assert_eq!(*pushes.borrow(), vec![CAPS_ON, CAPS_ON]);
}

#[test]
fn apply_without_controller_is_safe_and_does_not_poison_cache() {
    // The winit backend installs no controller; LED operations must be
    // silent no-ops there.
    let mut leds = KeyboardLeds::new();
    leds.apply(CAPS_ON);
    leds.invalidate();
    leds.apply(NUM_ON);

    // A state seen while no controller was installed was never written to
    // hardware, so it must not be treated as already applied later.
    let pushes = Rc::new(RefCell::new(Vec::new()));
    leds.install_controller(Box::new(MockController {
        pushes: pushes.clone(),
    }));
    leds.apply(NUM_ON);
    assert_eq!(*pushes.borrow(), vec![NUM_ON]);
}

#[test]
fn registry_fans_out_to_all_devices() {
    let writes = Rc::new(RefCell::new(Vec::new()));
    let registry = LedDeviceRegistry::new();
    registry.add_device(MockDevice::new(1, &writes), KeyboardLedState::default());
    registry.add_device(MockDevice::new(2, &writes), KeyboardLedState::default());
    writes.borrow_mut().clear();

    let mut controller: Box<dyn KeyboardLedController> = Box::new(registry);
    controller.set_keyboard_leds(CAPS_ON);
    assert_eq!(*writes.borrow(), vec![(1, CAPS_ON), (2, CAPS_ON)]);
}

#[test]
fn adding_device_applies_current_state() {
    // A keyboard plugged in while Caps Lock is active must light up
    // immediately, not on the next lock-key press.
    let writes = Rc::new(RefCell::new(Vec::new()));
    let registry: LedDeviceRegistry<MockDevice> = LedDeviceRegistry::new();
    registry.add_device(MockDevice::new(7, &writes), CAPS_ON);
    assert_eq!(*writes.borrow(), vec![(7, CAPS_ON)]);
}

#[test]
fn removed_device_receives_no_further_writes() {
    let writes = Rc::new(RefCell::new(Vec::new()));
    let registry = LedDeviceRegistry::new();
    let first = MockDevice::new(1, &writes);
    registry.add_device(first.clone(), KeyboardLedState::default());
    registry.add_device(MockDevice::new(2, &writes), KeyboardLedState::default());
    registry.remove_device(&first);
    writes.borrow_mut().clear();

    let mut controller: Box<dyn KeyboardLedController> = Box::new(registry);
    controller.set_keyboard_leds(NUM_ON);
    assert_eq!(*writes.borrow(), vec![(2, NUM_ON)]);
}

#[test]
fn capslock_and_numlock_toggle_leds_through_real_us_keymap() {
    // End-to-end through real xkbcommon: compile the default US keymap, press
    // the lock keys, and check the indicators land in KeyboardLedState the
    // way smithay's `led_state_changed` would deliver them.
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "us",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("compile default us keymap");
    let mut state = xkb::State::new(&keymap);
    let mapping = LedMapping::from_keymap(&keymap);
    let mut leds = LedState::from_state(&state, &mapping);
    assert_eq!(KeyboardLedState::from(leds), KeyboardLedState::default());

    // XKB keycodes are evdev keycodes + 8.
    let caps = xkb::Keycode::new(58 + 8); // KEY_CAPSLOCK
    let num = xkb::Keycode::new(69 + 8); // KEY_NUMLOCK

    let tap = |state: &mut xkb::State, key: xkb::Keycode| {
        state.update_key(key, xkb::KeyDirection::Down);
        state.update_key(key, xkb::KeyDirection::Up);
    };

    tap(&mut state, caps);
    assert!(leds.update_with(&state, &mapping), "caps tap changes LEDs");
    assert_eq!(KeyboardLedState::from(leds), CAPS_ON);

    tap(&mut state, num);
    leds.update_with(&state, &mapping);
    assert_eq!(
        KeyboardLedState::from(leds),
        KeyboardLedState {
            caps_lock: true,
            num_lock: true,
            scroll_lock: false,
        }
    );

    // A second Caps Lock tap unlocks again; Num Lock stays.
    tap(&mut state, caps);
    leds.update_with(&state, &mapping);
    assert_eq!(KeyboardLedState::from(leds), NUM_ON);
}

#[test]
fn keyboard_status_event_payload_format() {
    let status = KeyboardStatus {
        caps_lock: true,
        num_lock: false,
        scroll_lock: false,
        shift: true,
    };
    assert_eq!(
        status.event_payload(),
        "keyboard>>caps=1 num=0 scroll=0 shift=1"
    );
    assert_eq!(
        KeyboardStatus::default().event_payload(),
        "keyboard>>caps=0 num=0 scroll=0 shift=0"
    );
}
