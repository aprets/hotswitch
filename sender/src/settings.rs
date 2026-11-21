//! Modes, hotkeys and the settings file (`~/Library/Application Support/Hotswitch/settings.json`).

use core_graphics::event::CGEventFlags;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use tray_icon::menu::accelerator::{Accelerator, Code, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Keyboard, mouse and screen stay with the Mac.
    Mac,
    /// Keyboard and mouse go to the Windows receiver; its audio comes back over UDP.
    Windows,
    /// The monitor shows the PS5 and its audio plays through the Mac.
    Ps5,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Mac, Mode::Windows, Mode::Ps5];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Mac => "Mac",
            Mode::Windows => "Windows PC",
            Mode::Ps5 => "PS5",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    /// Name passed to the `on-mode-changed` hook.
    pub fn key(self) -> &'static str {
        match self {
            Mode::Mac => "mac",
            Mode::Windows => "windows",
            Mode::Ps5 => "ps5",
        }
    }
}

/// Where a mode's sound comes from. Windows could later move to `Passthrough` too, when its
/// audio reaches the Mac through the monitor's audio out like the PS5's does.
pub enum AudioRoute {
    /// The receiver streams WASAPI loopback over UDP (the listener runs all the time).
    Network,
    /// A local input, played to the default output with low latency.
    Passthrough { input: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    pub key: u16,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ctrl: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub option: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub shift: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub command: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

pub const KEY_ESCAPE: u16 = 0x35;
pub const KEY_DELETE: u16 = 0x33;

impl Hotkey {
    pub fn from_event(key: u16, flags: CGEventFlags) -> Self {
        Self {
            key,
            ctrl: flags.contains(CGEventFlags::CGEventFlagControl),
            option: flags.contains(CGEventFlags::CGEventFlagAlternate),
            shift: flags.contains(CGEventFlags::CGEventFlagShift),
            command: flags.contains(CGEventFlags::CGEventFlagCommand),
        }
    }

    /// A shortcut must use Control, Option or Command, or be a function key,
    /// so typing never triggers it.
    pub fn is_usable(&self) -> bool {
        self.ctrl
            || self.option
            || self.command
            || key_info(self.key).is_some_and(|k| k.1.starts_with('F') && k.1.len() > 1)
    }

    pub fn label(&self) -> String {
        let mut s = String::new();
        for (on, sym) in [
            (self.ctrl, "⌃"),
            (self.option, "⌥"),
            (self.shift, "⇧"),
            (self.command, "⌘"),
        ] {
            if on {
                s.push_str(sym);
            }
        }
        match key_info(self.key) {
            Some((_, label, _)) => s.push_str(label),
            None => s.push_str(&format!("key {}", self.key)),
        }
        s
    }

    /// Shown next to the mode in the menu. Keys without an equivalent get none.
    pub fn accelerator(&self) -> Option<Accelerator> {
        let (_, _, code) = key_info(self.key)?;
        let mut mods = Modifiers::empty();
        mods.set(Modifiers::CONTROL, self.ctrl);
        mods.set(Modifiers::ALT, self.option);
        mods.set(Modifiers::SHIFT, self.shift);
        mods.set(Modifiers::SUPER, self.command);
        Some(Accelerator::new(Some(mods), code))
    }
}

fn key_info(key: u16) -> Option<(u16, &'static str, Code)> {
    KEYS.iter().copied().find(|k| k.0 == key)
}

/// macOS virtual key codes (HIToolbox kVK_*) with a label and the menu's key code.
const KEYS: &[(u16, &str, Code)] = &[
    (0x00, "A", Code::KeyA),
    (0x01, "S", Code::KeyS),
    (0x02, "D", Code::KeyD),
    (0x03, "F", Code::KeyF),
    (0x04, "H", Code::KeyH),
    (0x05, "G", Code::KeyG),
    (0x06, "Z", Code::KeyZ),
    (0x07, "X", Code::KeyX),
    (0x08, "C", Code::KeyC),
    (0x09, "V", Code::KeyV),
    (0x0B, "B", Code::KeyB),
    (0x0C, "Q", Code::KeyQ),
    (0x0D, "W", Code::KeyW),
    (0x0E, "E", Code::KeyE),
    (0x0F, "R", Code::KeyR),
    (0x10, "Y", Code::KeyY),
    (0x11, "T", Code::KeyT),
    (0x12, "1", Code::Digit1),
    (0x13, "2", Code::Digit2),
    (0x14, "3", Code::Digit3),
    (0x15, "4", Code::Digit4),
    (0x16, "6", Code::Digit6),
    (0x17, "5", Code::Digit5),
    (0x19, "9", Code::Digit9),
    (0x1A, "7", Code::Digit7),
    (0x1C, "8", Code::Digit8),
    (0x1D, "0", Code::Digit0),
    (0x1F, "O", Code::KeyO),
    (0x20, "U", Code::KeyU),
    (0x22, "I", Code::KeyI),
    (0x23, "P", Code::KeyP),
    (0x24, "↩", Code::Enter),
    (0x25, "L", Code::KeyL),
    (0x26, "J", Code::KeyJ),
    (0x28, "K", Code::KeyK),
    (0x2D, "N", Code::KeyN),
    (0x2E, "M", Code::KeyM),
    (0x30, "⇥", Code::Tab),
    (0x31, "Space", Code::Space),
    (0x32, "`", Code::Backquote),
    (KEY_ESCAPE, "⎋", Code::Escape),
    (0x7A, "F1", Code::F1),
    (0x78, "F2", Code::F2),
    (0x63, "F3", Code::F3),
    (0x76, "F4", Code::F4),
    (0x60, "F5", Code::F5),
    (0x61, "F6", Code::F6),
    (0x62, "F7", Code::F7),
    (0x64, "F8", Code::F8),
    (0x65, "F9", Code::F9),
    (0x6D, "F10", Code::F10),
    (0x67, "F11", Code::F11),
    (0x6F, "F12", Code::F12),
    (0x7B, "←", Code::ArrowLeft),
    (0x7C, "→", Code::ArrowRight),
    (0x7D, "↓", Code::ArrowDown),
    (0x7E, "↑", Code::ArrowUp),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Windows receiver, `IP:port`.
    pub receiver: Option<String>,
    /// The PS5 monitor's serial. Unset works only while exactly one monitor can be used.
    pub monitor: Option<String>,
    /// VCP 0x60 value of the PS5's input (0x11 is HDMI 1).
    pub ps5_input: u16,
    /// CoreAudio input that carries the PS5's sound.
    pub audio_input: String,
    /// Shortcut per mode, indexed like `Mode::ALL`.
    pub hotkeys: [Option<Hotkey>; 3],
}

impl Default for Settings {
    fn default() -> Self {
        let ctrl = |key, option| Hotkey {
            key,
            ctrl: true,
            option,
            shift: false,
            command: false,
        };
        Self {
            receiver: None,
            monitor: None,
            ps5_input: 0x11,
            audio_input: "Microphone In".into(),
            hotkeys: [
                None,
                Some(ctrl(KEY_ESCAPE, false)),
                Some(ctrl(KEY_ESCAPE, true)),
            ],
        }
    }
}

impl Settings {
    pub fn audio_route(&self, mode: Mode) -> Option<AudioRoute> {
        match mode {
            Mode::Mac => None,
            Mode::Windows => Some(AudioRoute::Network),
            Mode::Ps5 => Some(AudioRoute::Passthrough {
                input: self.audio_input.clone(),
            }),
        }
    }

    /// The monitor input a mode needs. Windows switching stays in the receiver hooks for now.
    pub fn monitor_input(&self, mode: Mode) -> Option<u16> {
        (mode == Mode::Ps5).then_some(self.ps5_input)
    }

    pub fn load(dir: &Path) -> Self {
        let path = dir.join("settings.json");
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                eprintln!("settings: ignoring unreadable {}: {e}", path.display());
                Settings::default()
            }),
            // A dry run starts from a copy of the real settings; its edits stay in its folder.
            Err(_) if dry_run() && dir != real_support_dir() => Settings::load(&real_support_dir()),
            Err(_) => {
                // First run of this version: keep the receiver address from the old file.
                let receiver = std::fs::read_to_string(dir.join("receiver-address.txt"))
                    .ok()
                    .and_then(|raw| parse_receiver_address(&raw).ok());
                Settings {
                    receiver,
                    ..Settings::default()
                }
            }
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_atomically(&dir.join("settings.json"), &json)
    }
}

/// `HOTSWITCH_DRY_RUN=1`: monitors are simulated, hooks and login-item changes are skipped,
/// and all state lives in its own folder, so a dry run can't touch hardware or real state.
pub fn dry_run() -> bool {
    static DRY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DRY.get_or_init(|| std::env::var("HOTSWITCH_DRY_RUN").is_ok_and(|v| v == "1"))
}

/// `~/Library/Application Support/Hotswitch`, whatever the run.
pub fn real_support_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/Application Support")
        .join("Hotswitch")
}

/// Where this run keeps its state: settings, the hand-back claim, the DDC lock.
pub fn support_dir() -> PathBuf {
    if dry_run() {
        real_support_dir().join("dry-run")
    } else {
        real_support_dir()
    }
}

/// Replace `path` in one step: write a temporary file next to it, flush it to disk, rename.
/// A crash leaves either the old file or the new one, never a truncated one.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()
}

pub fn parse_receiver_address(value: &str) -> Result<String, &'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Enter the Windows receiver address in IP:port form.");
    }
    trimmed
        .parse::<std::net::SocketAddr>()
        .map(|addr| addr.to_string())
        .map_err(|_| "Use IP:port, like 10.0.0.209:24801.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let s = Settings::default();
        assert_eq!(s.hotkeys[Mode::Windows.index()].unwrap().label(), "⌃⎋");
        assert_eq!(s.hotkeys[Mode::Ps5.index()].unwrap().label(), "⌃⌥⎋");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        let partial: Settings = serde_json::from_str(r#"{"receiver":"10.0.0.209:24801"}"#).unwrap();
        assert_eq!(partial.ps5_input, 0x11);
        assert_eq!(partial.audio_input, "Microphone In");
    }

    #[test]
    fn hotkeys_need_a_modifier_or_function_key() {
        let key = |key, ctrl| Hotkey {
            key,
            ctrl,
            option: false,
            shift: false,
            command: false,
        };
        assert!(!key(0x03, false).is_usable()); // F
        assert!(key(0x60, false).is_usable()); // F5
        assert!(key(0x23, true).is_usable()); // ⌃P
        assert!(key(0x23, true).accelerator().is_some());
    }

    #[test]
    fn atomic_write_replaces_whole_file() {
        let dir = std::env::temp_dir().join(format!("hotswitch-test-{}", std::process::id()));
        let path = dir.join("x.json");
        write_atomically(&path, b"first, longer").unwrap();
        write_atomically(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_settings_files_still_load() {
        let old: Settings =
            serde_json::from_str(r#"{"receiver":"10.0.0.209:24801","restore_input":15}"#).unwrap();
        assert_eq!(old.receiver.as_deref(), Some("10.0.0.209:24801"));
    }

    #[test]
    fn receiver_address() {
        assert_eq!(
            parse_receiver_address(" 10.0.0.209:24801 "),
            Ok("10.0.0.209:24801".into())
        );
        assert!(parse_receiver_address("10.0.0.209").is_err());
    }
}
