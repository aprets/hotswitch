pub mod audio;
pub mod icon;
pub mod keymap;

/// Packet type tags
const TAG_MOUSE_MOVE: u8 = 0x01;
const TAG_MOUSE_BUTTON: u8 = 0x02;
const TAG_SCROLL: u8 = 0x03;
const TAG_KEY: u8 = 0x04;
const TAG_KEY_SYNC: u8 = 0x05;
const TAG_HEARTBEAT: u8 = 0x06;

/// Events sent over the wire between sender and receiver.
#[derive(Debug, Clone)]
pub enum Event {
    /// Relative mouse move (dx, dy as i16 after fractional accumulation on sender)
    MouseMove { dx: i16, dy: i16 },
    /// Mouse button press/release (button index, pressed)
    MouseButton { button: u8, pressed: bool },
    /// Scroll wheel (dx, dy — vertical scroll in dy, horizontal in dx)
    Scroll { dx: i16, dy: i16 },
    /// Key press/release (macOS CGKeyCode, pressed)
    Key { keycode: u16, pressed: bool },
    /// Periodic sync of all currently held keys (guards against dropped key-up packets).
    /// Limited to 255 keys on the wire; excess entries are silently truncated.
    KeySync { keys: Vec<u16> },
    /// Heartbeat (sender alive)
    Heartbeat,
}

impl Event {
    /// Serialize event into a byte buffer. Returns the number of bytes written.
    /// Required buffer size: 5 bytes for MouseMove/Scroll, 3 for MouseButton,
    /// 4 for Key, 1 for Heartbeat, or `2 + 2*keys.len()` for KeySync.
    pub fn to_bytes(&self, buf: &mut [u8]) -> usize {
        match self {
            Event::MouseMove { dx, dy } => {
                buf[0] = TAG_MOUSE_MOVE;
                buf[1..3].copy_from_slice(&dx.to_be_bytes());
                buf[3..5].copy_from_slice(&dy.to_be_bytes());
                5
            }
            Event::MouseButton { button, pressed } => {
                buf[0] = TAG_MOUSE_BUTTON;
                buf[1] = *button;
                buf[2] = *pressed as u8;
                3
            }
            Event::Scroll { dx, dy } => {
                buf[0] = TAG_SCROLL;
                buf[1..3].copy_from_slice(&dx.to_be_bytes());
                buf[3..5].copy_from_slice(&dy.to_be_bytes());
                5
            }
            Event::Key { keycode, pressed } => {
                buf[0] = TAG_KEY;
                buf[1..3].copy_from_slice(&keycode.to_be_bytes());
                buf[3] = *pressed as u8;
                4
            }
            Event::KeySync { keys } => {
                let count = keys.len().min(255);
                buf[0] = TAG_KEY_SYNC;
                buf[1] = count as u8;
                for (i, key) in keys.iter().take(count).enumerate() {
                    let offset = 2 + i * 2;
                    buf[offset..offset + 2].copy_from_slice(&key.to_be_bytes());
                }
                2 + count * 2
            }
            Event::Heartbeat => {
                buf[0] = TAG_HEARTBEAT;
                1
            }
        }
    }

    /// Deserialize event from a byte buffer.
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.is_empty() {
            return None;
        }
        match buf[0] {
            TAG_MOUSE_MOVE if buf.len() >= 5 => {
                let dx = i16::from_be_bytes([buf[1], buf[2]]);
                let dy = i16::from_be_bytes([buf[3], buf[4]]);
                Some(Event::MouseMove { dx, dy })
            }
            TAG_MOUSE_BUTTON if buf.len() >= 3 => Some(Event::MouseButton {
                button: buf[1],
                pressed: buf[2] != 0,
            }),
            TAG_SCROLL if buf.len() >= 5 => {
                let dx = i16::from_be_bytes([buf[1], buf[2]]);
                let dy = i16::from_be_bytes([buf[3], buf[4]]);
                Some(Event::Scroll { dx, dy })
            }
            TAG_KEY if buf.len() >= 4 => {
                let keycode = u16::from_be_bytes([buf[1], buf[2]]);
                Some(Event::Key {
                    keycode,
                    pressed: buf[3] != 0,
                })
            }
            TAG_KEY_SYNC if buf.len() >= 2 => {
                let count = buf[1] as usize;
                if buf.len() < 2 + count * 2 {
                    return None;
                }
                let keys: Vec<u16> = (0..count)
                    .map(|i| {
                        let offset = 2 + i * 2;
                        u16::from_be_bytes([buf[offset], buf[offset + 1]])
                    })
                    .collect();
                Some(Event::KeySync { keys })
            }
            TAG_HEARTBEAT => Some(Event::Heartbeat),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mouse_move() {
        let evt = Event::MouseMove { dx: -150, dy: 42 };
        let mut buf = [0u8; 64];
        let len = evt.to_bytes(&mut buf);
        let decoded = Event::from_bytes(&buf[..len]).unwrap();
        match decoded {
            Event::MouseMove { dx, dy } => {
                assert_eq!(dx, -150);
                assert_eq!(dy, 42);
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn roundtrip_key() {
        let evt = Event::Key {
            keycode: 0x00,
            pressed: true,
        };
        let mut buf = [0u8; 64];
        let len = evt.to_bytes(&mut buf);
        let decoded = Event::from_bytes(&buf[..len]).unwrap();
        match decoded {
            Event::Key { keycode, pressed } => {
                assert_eq!(keycode, 0x00);
                assert!(pressed);
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn roundtrip_key_sync() {
        let evt = Event::KeySync {
            keys: vec![0x00, 0x38, 0x7E],
        };
        let mut buf = [0u8; 64];
        let len = evt.to_bytes(&mut buf);
        let decoded = Event::from_bytes(&buf[..len]).unwrap();
        match decoded {
            Event::KeySync { keys } => {
                assert_eq!(keys, vec![0x00, 0x38, 0x7E]);
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn roundtrip_mouse_button() {
        let evt = Event::MouseButton {
            button: 2,
            pressed: true,
        };
        let mut buf = [0u8; 64];
        let len = evt.to_bytes(&mut buf);
        let decoded = Event::from_bytes(&buf[..len]).unwrap();
        match decoded {
            Event::MouseButton { button, pressed } => {
                assert_eq!(button, 2);
                assert!(pressed);
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn roundtrip_scroll() {
        let evt = Event::Scroll { dx: 3, dy: -120 };
        let mut buf = [0u8; 64];
        let len = evt.to_bytes(&mut buf);
        let decoded = Event::from_bytes(&buf[..len]).unwrap();
        match decoded {
            Event::Scroll { dx, dy } => {
                assert_eq!(dx, 3);
                assert_eq!(dy, -120);
            }
            _ => panic!("wrong event type"),
        }
    }

    #[test]
    fn roundtrip_heartbeat() {
        let evt = Event::Heartbeat;
        let mut buf = [0u8; 64];
        let len = evt.to_bytes(&mut buf);
        assert_eq!(len, 1);
        let decoded = Event::from_bytes(&buf[..len]).unwrap();
        assert!(matches!(decoded, Event::Heartbeat));
    }
}
