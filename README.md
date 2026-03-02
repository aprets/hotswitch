# Hotswitch

Minimal software KVM for sharing a Mac's keyboard and mouse with a Windows PC over LAN. Press a hotkey to send input to the PC, press again to reclaim it. Built for low-latency FPS gaming — plain UDP, no video, no encryption, just raw input forwarding.

## How it works

```
Mac (sender)                  UDP / LAN              Windows (receiver)
CGEventTap ───────────────> 3–5 byte datagrams ────> SendInput
```

The sender captures keyboard and mouse events on macOS via `CGEventTap`, serializes them into tiny UDP packets, and sends them to the receiver. The receiver injects them as native Windows input via `SendInput` with hardware scancodes.

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

Download `hotswitch-receiver.exe` from [Releases](https://github.com/aprets/hotswitch/releases) and run:

```
hotswitch-receiver.exe 0.0.0.0:24801
```

### Controls

- **Ctrl+Escape** — toggle capture (send input to Windows / reclaim on Mac)
- Both sides run as **system tray apps** with status icons and menus

### Tray menu (both sides)

- Status line (Waiting / Connected / Capturing)
- Check for Updates (auto-checks on startup)
- Show Log
- Start on Login
- Quit

## Building from source

Requires [Rust](https://rustup.rs/).

```bash
# Sender (on macOS)
cargo build --release -p hotswitch-sender

# Receiver (on Windows)
cargo build --release -p hotswitch-receiver

# Protocol tests (any platform)
cargo test -p hotswitch-proto
```

## Releasing

Tag a version and push — GitHub Actions builds both binaries and publishes a release:

```bash
git tag v0.2.0
git push origin v0.2.0
```

## Protocol

Plain UDP, 1-byte type tag, big-endian, no framing:

| Event | Bytes | Layout |
|---|---|---|
| Mouse move | 5 | `01 dx:i16 dy:i16` |
| Mouse button | 3 | `02 button:u8 pressed:u8` |
| Scroll | 5 | `03 dx:i16 dy:i16` |
| Key event | 4 | `04 cgkeycode:u16 pressed:u8` |
| Key sync | 2+2n | `05 count:u8 [cgkeycode:u16]...` |
| Heartbeat | 1 | `06` |

## Project structure

```
hotswitch/
  proto/          Shared protocol library (Event types, keymap, icon generation)
  sender/         macOS sender (CGEventTap + UDP + tray icon)
  receiver/       Windows receiver (UDP + SendInput + tray icon)
```

## License

[MIT](LICENSE)
