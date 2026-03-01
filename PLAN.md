# Hotswitch — Implementation Plan

## Context

Building a minimal software KVM (keyboard + mouse only, no video) to share a Mac's KB/M with a Windows gaming PC over LAN. The user is switching from Moonlight game streaming to a direct DP cable for display, and needs low-latency input forwarding with a hotkey toggle (press to send input to PC, press again to reclaim). Must feel as good as Moonlight for FPS gaming.

## Architecture

Two binaries, one shared protocol library:

```
Mac (sender)                    UDP datagrams              Windows (receiver)
CGEventTap ──────────────────> plain UDP, 3-5B ──────────> SendInput
+ CGAssociate...=false          no encryption               + KEYEVENTF_SCANCODE
+ hotkey toggle                 LAN only                    + MOUSEEVENTF_MOVE
```

### Protocol Design

Plain UDP, 1 byte type tag, no header overhead, big-endian:

| Event | Bytes | Layout |
|---|---|---|
| Mouse move | 5 | `[0x01][dx: i16 BE][dy: i16 BE]` |
| Mouse button | 3 | `[0x02][button: u8][pressed: u8]` |
| Scroll | 5 | `[0x03][dx: i16 BE][dy: i16 BE]` |
| Key event | 4 | `[0x04][cgkeycode: u16 BE][pressed: u8]` |
| Key sync | varies | `[0x05][count: u8][cgkeycode: u16 BE, ...]` — sent every 100ms, lists all held keys |
| Heartbeat | 1 | `[0x06]` |

Key decisions vs Moonlight/LAN Mouse:
- **i16 mouse deltas** (like Moonlight) — sender accumulates fractional remainder from CGEvent doubles, sends integer deltas. SendInput takes integers anyway.
- **No batching** (unlike Moonlight's 1ms batch) — bare UDP on LAN, send immediately
- **CGKeyCode on wire** (not VK codes or evdev) — only 2 platforms, map once on receiver
- **Key sync packets** every 100ms while captured — guards against dropped key-up UDP packets
- **No encryption** — LAN only, same switch

### Mac Sender (`sender/src/main.rs`, ~300 lines)

1. Parse config (target IP/port, hotkey combo)
2. Create UDP socket, connect to target
3. Set up CGEventTap at Session level for all mouse + keyboard events
4. In tap callback:
   - If event matches hotkey: toggle capture state
     - Capture ON: `CGAssociateMouseAndMouseCursorPosition(false)`, `CGDisplayHideCursor`, send key-sync
     - Capture OFF: `CGAssociateMouseAndMouseCursorPosition(true)`, `CGDisplayShowCursor`, release all keys on Windows
   - If capturing: serialize event → UDP send, return NULL (swallow)
   - If not capturing: return event (pass through)
5. Separate thread: heartbeat every 1s + key-sync every 100ms while captured
6. Run CFRunLoop

Uses **Moonlight's approach** (`CGAssociateMouseAndMouseCursorPosition(false)`) — not LAN Mouse's warp-to-edge.

### Windows Receiver (`receiver/src/main.rs`, ~180 lines)

1. Parse config (listen port)
2. Build CGKeyCode → (Windows scancode, extended flag) lookup table
3. UDP recv loop:
   - `0x01` → `SendInput(MOUSEEVENTF_MOVE, dx as i32, dy as i32)`
   - `0x02` → `SendInput(MOUSEEVENTF_*BUTTON*)`
   - `0x03` → `SendInput(MOUSEEVENTF_WHEEL/HWHEEL)`
   - `0x04` → map CGKeyCode → scancode, `SendInput(KEYEVENTF_SCANCODE)`
   - `0x05` → reconcile held keys, release any that shouldn't be held
   - `0x06` → update heartbeat timestamp

### Shared Protocol (`proto/src/lib.rs`, ~120 lines)

- Event enum + `to_bytes()` / `from_bytes()`
- CGKeyCode → Windows scancode table (`proto/src/keymap.rs`, ~130 entries lifted from LAN Mouse's scancode.rs)

## Project Structure

- `.gitignore` — ref/, target/
- `Cargo.toml` — workspace root
- `ref/` — reference repos (gitignored): lan-mouse, moonlight-qt, apollo
- `proto/src/lib.rs` — Event enum, serialize/deserialize
- `proto/src/keymap.rs` — CGKeyCode to Windows scancode table
- `sender/src/main.rs` — macOS capture + UDP send
- `receiver/src/main.rs` — UDP recv + SendInput
- `config.toml.example`

## Dependencies

- `proto`: none (stdlib only)
- `sender` (macOS): `core-graphics`, `core-foundation` (from servo/core-foundation-rs)
- `receiver` (Windows): `windows` crate (from microsoft/windows-rs) with features: `Win32_UI_Input_KeyboardAndMouse`, `Win32_UI_WindowsAndMessaging`
- Both: `toml` + `serde` for config

## Config Format

```toml
# Same file, platform-relevant section used
[sender]
target = "10.0.0.XXX:24801"
hotkey = "ctrl+escape"

[receiver]
listen = "0.0.0.0:24801"
```

## Build Order

1. **Project setup** — `mkdir ~/Projects/hotswitch`, `git init`, clone reference repos into `ref/`, add `ref/` to `.gitignore`, copy this plan into `PLAN.md` at project root
2. **proto crate** — Event types, serialization, keymap table
3. **receiver** (Windows) — simpler to test, can send test packets from Mac with a script
4. **sender** (macOS) — CGEventTap + CGAssociate + UDP
5. **End-to-end test** — hotkey toggle, mouse feel, key release

## Reference Repos (cloned into `ref/`, gitignored)

- `ref/lan-mouse/` — https://github.com/feschber/lan-mouse
- `ref/moonlight-qt/` — https://github.com/moonlight-stream/moonlight-qt (recursive)
- `ref/apollo/` — https://github.com/ClassicOldSong/Apollo

### What to reference from each:

**LAN Mouse** (`ref/lan-mouse/`) — primary reference for Rust + platform APIs:
- `input-event/src/scancode.rs` — macOS→evdev + evdev→Windows key mapping tables. We'll compose these into a direct CGKeyCode→Windows scancode table.
- `input-emulation/src/windows.rs` — `SendInput` patterns, `INPUT` struct construction, `KEYEVENTF_SCANCODE` usage, `send_input_safe` retry loop
- `input-capture/src/macos.rs` — CGEventTap creation, event type matching, `CGEventGetDoubleValueField` for deltas, `CGDisplayHideCursor`/`CGDisplayShowCursor`. We take the tap setup but replace their warp-to-edge approach with `CGAssociateMouseAndMouseCursorPosition(false)`
- `lan-mouse-proto/src/lib.rs` — simple binary encode/decode pattern (big-endian, no framing)

**Moonlight** (`ref/moonlight-qt/`) — reference for input feel / what "good" looks like:
- `moonlight-common-c/moonlight-common-c/src/Input.h` — packet struct definitions, proven wire format for gaming input
- `moonlight-common-c/moonlight-common-c/src/InputStream.c` — mouse delta batching with fractional accumulation, `INT16_MAX` overflow splitting, `LiSendMouseMoveEvent` as the gold standard relative mouse send
- `app/streaming/input/input.cpp` — `SDL_SetRelativeMouseMode(SDL_TRUE)` which calls `CGAssociateMouseAndMouseCursorPosition(false)` — the key technique we're adopting
- `app/streaming/input/keyboard.cpp` — SDL scancode → Windows VK code mapping (reference for completeness, we use CGKeyCode→scancode instead)

**Apollo/Sunshine** (`ref/apollo/`) — reference for Windows injection:
- `src/platform/windows/input.cpp` — `SendInput` with hardware scancodes, US English layout mapping, `syncThreadDesktop()` for UAC/secure desktop handling
- `src/video.cpp` — input-only mode architecture (not directly relevant but shows clean separation of concerns)

## Verification

1. `cargo build` on Mac (sender + proto), `cargo build` on Windows (receiver + proto)
2. Run receiver on Windows, sender on Mac
3. Press hotkey → cursor should disappear on Mac
4. Move mouse → cursor moves on Windows
5. Type keys → keys register on Windows
6. Press hotkey again → cursor reappears on Mac, all keys released on Windows
7. FPS test: compare mouse feel to Moonlight in a game
