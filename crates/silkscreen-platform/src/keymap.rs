//! Linux evdev key codes (the protocol's encoding) → native key codes.

/// PC set-1 scancode for `SendInput`. Bit 15 (`0xE000`) marks E0-extended
/// keys. Codes 1–88 are numerically identical in evdev and set 1.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn windows_scancode(evdev: u16) -> Option<u16> {
    Some(match evdev {
        // Pause is an E1 sequence; callers send it as a virtual key instead.
        119 => return None,
        1..=88 => evdev,
        89 => 0x73,    // IntlRo
        96 => 0xE01C,  // KP Enter
        97 => 0xE01D,  // Right Ctrl
        98 => 0xE035,  // KP /
        99 => 0xE037,  // Print Screen
        100 => 0xE038, // Right Alt
        102 => 0xE047, // Home
        103 => 0xE048, // Up
        104 => 0xE049, // Page Up
        105 => 0xE04B, // Left
        106 => 0xE04D, // Right
        107 => 0xE04F, // End
        108 => 0xE050, // Down
        109 => 0xE051, // Page Down
        110 => 0xE052, // Insert
        111 => 0xE053, // Delete
        113 => 0xE020, // Mute
        114 => 0xE02E, // Volume Down
        115 => 0xE030, // Volume Up
        117 => 0x59,   // KP =
        121 => 0x7E,   // KP ,
        124 => 0x7D,   // IntlYen
        125 => 0xE05B, // Left Win
        126 => 0xE05C, // Right Win
        127 => 0xE05D, // Menu
        163 => 0xE019, // Next track
        164 => 0xE022, // Play/Pause
        165 => 0xE010, // Previous track
        166 => 0xE024, // Stop
        183..=194 => [
            0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76,
        ][(evdev - 183) as usize], // F13–F24
        _ => return None,
    })
}

/// macOS virtual key code (`kVK_*`) for `CGEventCreateKeyboardEvent`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn mac_keycode(evdev: u16) -> Option<u16> {
    Some(match evdev {
        1 => 0x35,   // Escape
        2 => 0x12,   // 1
        3 => 0x13,   // 2
        4 => 0x14,   // 3
        5 => 0x15,   // 4
        6 => 0x17,   // 5
        7 => 0x16,   // 6
        8 => 0x1A,   // 7
        9 => 0x1C,   // 8
        10 => 0x19,  // 9
        11 => 0x1D,  // 0
        12 => 0x1B,  // -
        13 => 0x18,  // =
        14 => 0x33,  // Backspace (Delete)
        15 => 0x30,  // Tab
        16 => 0x0C,  // Q
        17 => 0x0D,  // W
        18 => 0x0E,  // E
        19 => 0x0F,  // R
        20 => 0x11,  // T
        21 => 0x10,  // Y
        22 => 0x20,  // U
        23 => 0x22,  // I
        24 => 0x1F,  // O
        25 => 0x23,  // P
        26 => 0x21,  // [
        27 => 0x1E,  // ]
        28 => 0x24,  // Return
        29 => 0x3B,  // Left Control
        30 => 0x00,  // A
        31 => 0x01,  // S
        32 => 0x02,  // D
        33 => 0x03,  // F
        34 => 0x05,  // G
        35 => 0x04,  // H
        36 => 0x26,  // J
        37 => 0x28,  // K
        38 => 0x25,  // L
        39 => 0x29,  // ;
        40 => 0x27,  // '
        41 => 0x32,  // `
        42 => 0x38,  // Left Shift
        43 => 0x2A,  // \
        44 => 0x06,  // Z
        45 => 0x07,  // X
        46 => 0x08,  // C
        47 => 0x09,  // V
        48 => 0x0B,  // B
        49 => 0x2D,  // N
        50 => 0x2E,  // M
        51 => 0x2B,  // ,
        52 => 0x2F,  // .
        53 => 0x2C,  // /
        54 => 0x3C,  // Right Shift
        55 => 0x43,  // KP *
        56 => 0x3A,  // Left Option
        57 => 0x31,  // Space
        58 => 0x39,  // Caps Lock
        59 => 0x7A,  // F1
        60 => 0x78,  // F2
        61 => 0x63,  // F3
        62 => 0x76,  // F4
        63 => 0x60,  // F5
        64 => 0x61,  // F6
        65 => 0x62,  // F7
        66 => 0x64,  // F8
        67 => 0x65,  // F9
        68 => 0x6D,  // F10
        69 => 0x47,  // Num Lock → Keypad Clear
        71 => 0x59,  // KP 7
        72 => 0x5B,  // KP 8
        73 => 0x5C,  // KP 9
        74 => 0x4E,  // KP -
        75 => 0x56,  // KP 4
        76 => 0x57,  // KP 5
        77 => 0x58,  // KP 6
        78 => 0x45,  // KP +
        79 => 0x53,  // KP 1
        80 => 0x54,  // KP 2
        81 => 0x55,  // KP 3
        82 => 0x52,  // KP 0
        83 => 0x41,  // KP .
        86 => 0x0A,  // ISO section
        87 => 0x67,  // F11
        88 => 0x6F,  // F12
        96 => 0x4C,  // KP Enter
        97 => 0x3E,  // Right Control
        98 => 0x4B,  // KP /
        100 => 0x3D, // Right Option
        102 => 0x73, // Home
        103 => 0x7E, // Up
        104 => 0x74, // Page Up
        105 => 0x7B, // Left
        106 => 0x7C, // Right
        107 => 0x77, // End
        108 => 0x7D, // Down
        109 => 0x79, // Page Down
        110 => 0x72, // Insert → Help
        111 => 0x75, // Forward Delete
        113 => 0x4A, // Mute
        114 => 0x49, // Volume Down
        115 => 0x48, // Volume Up
        117 => 0x51, // KP =
        125 => 0x37, // Left Command
        126 => 0x36, // Right Command
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_extended_keys() {
        assert_eq!(windows_scancode(30), Some(0x1E)); // A
        assert_eq!(windows_scancode(103), Some(0xE048)); // Up
        assert_eq!(windows_scancode(125), Some(0xE05B)); // Win
        assert_eq!(mac_keycode(30), Some(0x00)); // A
        assert_eq!(mac_keycode(125), Some(0x37)); // Command
        // The main block and keypad map on both (Mac keyboards have no Scroll Lock, 70).
        for code in (1..=83).filter(|c| *c != 70) {
            assert!(windows_scancode(code).is_some(), "win {code}");
            assert!(mac_keycode(code).is_some(), "mac {code}");
        }
    }
}
