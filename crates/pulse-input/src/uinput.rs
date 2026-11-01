//! Linux uinput backend.
//!
//! Creates two persistent virtual devices once — `Pulse Virtual Keyboard`
//! and `Pulse Virtual Mouse` — with only the capabilities Pulse emits. They
//! are reused for every run and destroyed when the backend is dropped (closing
//! the uinput fd unregisters the device; the kernel then releases any keys it
//! still reports as down). Injection needs write access to `/dev/uinput` only;
//! no physical `/dev/input/event*` node is ever read.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, RelativeAxisCode};
use pulse_core::input::KEY_TABLE;
use pulse_core::{BackendKind, HeldInputs, KeyState, LogicalInput, MouseButton};

use crate::backend::{BackendHealth, InputBackend, InputError};

pub const UINPUT_PATH: &str = "/dev/uinput";
pub const KEYBOARD_NAME: &str = "Pulse Virtual Keyboard";
pub const MOUSE_NAME: &str = "Pulse Virtual Mouse";

/// Max events per device frame (one `SYN_REPORT`).
const FRAME_CAPACITY: usize = 32;

pub const PERMISSION_HINT: &str = "grant your user write access to /dev/uinput with a narrow udev rule \
(see packaging/udev/90-pulse-uinput.rules.example and docs/security.md); do not run Pulse as root \
and do not make /dev/uinput world-writable";

pub const MISSING_HINT: &str = "load the uinput kernel module (`sudo modprobe uinput`, or add `uinput` \
to /etc/modules-load.d/) and check that udev created /dev/uinput";

const fn button_code(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => KeyCode::BTN_LEFT.0,
        MouseButton::Right => KeyCode::BTN_RIGHT.0,
        MouseButton::Middle => KeyCode::BTN_MIDDLE.0,
    }
}

fn map_open_error(path: &Path, e: io::Error) -> InputError {
    match e.kind() {
        io::ErrorKind::PermissionDenied => InputError::DevicePermissionDenied {
            path: path.to_owned(),
            hint: PERMISSION_HINT.to_owned(),
        },
        io::ErrorKind::NotFound => InputError::DeviceMissing {
            path: path.to_owned(),
            hint: MISSING_HINT.to_owned(),
        },
        _ => InputError::DeviceInit(format!("{}: {e}", path.display())),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Keyboard,
    Mouse,
}

fn translate(ev: &LogicalInput) -> (Target, InputEvent) {
    let value = |s: KeyState| match s {
        KeyState::Pressed => 1,
        KeyState::Released => 0,
    };
    match *ev {
        LogicalInput::KeyboardKey { key, state } => (
            Target::Keyboard,
            InputEvent::new(EventType::KEY.0, key.code(), value(state)),
        ),
        LogicalInput::MouseButton { button, state } => (
            Target::Mouse,
            InputEvent::new(EventType::KEY.0, button_code(button), value(state)),
        ),
    }
}

pub struct UinputBackend {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    nodes: Vec<PathBuf>,
    held: HeldInputs,
    last_error: Option<String>,
    dead: bool,
}

impl std::fmt::Debug for UinputBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UinputBackend").field("nodes", &self.nodes).finish()
    }
}

impl UinputBackend {
    /// Create both virtual devices and wait (bounded) until their
    /// `/dev/input/event*` nodes exist. Desktop discovery by the X server
    /// happens asynchronously after this; the engine waits for it separately.
    pub fn open() -> Result<UinputBackend, InputError> {
        UinputBackend::open_at(Path::new(UINPUT_PATH))
    }

    pub fn open_at(path: &Path) -> Result<UinputBackend, InputError> {
        // Explicit probe first so the error names the path precisely.
        if !path.exists() {
            return Err(InputError::DeviceMissing {
                path: path.to_owned(),
                hint: MISSING_HINT.to_owned(),
            });
        }
        if path != Path::new(UINPUT_PATH) {
            return Err(InputError::DeviceInit(format!(
                "evdev only supports {UINPUT_PATH}; {} is not usable",
                path.display()
            )));
        }

        let mut keys = AttributeSet::<KeyCode>::new();
        for info in KEY_TABLE {
            keys.insert(KeyCode(info.code));
        }
        let keyboard = VirtualDevice::builder()
            .map_err(|e| map_open_error(path, e))?
            .name(KEYBOARD_NAME)
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x0000, 0x0001, 1))
            .with_keys(&keys)
            .and_then(|b| b.build())
            .map_err(|e| InputError::DeviceInit(format!("{KEYBOARD_NAME}: {e}")))?;

        let mut buttons = AttributeSet::<KeyCode>::new();
        for b in MouseButton::ALL {
            buttons.insert(KeyCode(button_code(b)));
        }
        // Pulse never moves the pointer, but libinput only classifies a
        // device as a mouse when it advertises relative X/Y axes.
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        axes.insert(RelativeAxisCode::REL_X);
        axes.insert(RelativeAxisCode::REL_Y);
        let mouse = VirtualDevice::builder()
            .map_err(|e| map_open_error(path, e))?
            .name(MOUSE_NAME)
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x0000, 0x0002, 1))
            .with_keys(&buttons)
            .and_then(|b| b.with_relative_axes(&axes))
            .and_then(|b| b.build())
            .map_err(|e| InputError::DeviceInit(format!("{MOUSE_NAME}: {e}")))?;

        let mut backend = UinputBackend {
            keyboard,
            mouse,
            nodes: Vec::new(),
            held: HeldInputs::new(),
            last_error: None,
            dead: false,
        };
        backend.nodes = backend.wait_for_nodes(Duration::from_secs(2))?;
        Ok(backend)
    }

    fn wait_for_nodes(&mut self, timeout: Duration) -> Result<Vec<PathBuf>, InputError> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut nodes = Vec::new();
            for dev in [&mut self.keyboard, &mut self.mouse] {
                if let Ok(iter) = dev.enumerate_dev_nodes_blocking() {
                    nodes.extend(iter.filter_map(Result::ok).filter(|p| p.exists()));
                }
            }
            if nodes.len() >= 2 {
                nodes.sort();
                return Ok(nodes);
            }
            if Instant::now() >= deadline {
                return Err(InputError::DeviceInit(
                    "virtual devices were created but udev did not create /dev/input nodes within 2 s".into(),
                ));
            }
            // One-time initialization wait, never on the hot path.
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `/dev/input/event*` nodes of the two virtual devices.
    pub fn device_nodes(&self) -> &[PathBuf] {
        &self.nodes
    }

    fn fail(&mut self, e: io::Error) -> InputError {
        let err = match e.raw_os_error() {
            Some(code) if code == libc_enodev() => InputError::Disconnected(format!("virtual device removed: {e}")),
            _ => InputError::Submit(e.to_string()),
        };
        self.dead = true;
        self.last_error = Some(err.to_string());
        err
    }

    /// Emit events, one `SYN_REPORT` frame per run of same-device events.
    fn write_frames(&mut self, events: &[LogicalInput]) -> Result<(), InputError> {
        let mut frame = [InputEvent::new(0, 0, 0); FRAME_CAPACITY];
        let mut len = 0;
        let mut current = Target::Keyboard;
        for ev in events {
            let (target, event) = translate(ev);
            if len > 0 && (target != current || len == FRAME_CAPACITY) {
                self.flush_frame(current, &frame[..len])?;
                len = 0;
            }
            current = target;
            frame[len] = event;
            len += 1;
        }
        if len > 0 {
            self.flush_frame(current, &frame[..len])?;
        }
        Ok(())
    }

    fn flush_frame(&mut self, target: Target, frame: &[InputEvent]) -> Result<(), InputError> {
        let dev = match target {
            Target::Keyboard => &mut self.keyboard,
            Target::Mouse => &mut self.mouse,
        };
        dev.emit(frame).map_err(|e| self.fail(e))
    }
}

/// `ENODEV`, without pulling in libc for one constant.
const fn libc_enodev() -> i32 {
    19
}

impl InputBackend for UinputBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::UInput
    }

    fn emit(&mut self, events: &[LogicalInput]) -> Result<(), InputError> {
        if self.dead {
            return Err(InputError::Disconnected("virtual device previously failed".into()));
        }
        self.write_frames(events)?;
        for ev in events {
            self.held.apply_lenient(ev);
        }
        Ok(())
    }

    fn release_all(&mut self, extra: &HeldInputs) -> Result<(), InputError> {
        let all = self.held.union(extra);
        let mut first_err = None;
        // Release one at a time so one failure does not skip the rest.
        for p in all.iter() {
            let ev = LogicalInput::release_of(p);
            match self.write_frames(std::slice::from_ref(&ev)) {
                Ok(()) => self.held.apply_lenient(&ev),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn held(&self) -> HeldInputs {
        self.held
    }

    fn check(&mut self) -> Result<(), InputError> {
        if self.dead {
            return Err(InputError::Disconnected("virtual device previously failed".into()));
        }
        // The device is alive while its sysfs node exists.
        match self.keyboard.get_syspath().and_then(|_| self.mouse.get_syspath()) {
            Ok(_) => Ok(()),
            Err(e) => Err(self.fail(e)),
        }
    }

    fn supports(&self, _set: &HeldInputs) -> Result<(), InputError> {
        // Every key in Pulse's table is advertised on the virtual keyboard.
        Ok(())
    }

    fn health(&self) -> BackendHealth {
        let nodes: Vec<String> = self.nodes.iter().map(|p| p.display().to_string()).collect();
        BackendHealth {
            kind: BackendKind::UInput,
            healthy: !self.dead,
            detail: format!("{KEYBOARD_NAME} + {MOUSE_NAME} ({})", nodes.join(", ")),
            last_error: self.last_error.clone(),
        }
    }
}

impl Drop for UinputBackend {
    fn drop(&mut self) {
        if !self.dead && !self.held.is_empty() {
            let _ = self.release_all(&HeldInputs::new());
        }
        // Dropping the VirtualDevices closes their fds, which destroys them.
    }
}
