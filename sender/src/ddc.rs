//! DDC/CI monitor control on Apple Silicon, through the private IOAVService calls
//! that m1ddc and MonitorControl use. Only external displays answer.
//!
//! Gentle by design, because monitors behind a dock can drop off when pushed:
//! - monitors are found from IORegistry properties alone, with no EDID or I2C traffic;
//! - the monitor showing the Mac's main display is never touched;
//! - a read is at most two tries with a pause between, a write is sent once;
//! - nothing polls: callers read and write only when the user switches modes.

use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::thread::sleep;
use std::time::Duration;

/// VCP code for the input source (MCCS).
pub const VCP_INPUT: u8 = 0x60;

/// Common VCP 0x60 values. Monitors may use others; the settings window lists these.
pub const INPUTS: [(u16, &str); 5] = [
    (0x11, "HDMI 1"),
    (0x12, "HDMI 2"),
    (0x0F, "DisplayPort 1"),
    (0x10, "DisplayPort 2"),
    (0x1B, "USB-C"),
];

pub fn input_name(value: u16) -> String {
    INPUTS
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| format!("input {value}"))
}

type IoObject = u32;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(port: u32, matching: *mut c_void, iter: *mut IoObject) -> i32;
    fn IOIteratorNext(iter: IoObject) -> IoObject;
    fn IOObjectRelease(obj: IoObject) -> i32;
    fn IORegistryEntryGetParentEntry(
        entry: IoObject,
        plane: *const c_char,
        parent: *mut IoObject,
    ) -> i32;
    fn IORegistryEntryGetName(entry: IoObject, name: *mut c_char) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        allocator: *const c_void,
        options: u32,
    ) -> CFTypeRef;
    fn IOAVServiceCreateWithService(allocator: *const c_void, service: IoObject) -> CFTypeRef;
    fn IOAVServiceReadI2C(
        service: CFTypeRef,
        chip: u32,
        offset: u32,
        buf: *mut u8,
        len: u32,
    ) -> i32;
    fn IOAVServiceWriteI2C(
        service: CFTypeRef,
        chip: u32,
        addr: u32,
        buf: *const u8,
        len: u32,
    ) -> i32;
}

const CHIP: u32 = 0x37;
const DATA: u8 = 0x51;
/// DDC/CI asks for 40 ms between a request and reading its reply.
const REPLY_WAIT: Duration = Duration::from_millis(50);
const RETRY_WAIT: Duration = Duration::from_millis(250);

/// What the registry says about the display on one framebuffer.
#[derive(Clone, Debug, PartialEq)]
struct Identity {
    name: String,
    serial_text: String,
    serial: i64,
    vendor: i64,
    product: i64,
}

impl Identity {
    /// Same model as the main display, and the serials don't tell them apart.
    fn could_be(&self, vendor: i64, product: i64, serial: i64) -> bool {
        self.vendor == vendor
            && self.product == product
            && (self.serial == serial || self.serial == 0 || serial == 0)
    }
}

pub struct Monitor {
    pub name: String,
    pub serial: String,
    service: CFTypeRef,
}

// The IOAVService handle is a plain CF object; it is used from one thread at a time.
unsafe impl Send for Monitor {}

impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe { CFRelease(self.service) }
    }
}

impl Monitor {
    /// Stable id for settings: the serial, or the name when the monitor has none.
    pub fn id(&self) -> &str {
        if self.serial.is_empty() {
            &self.name
        } else {
            &self.serial
        }
    }

    /// One write, no retries.
    pub fn set(&self, vcp: u8, value: u16) -> Result<(), String> {
        if unsafe { self.write(&set_packet(vcp, value)) } == 0 {
            Ok(())
        } else {
            Err(format!("DDC write to {} failed", self.name))
        }
    }

    /// Current value of a VCP code: at most two tries, a pause between them.
    pub fn get(&self, vcp: u8) -> Result<u16, String> {
        let request = [0x82, 0x01, vcp, 0x6E ^ 0x82 ^ 0x01 ^ vcp];
        for attempt in 0..2 {
            if attempt > 0 {
                sleep(RETRY_WAIT);
            }
            if unsafe { self.write(&request) } != 0 {
                continue;
            }
            sleep(REPLY_WAIT);
            let mut reply = [0u8; 11];
            let read = unsafe {
                IOAVServiceReadI2C(self.service, CHIP, DATA as u32, reply.as_mut_ptr(), 11)
            };
            if read == 0 {
                if let Some(value) = reply_value(&reply, vcp) {
                    return Ok(value);
                }
            }
        }
        Err(format!("DDC read from {} failed", self.name))
    }

    unsafe fn write(&self, packet: &[u8]) -> i32 {
        IOAVServiceWriteI2C(
            self.service,
            CHIP,
            DATA as u32,
            packet.as_ptr(),
            packet.len() as u32,
        )
    }
}

fn set_packet(vcp: u8, value: u16) -> [u8; 6] {
    let mut packet = [0x84, 0x03, vcp, (value >> 8) as u8, value as u8, 0];
    packet[5] = packet[..5].iter().fold(0x6E ^ DATA, |c, b| c ^ b);
    packet
}

/// The current value from a "VCP feature reply", if the reply is valid and for `vcp`.
fn reply_value(reply: &[u8; 11], vcp: u8) -> Option<u16> {
    let checksum = reply[..10].iter().fold(0x50, |c, b| c ^ b);
    (checksum == reply[10] && reply[2] == 0x02 && reply[3] == 0 && reply[4] == vcp)
        .then(|| u16::from_be_bytes([reply[8], reply[9]]))
}

/// External monitors that DDC may talk to. Leaves out any monitor that could be the one
/// showing the Mac's main display, and any whose identity the registry doesn't give.
pub fn monitors() -> Vec<Monitor> {
    let framebuffers = framebuffer_identities();
    let main = CGDisplay::main();
    let (vendor, product, serial) = (
        main.vendor_number() as i64,
        main.model_number() as i64,
        main.serial_number() as i64,
    );
    let mut out = Vec::new();
    unsafe {
        for_each_service(c"DCPAVServiceProxy", |entry| {
            if registry_string(entry, "Location").as_deref() != Some("External") {
                return;
            }
            // The proxy's parent is "dispextN:dcpav-service-epic:0"; its framebuffer's is "dispextN".
            let Some(port) =
                parent_name(entry).and_then(|n| n.split(':').next().map(str::to_owned))
            else {
                return;
            };
            let Some(id) = framebuffers
                .iter()
                .find(|(p, _)| *p == port)
                .map(|(_, id)| id)
            else {
                return;
            };
            if id.could_be(vendor, product, serial) {
                return;
            }
            let service = IOAVServiceCreateWithService(ptr::null(), entry);
            if !service.is_null() {
                out.push(Monitor {
                    name: id.name.clone(),
                    serial: id.serial_text.clone(),
                    service,
                });
            }
        });
    }
    out
}

/// The monitor with this id, or the first one when no id is set. `None` when the saved
/// monitor is gone (or shows the main display), so another monitor is never switched instead.
pub fn find(id: Option<&str>) -> Option<Monitor> {
    let mut all = monitors();
    let index = match id {
        Some(id) => all.iter().position(|m| m.id() == id)?,
        None => 0,
    };
    (index < all.len()).then(|| all.swap_remove(index))
}

/// (framebuffer parent name like "dispext0", identity) for every framebuffer with a display.
fn framebuffer_identities() -> Vec<(String, Identity)> {
    let mut out = Vec::new();
    unsafe {
        for_each_service(c"IOMobileFramebufferShim", |entry| {
            if let (Some(port), Some(id)) = (parent_name(entry), identity(entry)) {
                out.push((port, id));
            }
        });
    }
    out
}

unsafe fn identity(entry: IoObject) -> Option<Identity> {
    let key = CFString::new("DisplayAttributes");
    let value = IORegistryEntryCreateCFProperty(entry, key.as_concrete_TypeRef(), ptr::null(), 0);
    if value.is_null() {
        return None;
    }
    let attributes = CFType::wrap_under_create_rule(value).downcast::<CFDictionary>()?;
    let product = dict_get(&attributes, "ProductAttributes")?.downcast::<CFDictionary>()?;
    let text = |key| {
        dict_get(&product, key)
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string())
    };
    let number = |key| {
        dict_get(&product, key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
    };
    let serial = number("SerialNumber").unwrap_or(0);
    Some(Identity {
        name: text("ProductName").unwrap_or_else(|| "External display".into()),
        serial_text: text("AlphanumericSerialNumber")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if serial == 0 {
                    String::new()
                } else {
                    serial.to_string()
                }
            }),
        serial,
        vendor: number("LegacyManufacturerID")?,
        product: number("ProductID")?,
    })
}

fn dict_get(dict: &CFDictionary, key: &str) -> Option<CFType> {
    let key = CFString::new(key);
    dict.find(key.as_CFTypeRef() as *const c_void)
        .map(|v| unsafe { CFType::wrap_under_get_rule(*v as CFTypeRef) })
}

unsafe fn for_each_service(class: &CStr, mut f: impl FnMut(IoObject)) {
    let mut iter: IoObject = 0;
    if IOServiceGetMatchingServices(0, IOServiceMatching(class.as_ptr()), &mut iter) != 0 {
        return;
    }
    loop {
        let entry = IOIteratorNext(iter);
        if entry == 0 {
            break;
        }
        f(entry);
        IOObjectRelease(entry);
    }
    IOObjectRelease(iter);
}

unsafe fn parent_name(entry: IoObject) -> Option<String> {
    let mut parent: IoObject = 0;
    if IORegistryEntryGetParentEntry(entry, c"IOService".as_ptr(), &mut parent) != 0 {
        return None;
    }
    let mut name = [0 as c_char; 128];
    let ok = IORegistryEntryGetName(parent, name.as_mut_ptr()) == 0;
    IOObjectRelease(parent);
    ok.then(|| CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned())
}

unsafe fn registry_string(entry: IoObject, key: &str) -> Option<String> {
    let key = CFString::new(key);
    let value = IORegistryEntryCreateCFProperty(entry, key.as_concrete_TypeRef(), ptr::null(), 0);
    if value.is_null() {
        return None;
    }
    CFType::wrap_under_create_rule(value)
        .downcast::<CFString>()
        .map(|s| s.to_string())
}

/// `hotswitch-sender ddc [get <vcp> [monitor] | set <vcp> <value> [monitor]]`, for debugging
/// and hooks. The plain listing sends nothing to the monitors. `monitor` is a serial or name
/// from that listing; the main display's monitor is never listed or touched.
pub fn cli(args: &[String]) {
    let parse = |s: &str| match s.strip_prefix("0x") {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    };
    let run = || -> Result<(), String> {
        match args {
            [] => {
                for m in monitors() {
                    println!("{}\tserial {}", m.name, m.serial);
                }
                Ok(())
            }
            [cmd, vcp, rest @ ..] if cmd == "get" && rest.len() <= 1 => {
                let m = find(rest.first().map(String::as_str)).ok_or("no such monitor")?;
                let vcp = parse(vcp).ok_or("bad VCP code")?;
                m.get(vcp as u8).map(|v| println!("{v}"))
            }
            [cmd, vcp, value, rest @ ..] if cmd == "set" && rest.len() <= 1 => {
                let m = find(rest.first().map(String::as_str)).ok_or("no such monitor")?;
                let (vcp, value) = parse(vcp).zip(parse(value)).ok_or("bad arguments")?;
                m.set(vcp as u8, value)
            }
            _ => Err(
                "usage: hotswitch-sender ddc [get <vcp> [monitor] | set <vcp> <value> [monitor]]"
                    .into(),
            ),
        }
    };
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_packet_checksum() {
        // Input select to DisplayPort 1, as m1ddc builds it.
        let p = set_packet(0x60, 0x0F);
        assert_eq!(p[..5], [0x84, 0x03, 0x60, 0x00, 0x0F]);
        assert_eq!(p[5], 0x6E ^ 0x51 ^ 0x84 ^ 0x03 ^ 0x60 ^ 0x00 ^ 0x0F);
    }

    #[test]
    fn reply_parsing() {
        let mut reply = [
            0x6E, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0x12, 0x00, 0x11, 0,
        ];
        reply[10] = reply[..10].iter().fold(0x50, |c, b| c ^ b);
        assert_eq!(reply_value(&reply, 0x60), Some(0x11));
        assert_eq!(reply_value(&reply, 0x10), None);
        reply[10] ^= 1;
        assert_eq!(reply_value(&reply, 0x60), None);
    }

    #[test]
    fn main_display_match_is_conservative() {
        let id = Identity {
            name: "Example 27".into(),
            serial_text: "TEST0001".into(),
            serial: 12345678,
            vendor: 4242,
            product: 1001,
        };
        assert!(id.could_be(4242, 1001, 12345678));
        assert!(!id.could_be(4242, 1001, 87654321));
        assert!(id.could_be(4242, 1001, 0));
        assert!(!id.could_be(4242, 1002, 12345678));
    }
}
