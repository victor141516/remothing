//! Linux evdev keycodes (wire format) → macOS virtual keycodes (`kVK_*` of
//! Carbon's Events.h). Virtual keycodes name the physical key; the Mac's
//! keyboard layout decides the character.

use objc2_core_graphics::CGEventFlags;

/// How a modifier key changes the event flags.
pub enum Modifier {
    /// Set while held.
    Held(CGEventFlags),
    /// Caps Lock: toggled by each press.
    Lock,
}

pub fn modifier(evdev: u16) -> Option<Modifier> {
    Some(match evdev {
        29 | 97 => Modifier::Held(CGEventFlags::MaskControl),
        42 | 54 => Modifier::Held(CGEventFlags::MaskShift),
        56 | 100 => Modifier::Held(CGEventFlags::MaskAlternate),
        125 | 126 => Modifier::Held(CGEventFlags::MaskCommand),
        58 => Modifier::Lock,
        _ => return None,
    })
}

/// Return, for typed newlines.
pub const KVK_RETURN: u16 = 0x24;

pub fn lookup(evdev: u16) -> Option<u16> {
    Some(match evdev {
        1 => 0x35,  // Escape
        2 => 0x12,  // 1
        3 => 0x13,  // 2
        4 => 0x14,  // 3
        5 => 0x15,  // 4
        6 => 0x17,  // 5
        7 => 0x16,  // 6
        8 => 0x1A,  // 7
        9 => 0x1C,  // 8
        10 => 0x19, // 9
        11 => 0x1D, // 0
        12 => 0x1B, // Minus
        13 => 0x18, // Equal
        14 => 0x33, // Backspace (kVK_Delete)
        15 => 0x30, // Tab
        16 => 0x0C, // Q
        17 => 0x0D, // W
        18 => 0x0E, // E
        19 => 0x0F, // R
        20 => 0x11, // T
        21 => 0x10, // Y
        22 => 0x20, // U
        23 => 0x22, // I
        24 => 0x1F, // O
        25 => 0x23, // P
        26 => 0x21, // BracketLeft
        27 => 0x1E, // BracketRight
        28 => KVK_RETURN,
        29 => 0x3B, // ControlLeft
        30 => 0x00, // A
        31 => 0x01, // S
        32 => 0x02, // D
        33 => 0x03, // F
        34 => 0x05, // G
        35 => 0x04, // H
        36 => 0x26, // J
        37 => 0x28, // K
        38 => 0x25, // L
        39 => 0x29, // Semicolon
        40 => 0x27, // Quote
        41 => 0x32, // Backquote
        42 => 0x38, // ShiftLeft
        43 => 0x2A, // Backslash
        44 => 0x06, // Z
        45 => 0x07, // X
        46 => 0x08, // C
        47 => 0x09, // V
        48 => 0x0B, // B
        49 => 0x2D, // N
        50 => 0x2E, // M
        51 => 0x2B, // Comma
        52 => 0x2F, // Period
        53 => 0x2C, // Slash
        54 => 0x3C, // ShiftRight
        55 => 0x43, // NumpadMultiply
        56 => 0x3A, // AltLeft (Option)
        57 => 0x31, // Space
        58 => 0x39, // CapsLock
        59 => 0x7A, // F1
        60 => 0x78, // F2
        61 => 0x63, // F3
        62 => 0x76, // F4
        63 => 0x60, // F5
        64 => 0x61, // F6
        65 => 0x62, // F7
        66 => 0x64, // F8
        67 => 0x65, // F9
        68 => 0x6D, // F10
        69 => 0x47, // NumLock (the Mac's keypad Clear sits there)
        71 => 0x59, // Numpad7
        72 => 0x5B, // Numpad8
        73 => 0x5C, // Numpad9
        74 => 0x4E, // NumpadSubtract
        75 => 0x56, // Numpad4
        76 => 0x57, // Numpad5
        77 => 0x58, // Numpad6
        78 => 0x45, // NumpadAdd
        79 => 0x53, // Numpad1
        80 => 0x54, // Numpad2
        81 => 0x55, // Numpad3
        82 => 0x52, // Numpad0
        83 => 0x41, // NumpadDecimal
        86 => 0x0A, // IntlBackslash (kVK_ISO_Section)
        87 => 0x67, // F11
        88 => 0x6F, // F12
        89 => 0x5E, // IntlRo (kVK_JIS_Underscore)
        96 => 0x4C, // NumpadEnter
        97 => 0x3E, // ControlRight
        98 => 0x4B, // NumpadDivide
        100 => 0x3D, // AltRight (right Option)
        102 => 0x73, // Home
        103 => 0x7E, // ArrowUp
        104 => 0x74, // PageUp
        105 => 0x7B, // ArrowLeft
        106 => 0x7C, // ArrowRight
        107 => 0x77, // End
        108 => 0x7D, // ArrowDown
        109 => 0x79, // PageDown
        110 => 0x72, // Insert (the Mac's Help key sits there)
        111 => 0x75, // Delete (kVK_ForwardDelete)
        113 => 0x4A, // AudioVolumeMute
        114 => 0x49, // AudioVolumeDown
        115 => 0x48, // AudioVolumeUp
        117 => 0x51, // NumpadEqual
        121 => 0x5F, // NumpadComma (kVK_JIS_KeypadComma)
        124 => 0x5D, // IntlYen
        125 => 0x37, // MetaLeft (Command)
        126 => 0x36, // MetaRight (right Command)
        183 => 0x69, // F13
        184 => 0x6B, // F14
        185 => 0x71, // F15
        186 => 0x6A, // F16
        187 => 0x40, // F17
        188 => 0x4F, // F18
        189 => 0x50, // F19
        190 => 0x5A, // F20
        _ => return None,
    })
}
