# Hotswitch

Minimal software KVM for sharing a Mac's keyboard and mouse with a Windows PC over LAN. Press a hotkey to send input to the PC, press again to reclaim it. Built for low-latency use (e.g. gaming). Raw input over unencrypted UDP, no video or screen sharing.

## How it works

```
Mac (sender)                  UDP / LAN              Windows (receiver)
CGEventTap ───────────────> 3–5 byte datagrams ────> SendInput
                    <──── audio (PCM over UDP) <──── WASAPI loopback
```

The sender captures keyboard and mouse events on macOS via `CGEventTap`, serializes them into UDP packets, and sends them to the receiver. The receiver injects them as native Windows input via `SendInput` with hardware scancodes.

Audio flows in the reverse direction: the receiver captures Windows system audio via WASAPI loopback and streams raw PCM over UDP back to the sender, which plays it through the Mac's default output device.

When capturing, the Mac cursor is hidden and locked. Mouse deltas and key events are forwarded to Windows. A key-sync packet every 100ms guards against dropped UDP key-up packets.

## Usage

### Sender (macOS)

```bash
cargo run -p hotswitch-sender -- <WINDOWS_IP>:24801
```

Or install the release binary somewhere in your `$PATH`:

```bash
cp target/release/hotswitch-sender /usr/local/bin/
hotswitch-sender 10.0.0.100:24801
```

### Receiver (Windows)

Download the Windows release zip. It now contains:

- `hotswitch-receiver.exe`
- `hotswitch-receiver-service.exe`
- `install-hotswitch.ps1`
- `start-hotswitch.ps1`
- `uninstall-hotswitch.ps1`

Install or migrate from an elevated PowerShell prompt:

```powershell
.\install-hotswitch.ps1
```

The installer copies the files into `C:\Program Files\Hotswitch`, removes the old scheduled-task startup entry, creates or updates the `Hotswitch` Windows service, and starts it.
It also creates the required Private-network inbound Windows Firewall rule for the installed receiver on UDP `24801`.

The service launches `hotswitch-receiver.exe` into the active console session, where it owns the tray icon, audio loopback, and input injection.

For development, you can still run the receiver directly:

```
hotswitch-receiver.exe 0.0.0.0:24801
```

### Controls

- **Ctrl+Escape**: toggle capture (send input to Windows / reclaim on Mac)
- Both sides run as system tray apps with status icons and menus

### Tray menu (both sides)

- Status line (Waiting / Connected / Capturing)
- Check for Updates (auto-checks on startup)
- Show Log
- Start on Login
- Quit

On Windows, `Start on Login` now controls the `Hotswitch` service startup type. `Quit` stops the current service-backed receiver session. To bring it back in the same session, run `start-hotswitch.ps1` from `C:\Program Files\Hotswitch` or `Start-Service Hotswitch` from an elevated shell.

## Building from source

Requires [Rust](https://rustup.rs/).

```bash
# Sender (on macOS)
cargo build --release -p hotswitch-sender

# Receiver (on Windows)
cargo build --release -p hotswitch-receiver
cargo build --release -p hotswitch-receiver-service

# Protocol tests (any platform)
cargo test -p hotswitch-proto
```

## Releasing

Every push to `main` triggers a GitHub Actions build that publishes:

- `hotswitch-sender-aarch64-apple-darwin.tar.gz`
- `hotswitch-receiver-x86_64-pc-windows-msvc.zip`

The Windows zip contains the receiver, the receiver service, and the install/uninstall scripts. The version is derived from the commit timestamp.

## Protocol

Plain UDP, 1-byte type tag, big-endian, no framing:

| Event | Port | Bytes | Layout |
|---|---|---|---|
| Mouse move | 24801 | 5 | `01 dx:i16 dy:i16` |
| Mouse button | 24801 | 3 | `02 button:u8 pressed:u8` |
| Scroll | 24801 | 5 | `03 dx:i16 dy:i16` |
| Key event | 24801 | 4 | `04 cgkeycode:u16 pressed:u8` |
| Key sync | 24801 | 2+2n | `05 count:u8 [cgkeycode:u16]...` |
| Heartbeat | 24801 | 1 | `06` |
| Audio | 24802 | 3+4n | `07 channels:u16 [sample:f32le]...` |

## Project structure

```
hotswitch/
  proto/          Shared protocol library (Event types, keymap, icon generation)
  sender/         macOS sender (CGEventTap + UDP + tray icon)
  receiver/       Windows session receiver (UDP + SendInput + tray icon)
  receiver-service/ Windows service that launches the receiver into the active session
  scripts/        Windows install/uninstall helpers

## Migrating an Existing Windows Install

If you already have the old receiver installed and enabled via the old `Start on Login` scheduled-task path:

1. Download the new Windows release zip and extract it.
2. Open an elevated PowerShell prompt in that folder.
3. Run `.\install-hotswitch.ps1`.

That script removes the legacy scheduled task automatically, updates the binaries in `C:\Program Files\Hotswitch`, creates or updates the Windows service, and starts it. You do not need to disable the old startup entry first.
```

## Acknowledgements

Huge thanks to [Moonlight](https://github.com/moonlight-stream/moonlight-qt)/[Sunshine](https://github.com/LizardByte/Sunshine) and [LAN Mouse](https://github.com/feschber/lan-mouse). This project wouldn't exist without their implementations, which were invaluable references for the input and audio handling. Audio streaming was also informed by [Beer](https://github.com/alii/beer) and [W11-to-Mac-Sound-Stream](https://github.com/egehandogan35/w11-to-mac-sound-stream).

## License

[MIT](LICENSE)
