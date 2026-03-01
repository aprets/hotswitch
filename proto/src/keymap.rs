/// Maps macOS CGKeyCode values to Windows scancodes with extended flag.
///
/// This mapping is composed from two sources in the lan-mouse project
/// (`input-event/src/scancode.rs`):
///   1. macOS CGKeyCode -> Linux evdev code (via Apple's HIToolbox/Events.h
///      kVK_* constants and the `keycode` crate's Chromium-derived mapping)
///   2. Linux evdev code -> Windows scancode (via the `TryFrom<Linux> for Windows`
///      implementation in lan-mouse)
///
/// The Windows scancode values follow the PS/2 Set 1 convention. Extended keys
/// (those requiring an E0 prefix byte) have the `extended` flag set to `true`.
///
/// CGKeyCode reference: Apple HIToolbox/Events.h (kVK_* constants)
/// Windows scancode reference: Microsoft scan code documentation
/// <https://download.microsoft.com/download/1/6/1/161ba512-40e2-4cc9-843a-923143f3456c/translate.pdf>

/// Maps a macOS CGKeyCode to a (Windows scancode, extended flag) pair.
/// Returns None for unmapped keys.
pub fn cg_to_win_scancode(cg_keycode: u16) -> Option<(u16, bool)> {
    // The tuple is (scan_code, extended).
    // scan_code is the low byte of the Windows scancode.
    // extended is true when the scancode requires the E0 prefix.
    match cg_keycode {
        // ===== Letters =====
        // kVK_ANSI_A (0x00) -> evdev 30 (KEY_A) -> Win 0x001E
        0x00 => Some((0x1E, false)),
        // kVK_ANSI_S (0x01) -> evdev 31 (KEY_S) -> Win 0x001F
        0x01 => Some((0x1F, false)),
        // kVK_ANSI_D (0x02) -> evdev 32 (KEY_D) -> Win 0x0020
        0x02 => Some((0x20, false)),
        // kVK_ANSI_F (0x03) -> evdev 33 (KEY_F) -> Win 0x0021
        0x03 => Some((0x21, false)),
        // kVK_ANSI_H (0x04) -> evdev 35 (KEY_H) -> Win 0x0023
        0x04 => Some((0x23, false)),
        // kVK_ANSI_G (0x05) -> evdev 34 (KEY_G) -> Win 0x0022
        0x05 => Some((0x22, false)),
        // kVK_ANSI_Z (0x06) -> evdev 44 (KEY_Z) -> Win 0x002C
        0x06 => Some((0x2C, false)),
        // kVK_ANSI_X (0x07) -> evdev 45 (KEY_X) -> Win 0x002D
        0x07 => Some((0x2D, false)),
        // kVK_ANSI_C (0x08) -> evdev 46 (KEY_C) -> Win 0x002E
        0x08 => Some((0x2E, false)),
        // kVK_ANSI_V (0x09) -> evdev 47 (KEY_V) -> Win 0x002F
        0x09 => Some((0x2F, false)),
        // kVK_ISO_Section (0x0A) -> evdev 86 (KEY_102ND) -> Win 0x0056
        0x0A => Some((0x56, false)),
        // kVK_ANSI_B (0x0B) -> evdev 48 (KEY_B) -> Win 0x0030
        0x0B => Some((0x30, false)),
        // kVK_ANSI_Q (0x0C) -> evdev 16 (KEY_Q) -> Win 0x0010
        0x0C => Some((0x10, false)),
        // kVK_ANSI_W (0x0D) -> evdev 17 (KEY_W) -> Win 0x0011
        0x0D => Some((0x11, false)),
        // kVK_ANSI_E (0x0E) -> evdev 18 (KEY_E) -> Win 0x0012
        0x0E => Some((0x12, false)),
        // kVK_ANSI_R (0x0F) -> evdev 19 (KEY_R) -> Win 0x0013
        0x0F => Some((0x13, false)),
        // kVK_ANSI_Y (0x10) -> evdev 21 (KEY_Y) -> Win 0x0015
        0x10 => Some((0x15, false)),
        // kVK_ANSI_T (0x11) -> evdev 20 (KEY_T) -> Win 0x0014
        0x11 => Some((0x14, false)),
        // kVK_ANSI_1 (0x12) -> evdev 2 (KEY_1) -> Win 0x0002
        0x12 => Some((0x02, false)),
        // kVK_ANSI_2 (0x13) -> evdev 3 (KEY_2) -> Win 0x0003
        0x13 => Some((0x03, false)),
        // kVK_ANSI_3 (0x14) -> evdev 4 (KEY_3) -> Win 0x0004
        0x14 => Some((0x04, false)),
        // kVK_ANSI_4 (0x15) -> evdev 5 (KEY_4) -> Win 0x0005
        0x15 => Some((0x05, false)),
        // kVK_ANSI_6 (0x16) -> evdev 7 (KEY_6) -> Win 0x0007
        0x16 => Some((0x07, false)),
        // kVK_ANSI_5 (0x17) -> evdev 6 (KEY_5) -> Win 0x0006
        0x17 => Some((0x06, false)),
        // kVK_ANSI_Equal (0x18) -> evdev 13 (KEY_EQUAL) -> Win 0x000D
        0x18 => Some((0x0D, false)),
        // kVK_ANSI_9 (0x19) -> evdev 10 (KEY_9) -> Win 0x000A
        0x19 => Some((0x0A, false)),
        // kVK_ANSI_7 (0x1A) -> evdev 8 (KEY_7) -> Win 0x0008
        0x1A => Some((0x08, false)),
        // kVK_ANSI_Minus (0x1B) -> evdev 12 (KEY_MINUS) -> Win 0x000C
        0x1B => Some((0x0C, false)),
        // kVK_ANSI_8 (0x1C) -> evdev 9 (KEY_8) -> Win 0x0009
        0x1C => Some((0x09, false)),
        // kVK_ANSI_0 (0x1D) -> evdev 11 (KEY_0) -> Win 0x000B
        0x1D => Some((0x0B, false)),
        // kVK_ANSI_RightBracket (0x1E) -> evdev 27 (KEY_RIGHTBRACE) -> Win 0x001B
        0x1E => Some((0x1B, false)),
        // kVK_ANSI_O (0x1F) -> evdev 24 (KEY_O) -> Win 0x0018
        0x1F => Some((0x18, false)),
        // kVK_ANSI_U (0x20) -> evdev 22 (KEY_U) -> Win 0x0016
        0x20 => Some((0x16, false)),
        // kVK_ANSI_LeftBracket (0x21) -> evdev 26 (KEY_LEFTBRACE) -> Win 0x001A
        0x21 => Some((0x1A, false)),
        // kVK_ANSI_I (0x22) -> evdev 23 (KEY_I) -> Win 0x0017
        0x22 => Some((0x17, false)),
        // kVK_ANSI_P (0x23) -> evdev 25 (KEY_P) -> Win 0x0019
        0x23 => Some((0x19, false)),
        // kVK_ANSI_L (0x25) -> evdev 38 (KEY_L) -> Win 0x0026
        0x25 => Some((0x26, false)),
        // kVK_ANSI_J (0x26) -> evdev 36 (KEY_J) -> Win 0x0024
        0x26 => Some((0x24, false)),
        // kVK_ANSI_Quote (0x27) -> evdev 40 (KEY_APOSTROPHE) -> Win 0x0028
        0x27 => Some((0x28, false)),
        // kVK_ANSI_K (0x28) -> evdev 37 (KEY_K) -> Win 0x0025
        0x28 => Some((0x25, false)),
        // kVK_ANSI_Semicolon (0x29) -> evdev 39 (KEY_SEMICOLON) -> Win 0x0027
        0x29 => Some((0x27, false)),
        // kVK_ANSI_Backslash (0x2A) -> evdev 43 (KEY_BACKSLASH) -> Win 0x002B
        0x2A => Some((0x2B, false)),
        // kVK_ANSI_Comma (0x2B) -> evdev 51 (KEY_COMMA) -> Win 0x0033
        0x2B => Some((0x33, false)),
        // kVK_ANSI_Slash (0x2C) -> evdev 53 (KEY_SLASH) -> Win 0x0035
        0x2C => Some((0x35, false)),
        // kVK_ANSI_N (0x2D) -> evdev 49 (KEY_N) -> Win 0x0031
        0x2D => Some((0x31, false)),
        // kVK_ANSI_M (0x2E) -> evdev 50 (KEY_M) -> Win 0x0032
        0x2E => Some((0x32, false)),
        // kVK_ANSI_Period (0x2F) -> evdev 52 (KEY_DOT) -> Win 0x0034
        0x2F => Some((0x34, false)),
        // kVK_ANSI_Grave (0x32) -> evdev 41 (KEY_GRAVE) -> Win 0x0029
        0x32 => Some((0x29, false)),

        // ===== Numpad =====
        // kVK_ANSI_KeypadDecimal (0x41) -> evdev 83 (KEY_KPDOT) -> Win 0x0053
        0x41 => Some((0x53, false)),
        // kVK_ANSI_KeypadMultiply (0x43) -> evdev 55 (KEY_KPASTERISK) -> Win 0x0037
        0x43 => Some((0x37, false)),
        // kVK_ANSI_KeypadPlus (0x45) -> evdev 78 (KEY_KPPLUS) -> Win 0x004E
        0x45 => Some((0x4E, false)),
        // kVK_ANSI_KeypadClear (0x47) -> evdev 69 (KEY_NUMLOCK) -> Win 0x0045
        0x47 => Some((0x45, false)),
        // kVK_ANSI_KeypadDivide (0x4B) -> evdev 98 (KEY_KPSLASH) -> Win 0xE035
        0x4B => Some((0x35, true)),
        // kVK_ANSI_KeypadEnter (0x4C) -> evdev 96 (KEY_KPENTER) -> Win 0xE01C
        0x4C => Some((0x1C, true)),
        // kVK_ANSI_KeypadMinus (0x4E) -> evdev 74 (KEY_KPMINUS) -> Win 0x004A
        0x4E => Some((0x4A, false)),
        // kVK_ANSI_KeypadEquals (0x51) -> evdev 117 (KEY_KPEQUAL) -> Win 0x0059
        0x51 => Some((0x59, false)),
        // kVK_ANSI_Keypad0 (0x52) -> evdev 82 (KEY_KP0) -> Win 0x0052
        0x52 => Some((0x52, false)),
        // kVK_ANSI_Keypad1 (0x53) -> evdev 79 (KEY_KP1) -> Win 0x004F
        0x53 => Some((0x4F, false)),
        // kVK_ANSI_Keypad2 (0x54) -> evdev 80 (KEY_KP2) -> Win 0x0050
        0x54 => Some((0x50, false)),
        // kVK_ANSI_Keypad3 (0x55) -> evdev 81 (KEY_KP3) -> Win 0x0051
        0x55 => Some((0x51, false)),
        // kVK_ANSI_Keypad4 (0x56) -> evdev 75 (KEY_KP4) -> Win 0x004B
        0x56 => Some((0x4B, false)),
        // kVK_ANSI_Keypad5 (0x57) -> evdev 76 (KEY_KP5) -> Win 0x004C
        0x57 => Some((0x4C, false)),
        // kVK_ANSI_Keypad6 (0x58) -> evdev 77 (KEY_KP6) -> Win 0x004D
        0x58 => Some((0x4D, false)),
        // kVK_ANSI_Keypad7 (0x59) -> evdev 71 (KEY_KP7) -> Win 0x0047
        0x59 => Some((0x47, false)),
        // kVK_ANSI_Keypad8 (0x5B) -> evdev 72 (KEY_KP8) -> Win 0x0048
        0x5B => Some((0x48, false)),
        // kVK_ANSI_Keypad9 (0x5C) -> evdev 73 (KEY_KP9) -> Win 0x0049
        0x5C => Some((0x49, false)),

        // ===== Special keys =====
        // kVK_Return (0x24) -> evdev 28 (KEY_ENTER) -> Win 0x001C
        0x24 => Some((0x1C, false)),
        // kVK_Tab (0x30) -> evdev 15 (KEY_TAB) -> Win 0x000F
        0x30 => Some((0x0F, false)),
        // kVK_Space (0x31) -> evdev 57 (KEY_SPACE) -> Win 0x0039
        0x31 => Some((0x39, false)),
        // kVK_Delete (0x33) -> evdev 14 (KEY_BACKSPACE) -> Win 0x000E
        0x33 => Some((0x0E, false)),
        // kVK_Escape (0x35) -> evdev 1 (KEY_ESC) -> Win 0x0001
        0x35 => Some((0x01, false)),

        // ===== Modifier keys =====
        // kVK_Command (0x37) -> evdev 125 (KEY_LEFTMETA) -> Win 0xE05B
        0x37 => Some((0x5B, true)),
        // kVK_Shift (0x38) -> evdev 42 (KEY_LEFTSHIFT) -> Win 0x002A
        0x38 => Some((0x2A, false)),
        // kVK_CapsLock (0x39) -> evdev 58 (KEY_CAPSLOCK) -> Win 0x003A
        0x39 => Some((0x3A, false)),
        // kVK_Option (0x3A) -> evdev 56 (KEY_LEFTALT) -> Win 0x0038
        0x3A => Some((0x38, false)),
        // kVK_Control (0x3B) -> evdev 29 (KEY_LEFTCTRL) -> Win 0x001D
        0x3B => Some((0x1D, false)),
        // kVK_RightShift (0x3C) -> evdev 54 (KEY_RIGHTSHIFT) -> Win 0x0036
        0x3C => Some((0x36, false)),
        // kVK_RightOption (0x3D) -> evdev 100 (KEY_RIGHTALT) -> Win 0xE038
        0x3D => Some((0x38, true)),
        // kVK_RightControl (0x3E) -> evdev 97 (KEY_RIGHTCTRL) -> Win 0xE01D
        0x3E => Some((0x1D, true)),
        // kVK_RightCommand (0x36) -> evdev 126 (KEY_RIGHTMETA) -> Win 0xE05C
        0x36 => Some((0x5C, true)),
        // kVK_Function (0x3F) — macOS-only Fn key, no standard Windows scancode
        // (mapped to None)

        // ===== Function keys =====
        // kVK_F1 (0x7A) -> evdev 59 (KEY_F1) -> Win 0x003B
        0x7A => Some((0x3B, false)),
        // kVK_F2 (0x78) -> evdev 60 (KEY_F2) -> Win 0x003C
        0x78 => Some((0x3C, false)),
        // kVK_F3 (0x63) -> evdev 61 (KEY_F3) -> Win 0x003D
        0x63 => Some((0x3D, false)),
        // kVK_F4 (0x76) -> evdev 62 (KEY_F4) -> Win 0x003E
        0x76 => Some((0x3E, false)),
        // kVK_F5 (0x60) -> evdev 63 (KEY_F5) -> Win 0x003F
        0x60 => Some((0x3F, false)),
        // kVK_F6 (0x61) -> evdev 64 (KEY_F6) -> Win 0x0040
        0x61 => Some((0x40, false)),
        // kVK_F7 (0x62) -> evdev 65 (KEY_F7) -> Win 0x0041
        0x62 => Some((0x41, false)),
        // kVK_F8 (0x64) -> evdev 66 (KEY_F8) -> Win 0x0042
        0x64 => Some((0x42, false)),
        // kVK_F9 (0x65) -> evdev 67 (KEY_F9) -> Win 0x0043
        0x65 => Some((0x43, false)),
        // kVK_F10 (0x6D) -> evdev 68 (KEY_F10) -> Win 0x0044
        0x6D => Some((0x44, false)),
        // kVK_F11 (0x67) -> evdev 87 (KEY_F11) -> Win 0x0057
        0x67 => Some((0x57, false)),
        // kVK_F12 (0x6F) -> evdev 88 (KEY_F12) -> Win 0x0058
        0x6F => Some((0x58, false)),
        // kVK_F13 (0x69) -> evdev 183 (KEY_F13) -> Win 0x0064
        0x69 => Some((0x64, false)),
        // kVK_F14 (0x6B) -> evdev 184 (KEY_F14) -> Win 0x0065
        0x6B => Some((0x65, false)),
        // kVK_F15 (0x71) -> evdev 185 (KEY_F15) -> Win 0x0066
        0x71 => Some((0x66, false)),
        // kVK_F16 (0x6A) -> evdev 186 (KEY_F16) -> Win 0x0067
        0x6A => Some((0x67, false)),
        // kVK_F17 (0x40) -> evdev 187 (KEY_F17) -> Win 0x0068
        0x40 => Some((0x68, false)),
        // kVK_F18 (0x4F) -> evdev 188 (KEY_F18) -> Win 0x0069
        0x4F => Some((0x69, false)),
        // kVK_F19 (0x50) -> evdev 189 (KEY_F19) -> Win 0x006A
        0x50 => Some((0x6A, false)),
        // kVK_F20 (0x5A) -> evdev 190 (KEY_F20) -> Win 0x006B
        0x5A => Some((0x6B, false)),

        // ===== Navigation / editing keys =====
        // kVK_Home (0x73) -> evdev 102 (KEY_HOME) -> Win 0xE047
        0x73 => Some((0x47, true)),
        // kVK_End (0x77) -> evdev 107 (KEY_END) -> Win 0xE04F
        0x77 => Some((0x4F, true)),
        // kVK_PageUp (0x74) -> evdev 104 (KEY_PAGEUP) -> Win 0xE049
        0x74 => Some((0x49, true)),
        // kVK_PageDown (0x79) -> evdev 109 (KEY_PAGEDOWN) -> Win 0xE051
        0x79 => Some((0x51, true)),
        // kVK_ForwardDelete (0x75) -> evdev 111 (KEY_DELETE) -> Win 0xE053
        0x75 => Some((0x53, true)),

        // ===== Arrow keys =====
        // kVK_LeftArrow (0x7B) -> evdev 105 (KEY_LEFT) -> Win 0xE04B
        0x7B => Some((0x4B, true)),
        // kVK_RightArrow (0x7C) -> evdev 106 (KEY_RIGHT) -> Win 0xE04D
        0x7C => Some((0x4D, true)),
        // kVK_DownArrow (0x7D) -> evdev 108 (KEY_DOWN) -> Win 0xE050
        0x7D => Some((0x50, true)),
        // kVK_UpArrow (0x7E) -> evdev 103 (KEY_UP) -> Win 0xE048
        0x7E => Some((0x48, true)),

        // ===== Japanese / International keys =====
        // kVK_JIS_Yen (0x5D) -> evdev 124 (KEY_YEN) -> Win 0x007D (International3)
        0x5D => Some((0x7D, false)),
        // kVK_JIS_Underscore (0x5E) -> evdev 89 (KEY_RO) -> Win 0x0073 (International1)
        0x5E => Some((0x73, false)),
        // kVK_JIS_KeypadComma (0x5F) -> evdev 95 (KEY_KPJPCOMMA) -> Win 0x007E
        0x5F => Some((0x7E, false)),
        // kVK_JIS_Eisu (0x66) -> evdev 123 (KEY_HANJA) -> Win 0x0071 (LANG2)
        0x66 => Some((0x71, false)),
        // kVK_JIS_Kana (0x68) -> evdev 122 (KEY_HANGUEL) -> Win 0x0072 (LANG1)
        0x68 => Some((0x72, false)),

        // ===== Misc =====
        // kVK_Help (0x72) -> evdev 138 (KEY_HELP) -> Win 0xE052 (Insert)
        // Note: The Mac Help key is conventionally mapped to Insert on Windows.
        // lan-mouse maps KEY_HELP -> KeyF1, but physically Help occupies Insert position.
        // We map to Insert (0xE052) which matches the physical key position on Apple keyboards.
        0x72 => Some((0x52, true)),

        // kVK_VolumeUp (0x48) -> evdev 115 (KEY_VOLUMEUP) -> Win 0xE030
        0x48 => Some((0x30, true)),
        // kVK_VolumeDown (0x49) -> evdev 114 (KEY_VOLUMEDOWN) -> Win 0xE02E
        0x49 => Some((0x2E, true)),
        // kVK_Mute (0x4A) -> evdev 113 (KEY_MUTE) -> Win 0xE020
        0x4A => Some((0x20, true)),

        // Anything unmapped
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_letter_keys() {
        // kVK_ANSI_A -> scancode 0x1E, not extended
        assert_eq!(cg_to_win_scancode(0x00), Some((0x1E, false)));
        // kVK_ANSI_Z -> scancode 0x2C, not extended
        assert_eq!(cg_to_win_scancode(0x06), Some((0x2C, false)));
    }

    #[test]
    fn test_number_keys() {
        // kVK_ANSI_1 -> scancode 0x02
        assert_eq!(cg_to_win_scancode(0x12), Some((0x02, false)));
        // kVK_ANSI_0 -> scancode 0x0B
        assert_eq!(cg_to_win_scancode(0x1D), Some((0x0B, false)));
    }

    #[test]
    fn test_arrow_keys_are_extended() {
        assert_eq!(cg_to_win_scancode(0x7B), Some((0x4B, true))); // Left
        assert_eq!(cg_to_win_scancode(0x7C), Some((0x4D, true))); // Right
        assert_eq!(cg_to_win_scancode(0x7D), Some((0x50, true))); // Down
        assert_eq!(cg_to_win_scancode(0x7E), Some((0x48, true))); // Up
    }

    #[test]
    fn test_navigation_keys_are_extended() {
        assert_eq!(cg_to_win_scancode(0x73), Some((0x47, true))); // Home
        assert_eq!(cg_to_win_scancode(0x77), Some((0x4F, true))); // End
        assert_eq!(cg_to_win_scancode(0x74), Some((0x49, true))); // PageUp
        assert_eq!(cg_to_win_scancode(0x79), Some((0x51, true))); // PageDown
        assert_eq!(cg_to_win_scancode(0x75), Some((0x53, true))); // ForwardDelete
    }

    #[test]
    fn test_modifier_keys() {
        // Left Command -> extended
        assert_eq!(cg_to_win_scancode(0x37), Some((0x5B, true)));
        // Left Shift -> not extended
        assert_eq!(cg_to_win_scancode(0x38), Some((0x2A, false)));
        // Right Control -> extended
        assert_eq!(cg_to_win_scancode(0x3E), Some((0x1D, true)));
        // Right Option (Alt) -> extended
        assert_eq!(cg_to_win_scancode(0x3D), Some((0x38, true)));
    }

    #[test]
    fn test_numpad_enter_is_extended() {
        assert_eq!(cg_to_win_scancode(0x4C), Some((0x1C, true)));
    }

    #[test]
    fn test_numpad_divide_is_extended() {
        assert_eq!(cg_to_win_scancode(0x4B), Some((0x35, true)));
    }

    #[test]
    fn test_numpad_regular_not_extended() {
        // Numpad 0 -> not extended
        assert_eq!(cg_to_win_scancode(0x52), Some((0x52, false)));
        // Numpad 5 -> not extended
        assert_eq!(cg_to_win_scancode(0x57), Some((0x4C, false)));
    }

    #[test]
    fn test_function_keys() {
        assert_eq!(cg_to_win_scancode(0x7A), Some((0x3B, false))); // F1
        assert_eq!(cg_to_win_scancode(0x6F), Some((0x58, false))); // F12
        assert_eq!(cg_to_win_scancode(0x69), Some((0x64, false))); // F13
    }

    #[test]
    fn test_unmapped_returns_none() {
        // kVK_Function (Fn key) is not mapped
        assert_eq!(cg_to_win_scancode(0x3F), None);
        // Random high value
        assert_eq!(cg_to_win_scancode(0xFF), None);
    }

    #[test]
    fn test_special_keys() {
        assert_eq!(cg_to_win_scancode(0x24), Some((0x1C, false))); // Return
        assert_eq!(cg_to_win_scancode(0x30), Some((0x0F, false))); // Tab
        assert_eq!(cg_to_win_scancode(0x31), Some((0x39, false))); // Space
        assert_eq!(cg_to_win_scancode(0x33), Some((0x0E, false))); // Delete (Backspace)
        assert_eq!(cg_to_win_scancode(0x35), Some((0x01, false))); // Escape
    }

    #[test]
    fn test_volume_keys_extended() {
        assert_eq!(cg_to_win_scancode(0x48), Some((0x30, true))); // Volume Up
        assert_eq!(cg_to_win_scancode(0x49), Some((0x2E, true))); // Volume Down
        assert_eq!(cg_to_win_scancode(0x4A), Some((0x20, true))); // Mute
    }
}
