//! Direct wlroots virtual input.
//!
//! This backend deliberately speaks `virtual-keyboard-unstable-v1` and
//! `wlr-virtual-pointer-unstable-v1` over the current Wayland connection. It
//! does not rely on `hyprctl sendshortcut`, uinput, or an input daemon.

use std::{
    fs::{self, File},
    io::Write,
    os::fd::AsFd,
    path::PathBuf,
    time::Instant,
};
use thiserror::Error;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_pointer::{Axis, AxisSource, ButtonState},
        wl_registry, wl_seat,
    },
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

mod virtual_keyboard {
    pub mod v1 {
        pub mod client {
            use wayland_client;
            use wayland_client::protocol::*;
            pub mod __interfaces {
                use wayland_client::protocol::__interfaces::*;
                wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");
            }
            use self::__interfaces::*;
            wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
        }
    }
}
use virtual_keyboard::v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

#[derive(Debug, Error)]
pub enum InputError {
    #[error("Wayland virtual input is unavailable: {0}")]
    Unavailable(String),
    #[error("unsupported key `{0}`")]
    Key(String),
}

struct State;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerV1);

/// A single direct input device, kept alive for the lifetime of a program so
/// held modifiers remain held until explicitly released.
pub struct InputBackend {
    connection: Connection,
    _queue: EventQueue<State>,
    keyboard: ZwpVirtualKeyboardV1,
    pointer: ZwlrVirtualPointerV1,
    started: Instant,
}

impl InputBackend {
    pub fn connect() -> Result<Self, InputError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| InputError::Unavailable(error.to_string()))?;
        let (globals, queue) = registry_queue_init::<State>(&connection)
            .map_err(|error| InputError::Unavailable(error.to_string()))?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=9, ())
            .map_err(|error| InputError::Unavailable(format!("wl_seat: {error}")))?;
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|error| {
                InputError::Unavailable(format!("virtual keyboard protocol: {error}"))
            })?;
        let pointer_manager: ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 1..=2, ()).map_err(|error| {
                InputError::Unavailable(format!("virtual pointer protocol: {error}"))
            })?;
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
        let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());
        install_keymap(&keyboard)?;
        connection
            .flush()
            .map_err(|error| InputError::Unavailable(error.to_string()))?;
        Ok(Self {
            connection,
            _queue: queue,
            keyboard,
            pointer,
            started: Instant::now(),
        })
    }

    /// Synthesise a chord. Keys use the XKB/evdev names accepted by basm.
    pub fn chord(&self, keys: &[String]) -> Result<(), InputError> {
        if keys.is_empty() {
            return Err(InputError::Key("empty chord".into()));
        }
        for key in &keys[..keys.len() - 1] {
            self.key(key, true)?;
        }
        self.key(keys.last().unwrap(), true)?;
        self.key(keys.last().unwrap(), false)?;
        for key in keys[..keys.len() - 1].iter().rev() {
            self.key(key, false)?;
        }
        self.flush()
    }
    pub fn key(&self, key: &str, pressed: bool) -> Result<(), InputError> {
        let key = evdev_key(key).ok_or_else(|| InputError::Key(key.into()))?;
        self.keyboard.key(self.time(), key, u32::from(pressed));
        Ok(())
    }
    /// Types printable ASCII using XKB key names. Non-ASCII text is intentionally
    /// rejected so callers can choose the clipboard path rather than corrupt it.
    pub fn text(&self, text: &str) -> Result<(), InputError> {
        for character in text.chars() {
            let (shift, key) =
                printable_key(character).ok_or_else(|| InputError::Key(character.to_string()))?;
            if shift {
                self.key("shift", true)?;
            }
            self.key(key, true)?;
            self.key(key, false)?;
            if shift {
                self.key("shift", false)?;
            }
        }
        self.flush()
    }
    pub fn click_primary(&self) -> Result<(), InputError> {
        const BTN_LEFT: u32 = 0x110;
        self.pointer
            .button(self.time(), BTN_LEFT, ButtonState::Pressed);
        self.pointer
            .button(self.time(), BTN_LEFT, ButtonState::Released);
        self.pointer.frame();
        self.flush()
    }
    pub fn scroll(&self, vertical: bool, steps: i32) -> Result<(), InputError> {
        let axis = if vertical {
            Axis::VerticalScroll
        } else {
            Axis::HorizontalScroll
        };
        self.pointer.axis_source(AxisSource::Wheel);
        self.pointer
            .axis_discrete(self.time(), axis, steps as f64 * 15.0, steps);
        self.pointer.axis_stop(self.time(), axis);
        self.pointer.frame();
        self.flush()
    }
    pub fn release_all(&self) -> Result<(), InputError> {
        for modifier in ["super", "ctrl", "alt", "shift"] {
            self.key(modifier, false)?;
        }
        self.flush()
    }
    fn time(&self) -> u32 {
        self.started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32
    }
    fn flush(&self) -> Result<(), InputError> {
        self.connection
            .flush()
            .map_err(|error| InputError::Unavailable(error.to_string()))
    }
}

fn install_keymap(keyboard: &ZwpVirtualKeyboardV1) -> Result<(), InputError> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| InputError::Unavailable("could not compile the active XKB keymap".into()))?;
    let source = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    let path = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into()))
        .join(format!("koto-keymap-{}", std::process::id()));
    let mut file =
        File::create(&path).map_err(|error| InputError::Unavailable(error.to_string()))?;
    file.write_all(source.as_bytes())
        .map_err(|error| InputError::Unavailable(error.to_string()))?;
    file.flush()
        .map_err(|error| InputError::Unavailable(error.to_string()))?;
    keyboard.keymap(1, file.as_fd(), source.len() as u32);
    let _ = fs::remove_file(path);
    Ok(())
}

fn printable_key(character: char) -> Option<(bool, &'static str)> {
    let plain = match character {
        'a'..='z' => return Some((false, Box::leak(character.to_string().into_boxed_str()))),
        'A'..='Z' => {
            return Some((
                true,
                Box::leak(character.to_ascii_lowercase().to_string().into_boxed_str()),
            ));
        }
        '0'..='9' => return Some((false, Box::leak(character.to_string().into_boxed_str()))),
        ' ' => (false, "space"),
        '\n' => (false, "enter"),
        '-' => (false, "minus"),
        '=' => (false, "equal"),
        '[' => (false, "leftbrace"),
        ']' => (false, "rightbrace"),
        '\\' => (false, "backslash"),
        ';' => (false, "semicolon"),
        '\'' => (false, "apostrophe"),
        '`' => (false, "grave"),
        ',' => (false, "comma"),
        '.' => (false, "dot"),
        '/' => (false, "slash"),
        '!' => (true, "1"),
        '@' => (true, "2"),
        '#' => (true, "3"),
        '$' => (true, "4"),
        '%' => (true, "5"),
        '^' => (true, "6"),
        '&' => (true, "7"),
        '*' => (true, "8"),
        '(' => (true, "9"),
        ')' => (true, "0"),
        '_' => (true, "minus"),
        '+' => (true, "equal"),
        '{' => (true, "leftbrace"),
        '}' => (true, "rightbrace"),
        '|' => (true, "backslash"),
        ':' => (true, "semicolon"),
        '"' => (true, "apostrophe"),
        '~' => (true, "grave"),
        '<' => (true, "comma"),
        '>' => (true, "dot"),
        '?' => (true, "slash"),
        _ => return None,
    };
    Some(plain)
}
fn evdev_key(key: &str) -> Option<u32> {
    let normalized = key.to_ascii_lowercase();
    let key = normalized.as_str();
    let code = match key {
        "esc" | "escape" => 1,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "minus" | "-" => 12,
        "equal" | "=" => 13,
        "backspace" => 14,
        "tab" => 15,
        "q" => 16,
        "w" => 17,
        "e" => 18,
        "r" => 19,
        "t" => 20,
        "y" => 21,
        "u" => 22,
        "i" => 23,
        "o" => 24,
        "p" => 25,
        "leftbrace" | "[" => 26,
        "rightbrace" | "]" => 27,
        "enter" | "return" => 28,
        "ctrl" | "control" => 29,
        "a" => 30,
        "s" => 31,
        "d" => 32,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "semicolon" | ";" => 39,
        "apostrophe" | "'" => 40,
        "grave" | "`" => 41,
        "shift" => 42,
        "backslash" | "\\" => 43,
        "z" => 44,
        "x" => 45,
        "c" => 46,
        "v" => 47,
        "b" => 48,
        "n" => 49,
        "m" => 50,
        "comma" | "," => 51,
        "dot" | "." => 52,
        "slash" | "/" => 53,
        "alt" => 56,
        "space" => 57,
        "capslock" => 58,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "super" | "meta" => 125,
        "left" => 105,
        "right" => 106,
        "up" => 103,
        "down" => 108,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "insert" => 110,
        "delete" | "del" => 111,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::evdev_key;
    #[test]
    fn common_keys_use_linux_evdev_codes() {
        assert_eq!(evdev_key("super"), Some(125));
        assert_eq!(evdev_key("enter"), Some(28));
    }
}
