# Hotswitch

Minimal software KVM for sharing a Mac's keyboard and mouse with a Windows PC over LAN. Press a hotkey to send input to the PC, press again to reclaim it. Built for low-latency use. Raw input over unencrypted UDP, no video or screen sharing.

On the Mac it is a menu bar mode switcher with three modes:

- **Mac**: keyboard, mouse and screen stay with the Mac. The default.
- **Windows PC**: keyboard and mouse go to the Windows receiver, and its sound comes back over UDP.
- **PS5**: a monitor switches to the PS5's input over DDC/CI, and the PS5's sound plays through the Mac. Keyboard and mouse stay with the Mac; the PS5 has its own controller.

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
2. Launch it and allow Accessibility when macOS asks (needed for the keyboard and mouse tap and the shortcuts).
3. Open `Settings…` from the menu and enter the Windows receiver address in `IP:port` form.

Settings live in `~/Library/Application Support/Hotswitch/settings.json`. The first run imports the address from the old `receiver-address.txt`. `Start Hotswitch at login` launches the app bundle directly; no command-line arguments are needed.

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

- `⌃⎋` (Control+Escape): Windows PC mode, press again for Mac. It now matches exactly, so Control+Shift+Escape no longer toggles.
- `⌃⌥⎋` (Control+Option+Escape): PS5 mode, press again for Mac.
- Every mode, Mac included, can have its own shortcut in Settings. Pressing the shortcut of the mode you are in goes back to Mac.
- Both sides run as tray apps with status icons and menus

## Tray menu

### macOS sender

- The three modes, with a check on the current one and each mode's shortcut. Windows PC is greyed out while the receiver isn't reachable.
- In PS5 mode only, three status lines: the monitor and its input ("XV275K P3 on HDMI 1"), the signal level ("Signal -21 dB" or "No signal"), and the route ("Microphone In → AirPods · 13 ms").
- The PC's state: connected, not reachable, or no address set.
- `Allow Accessibility Access…`, only while macOS hasn't granted it.
- `Settings…`, `Show Log`, `Check for Updates`, `Quit Hotswitch`.

The menu bar icon shows the mode: an outline circle (Mac, PC not reachable), a filled circle (Mac, PC connected), yellow (Windows PC) or blue (PS5).

### Settings window

One small window: the Windows receiver address with its connection state underneath, the monitor, the PS5's input and the Mac's input for PS5 mode, the audio input that carries the PS5's sound (input devices only), a shortcut recorder per mode (click, press the keys; Delete clears, Escape cancels) and `Start Hotswitch at login`.

### PS5 mode

The PS5 plugs into the monitor over HDMI. The monitor's 3.5 mm audio out goes into a line or microphone input on the Mac, for example a dock's "Microphone In". Entering PS5 mode:

1. starts a low-latency loop from that input to the default output (two HAL units and a small lock-free ring, 64-frame buffers, +6 dB with a soft limiter, about 13 ms on top of the output device's own latency);
2. asks for microphone access first if macOS hasn't decided yet, so the prompt shows before the screen goes away;
3. saves a hand-back record, then switches the monitor to the PS5's input over DDC/CI;
4. once the switch has gone through, detaches the monitor from macOS with BetterDisplay, as the PC's receiver hooks do, so windows stay on the other screens. `"detach_display": false` in `settings.json` turns this off.

Leaving PS5 mode stops the loop and hands the monitor back. It re-attaches the display first, waits two seconds for it to settle, then switches the monitor to the Mac's input (Settings, "Mac input", DisplayPort 1 by default). Nothing reads the monitor: a monitor showing another input can't be trusted to report it. The hand-back record is cleared only after that write succeeds; if a step fails, the hand-back stays owed. The record (`monitor-claim.json`) keeps the monitor, both inputs and whether it detached the display, as they were at entry, so Settings edits don't change it. It survives a crash, sleep or an unplugged dock and is retried on wake, when displays come back and at the next launch. Quitting waits for it, up to six seconds. Sleep ends PS5 mode; a locked or sleeping screen doesn't.

The loop follows the default output. It rebinds both its input and output after audio devices come and go (for example a dock that reboots), once they settle for a second, and restarts if its input stops delivering.

DDC rules:

- Monitors are found from the IORegistry only, with no EDID or I2C traffic.
- PS5 mode switches only the monitor saved in Settings, by a serial no other monitor shares. It never picks one on its own; with none saved, the menu says "Choose the PS5 monitor in Settings". The saved monitor may be the main display: detaching it moves the main display elsewhere, as the PC's receiver hooks do.
- Before every I2C transaction the monitors are looked up again; if the serial no longer maps to the same display port, the operation stops.
- DDC only writes, each write sent once. Operations are at least a second apart, queued switches collapse into the latest, and one lock file serializes DDC across the app, the CLI and hooks. Nothing reads or polls the monitor.

`hotswitch-sender ddc` lists the monitors and why any can't be used, sending nothing. `ddc set <vcp> <value> <serial>` follows the same rules. Only one Hotswitch runs at a time, so a single process owns the hand-back record.

A dry run (`open --env HOTSWITCH_DRY_RUN=1 Hotswitch.app`) tries modes and hand-back without touching hardware:
- no DDC service is ever opened, and DDC writes and BetterDisplay calls are only logged;
- hooks don't run, and the login item isn't touched;
- settings, the hand-back record and the log are kept apart, under `Application Support/Hotswitch/dry-run/` and `hotswitch-sender-dry-run.log`;
- the menu bar shows "DRY RUN", and the menu starts with "Dry run: monitors simulated, hooks off".

### HDMI switch

With an ESPHome HDMI switch sending the PS5 to one desk at a time (a select called "HDMI input" with outputs A, B, C, and LEDs "Input A/B/C LED"), each Mac follows it:

- **Entering PS5 mode** (shortcut or menu) first selects this Mac's output on the switch, waits up to 3 s for the switch's event stream to confirm, then switches the monitor and starts the audio. If the switch is unreachable or doesn't confirm, the local steps still run and the menu says so.
- **The event stream only makes a Mac leave.** When the switch moves to another output (the other Mac took the PS5, or someone pressed the button), this Mac leaves PS5 mode by itself and leaves the switch alone. Events never enter PS5 mode.
- **Leaving PS5 mode by this Mac's own hand** (shortcut, menu, quit or sleep) sends the switch back to the TV (A), but only while it still shows this Mac's output.

Per Mac, in `settings.json` (written owner-only, since it holds the switch's password):

```json
"hdmi_switch": { "urls": ["http://192.168.1.50", "http://hdmi-switch.local"], "user": "hotswitch", "password": "…", "output": "C" },
"modes": ["mac", "windows", "ps5"],
"detach_display": true,
"audio_input": "Microphone In"
```

`modes` lists what this Mac offers (a desk without a PC uses `["mac", "ps5"]`), `detach_display` is off for a single-monitor desk, and an empty `audio_input` means PS5 mode plays no audio. List the IP first: `.local` names can take seconds to resolve. Changes to `hdmi_switch` apply at the next launch.

Local builds report version 0.1.0 and never offer updates: a CI release would replace their signature and its permissions.

### macOS hooks

The sender runs optional executable hook scripts from:

```text
~/Library/Application Support/Hotswitch/hooks/
```

- `on-receiver-connected`: runs when the sender first sees the receiver.
- `on-receiver-disconnected`: runs when the receiver disappears, the sender quits, or the sender relaunches for an update/address change after having connected.

- `on-mode-changed <from> <to>`: runs on every mode change, with `mac`, `windows` or `ps5`.

Hooks run asynchronously during normal connection changes and are logged in `~/Library/Logs/hotswitch-sender.log`. Quit and relaunch cleanup waits for the disconnect hook to finish.

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

`build-sender-app.sh` signs the bundle with hardened runtime and `sender/macos/Hotswitch.entitlements` (audio input), using `$HOTSWITCH_SIGN_IDENTITY` or else the first "Apple Development" identity in the keychain. A stable signature keeps the Accessibility and Microphone permissions across rebuilds. An Apple Development certificate is only good on your own Macs; giving the app to anyone else needs a Developer ID certificate and notarization. CI signs releases with the self-signed "Hotswitch" certificate, also with hardened runtime and the same entitlements.

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
