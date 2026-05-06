# Hotswitch

Minimal software KVM for sharing a Mac's keyboard and mouse with a Windows PC over LAN. Press a hotkey to send input to the PC, press again to reclaim it. Built for low-latency use. Raw input over unencrypted UDP, no video or screen sharing.

## How it works

```text
Mac (sender)                  UDP / LAN              Windows (receiver)
CGEventTap ───────────────> 3–5 byte datagrams ────> SendInput
                    <──── audio (PCM over UDP) <──── WASAPI loopback
```

The sender captures keyboard and mouse events on macOS via `CGEventTap`, serializes them into UDP packets, and sends them to the receiver. The receiver injects them as native Windows input via `SendInput` with hardware scancodes.

Audio flows in the reverse direction: the receiver captures Windows system audio via WASAPI loopback and streams raw PCM over UDP back to the sender, which plays it through the Mac's default output device.

## Usage

### Sender (macOS)

The preferred macOS install is `Hotswitch.app`.

1. Drag `Hotswitch.app` into `/Applications`.
2. Launch it once.
3. If no receiver address is saved yet, the app prompts for the Windows receiver address in `IP:port` form.
4. Use the tray item `Receiver Address...` any time you want to change it later.

`Start on Login` launches the app bundle directly and uses the saved receiver address. No command-line arguments are required once the address has been saved.

The macOS release archive also includes the bare `hotswitch-sender` binary for development/debugging, but the app bundle is the normal install path.

`Check for Updates` on macOS now stages and swaps the whole `Hotswitch.app` bundle, rather than patching the inner executable in place.

### Receiver (Windows)

Download the Windows release zip. It contains:

- `hotswitch-receiver.exe`
- `hotswitch-receiver-service.exe`
- `install-hotswitch.ps1`
- `start-hotswitch.ps1`
- `uninstall-hotswitch.ps1`

Install or migrate from an elevated PowerShell prompt:

```powershell
.\install-hotswitch.ps1
```

The installer copies the files into `C:\Program Files\Hotswitch`, removes the old scheduled-task startup entry, creates or updates the `Hotswitch` Windows service, adds the required Private-network inbound firewall rule for UDP `24801`, and starts the service.

The service launches `hotswitch-receiver.exe` into the active console session, where it owns the tray icon, audio loopback, and input injection.

For development, you can still run the receiver directly:

```text
hotswitch-receiver.exe 0.0.0.0:24801
```

## Controls

- `Ctrl+Escape`: toggle capture
- Both sides run as tray apps with status icons and menus

## Tray menu

### macOS sender

- Status line
- Check for Updates
- Receiver Address...
- Show Log
- Start on Login
- Quit

### Windows receiver

- Status line
- Check for Updates
- Show Log
- Start on Login
- Quit

On Windows, `Start on Login` controls the `Hotswitch` service startup type. `Quit` stops the current service-backed receiver session. To bring it back in the same session, run `start-hotswitch.ps1` from `C:\Program Files\Hotswitch` or `Start-Service Hotswitch` from an elevated shell.

## Building from source

Requires [Rust](https://rustup.rs/).

```bash
# Sender (on macOS)
cargo build --release -p hotswitch-sender
./scripts/build-sender-app.sh target/release/hotswitch-sender "$(sed -n 's/^version = \"\\(.*\\)\"/\\1/p' sender/Cargo.toml | head -1)"
cp -R target/macos-app/Hotswitch.app /Applications/

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

The macOS tarball contains `Hotswitch.app` and the bare `hotswitch-sender` binary. The app bundle keeps the sender as a menu bar app (`LSUIElement`) while adding app-bundle metadata for Game Mode eligibility and normal macOS install behavior. The Windows zip contains the receiver, the receiver service, and the install/uninstall scripts. The version is derived from the commit timestamp.

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

```text
hotswitch/
  proto/             Shared protocol library
  sender/            macOS sender (CGEventTap + UDP + tray icon)
  receiver/          Windows session receiver (UDP + SendInput + tray icon)
  receiver-service/  Windows service that launches the receiver into the active session
  scripts/           Build and install helpers
```

## Acknowledgements

Huge thanks to [Moonlight](https://github.com/moonlight-stream/moonlight-qt)/[Sunshine](https://github.com/LizardByte/Sunshine) and [LAN Mouse](https://github.com/feschber/lan-mouse). This project wouldn't exist without their implementations, which were invaluable references for the input and audio handling. Audio streaming was also informed by [Beer](https://github.com/alii/beer) and [W11-to-Mac-Sound-Stream](https://github.com/egehandogan35/w11-to-mac-sound-stream).

## License

[MIT](LICENSE)
