//! Linux evdev keycodes (wire format) → PS/2 set-1 scancodes for SendInput.
//! Scancodes keep the physical key; the host's keyboard layout decides the character.

/// How one physical key is injected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    /// Set-1 scancode; `extended` = E0-prefixed.
    Scan { code: u16, extended: bool },
    /// Keys whose make code cannot be expressed as one (E0-)scancode through
    /// SendInput (Pause is E1-prefixed; PrintScreen is a fake-shift sequence):
    /// injected by virtual key with the scancode as a hint.
    Virtual { vk: u16, scan: u16, extended: bool },
}

const VK_SNAPSHOT: u16 = 0x2C;
const VK_PAUSE: u16 = 0x13;

/// Evdev codes of keys that take part in Ctrl+Alt+Delete.
pub const EV_LEFTCTRL: u16 = 29;
pub const EV_RIGHTCTRL: u16 = 97;
pub const EV_LEFTALT: u16 = 56;
pub const EV_RIGHTALT: u16 = 100;
pub const EV_DELETE: u16 = 111;
/// Numpad '.' is Delete with NumLock off; it triggers the SAS on real keyboards too.
pub const EV_KPDOT: u16 = 83;

pub const EV_TAB: u16 = 15;
pub const EV_ENTER: u16 = 28;

/// Shift, Ctrl, Alt, Meta and the lock keys (evdev codes).
pub fn is_modifier(evdev: u16) -> bool {
    matches!(evdev, 29 | 42 | 54 | 56 | 58 | 69 | 70 | 97 | 100 | 125 | 126)
}

pub fn lookup(evdev: u16) -> Option<KeyCode> {
    let scan = |code| Some(KeyCode::Scan { code, extended: false });
    let ext = |code| Some(KeyCode::Scan { code, extended: true });
    match evdev {
        // Esc..Numpad '.': evdev codes were defined as the set-1 make codes.
        1..=83 => scan(evdev),
        86 => scan(0x56),  // IntlBackslash (102nd key)
        87 => scan(0x57),  // F11
        88 => scan(0x58),  // F12
        89 => scan(0x73),  // IntlRo
        96 => ext(0x1C),   // NumpadEnter
        97 => ext(0x1D),   // ControlRight
        98 => ext(0x35),   // NumpadDivide
        99 => Some(KeyCode::Virtual { vk: VK_SNAPSHOT, scan: 0x37, extended: true }),
        100 => ext(0x38),  // AltRight
        102 => ext(0x47),  // Home
        103 => ext(0x48),  // ArrowUp
        104 => ext(0x49),  // PageUp
        105 => ext(0x4B),  // ArrowLeft
        106 => ext(0x4D),  // ArrowRight
        107 => ext(0x4F),  // End
        108 => ext(0x50),  // ArrowDown
        109 => ext(0x51),  // PageDown
        110 => ext(0x52),  // Insert
        111 => ext(0x53),  // Delete
        117 => scan(0x59), // NumpadEqual
        119 => Some(KeyCode::Virtual { vk: VK_PAUSE, scan: 0x45, extended: false }),
        121 => scan(0x7E), // NumpadComma
        124 => scan(0x7D), // IntlYen
        125 => ext(0x5B),  // MetaLeft
        126 => ext(0x5C),  // MetaRight
        127 => ext(0x5D),  // ContextMenu
        183..=193 => scan(0x64 + (evdev - 183)), // F13..F23
        194 => scan(0x76),                       // F24
        _ => None,
    }
}
