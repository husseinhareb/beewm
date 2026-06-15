//! Physical keyboard lock-LED synchronization.
//!
//! Smithay tracks the XKB indicator state for the seat keyboard and fires
//! [`SeatHandler::led_state_changed`] after a processed key event (or a keymap
//! change) toggles an indicator. This module turns those notifications into
//! writes to real hardware: the udev backend registers every libinput
//! keyboard in a [`LedDeviceRegistry`] and installs a clone of it as the
//! [`KeyboardLedController`] in [`KeyboardLeds`]; the nested winit backend
//! installs nothing — the host compositor owns the keyboards there — which
//! makes every LED operation a safe no-op.
//!
//! A note on the "Shift light": keyboards expose LEDs for the three lock
//! indicators only — Caps Lock, Num Lock and Scroll Lock make up the entire
//! libinput LED API (`LIBINPUT_LED_{NUM,CAPS,SCROLL}_LOCK`). Plain Shift is a
//! momentary modifier with no hardware LED, so it is deliberately absent from
//! [`KeyboardLedState`]. It *is* part of [`KeyboardStatus`], the
//! `keyboard>>…` event-socket snapshot, so a bar can render a software
//! "Shift light" without the compositor pretending such hardware exists.
//!
//! [`SeatHandler::led_state_changed`]: smithay::input::SeatHandler::led_state_changed

use std::cell::RefCell;
use std::rc::Rc;

use smithay::input::keyboard::LedState;
use smithay::reexports::input as libinput;

/// Effective lock-key LED state derived from the seat keyboard's XKB state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardLedState {
    pub caps_lock: bool,
    pub num_lock: bool,
    pub scroll_lock: bool,
}

impl From<LedState> for KeyboardLedState {
    fn from(state: LedState) -> Self {
        // A keymap that lacks one of the standard indicators reports `None`;
        // that lock can then never engage, so its LED is simply off. This
        // also keeps `None → Some(false)` keymap transitions from counting
        // as a hardware-visible change.
        Self {
            caps_lock: state.caps.unwrap_or(false),
            num_lock: state.num.unwrap_or(false),
            scroll_lock: state.scroll.unwrap_or(false),
        }
    }
}

impl From<KeyboardLedState> for libinput::Led {
    fn from(state: KeyboardLedState) -> Self {
        let mut leds = libinput::Led::empty();
        if state.num_lock {
            leds |= libinput::Led::NUMLOCK;
        }
        if state.caps_lock {
            leds |= libinput::Led::CAPSLOCK;
        }
        if state.scroll_lock {
            leds |= libinput::Led::SCROLLLOCK;
        }
        leds
    }
}

/// Backend hook that pushes lock-LED state to physical keyboards.
pub trait KeyboardLedController {
    fn set_keyboard_leds(&mut self, state: KeyboardLedState);
}

/// One keyboard whose LEDs can be written. Implemented for libinput devices;
/// tests substitute mock devices.
pub trait LedDevice {
    fn set_leds(&mut self, state: KeyboardLedState);
}

impl LedDevice for libinput::Device {
    fn set_leds(&mut self, state: KeyboardLedState) {
        // Void FFI call: libinput swallows write errors (e.g. a device that
        // was unplugged between the remove event and now), so a vanishing
        // keyboard can never fail or crash the apply path.
        self.led_update(state.into());
    }
}

/// The current keyboard devices of a backend. The backend's hotplug handler
/// keeps one clone to add/remove devices; another clone installed in
/// [`KeyboardLeds`] fans each LED write out to every registered device.
pub struct LedDeviceRegistry<D: LedDevice> {
    devices: Rc<RefCell<Vec<D>>>,
}

impl<D: LedDevice> LedDeviceRegistry<D> {
    pub fn new() -> Self {
        Self {
            devices: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Register a keyboard and immediately push `current` to it: a device
    /// that just (re)appeared — boot enumeration, hotplug, libinput resume —
    /// carries whatever LED state firmware or the previous VT owner left.
    pub fn add_device(&self, mut device: D, current: KeyboardLedState) {
        device.set_leds(current);
        self.devices.borrow_mut().push(device);
    }

    /// Forget a detached keyboard so no further writes go to a stale handle.
    pub fn remove_device(&self, device: &D)
    where
        D: PartialEq,
    {
        self.devices.borrow_mut().retain(|known| known != device);
    }
}

// Manual impls: derives would put unnecessary `D: Clone` / `D: Default`
// bounds on the type parameter, but only the shared `Rc` is cloned/defaulted.
impl<D: LedDevice> Clone for LedDeviceRegistry<D> {
    fn clone(&self) -> Self {
        Self {
            devices: self.devices.clone(),
        }
    }
}

impl<D: LedDevice> Default for LedDeviceRegistry<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: LedDevice> KeyboardLedController for LedDeviceRegistry<D> {
    fn set_keyboard_leds(&mut self, state: KeyboardLedState) {
        for device in self.devices.borrow_mut().iter_mut() {
            device.set_leds(state);
        }
    }
}

/// Compositor-side LED bookkeeping: the backend controller (if the active
/// backend can drive LEDs at all) plus the last state actually written, so
/// unchanged states are never re-pushed to hardware.
pub struct KeyboardLeds {
    controller: Option<Box<dyn KeyboardLedController>>,
    last_applied: Option<KeyboardLedState>,
}

impl KeyboardLeds {
    pub fn new() -> Self {
        Self {
            controller: None,
            last_applied: None,
        }
    }

    /// Install the backend hook. Backends without LED access never call this
    /// and every subsequent [`apply`](Self::apply) stays a no-op.
    pub fn install_controller(&mut self, controller: Box<dyn KeyboardLedController>) {
        self.controller = Some(controller);
    }

    /// Push `state` to physical keyboards, skipping the write when it matches
    /// the last applied state.
    pub fn apply(&mut self, state: KeyboardLedState) {
        let Some(controller) = self.controller.as_mut() else {
            // Nothing was written, so deliberately don't record `state`: if a
            // controller is installed later, the first real apply must not be
            // skipped as "already current".
            return;
        };
        if self.last_applied == Some(state) {
            return;
        }
        controller.set_keyboard_leds(state);
        self.last_applied = Some(state);
    }

    /// Forget the last-applied state so the next [`apply`](Self::apply)
    /// writes unconditionally — after a VT switch the hardware LEDs may have
    /// been rewritten behind our back, making the cache a lie.
    pub fn invalidate(&mut self) {
        self.last_applied = None;
    }
}

impl Default for KeyboardLeds {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock/Shift snapshot for the `keyboard>>…` event-socket line. Unlike
/// [`KeyboardLedState`] this is UI status, not hardware control: it carries
/// Shift precisely because Shift has no physical LED to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardStatus {
    pub caps_lock: bool,
    pub num_lock: bool,
    pub scroll_lock: bool,
    pub shift: bool,
}

impl KeyboardStatus {
    /// `keyboard>>caps=0|1 num=0|1 scroll=0|1 shift=0|1`, without the
    /// trailing newline the broadcaster appends.
    pub fn event_payload(&self) -> String {
        format!(
            "keyboard>>caps={} num={} scroll={} shift={}",
            u8::from(self.caps_lock),
            u8::from(self.num_lock),
            u8::from(self.scroll_lock),
            u8::from(self.shift),
        )
    }
}
