//! DDC/CI monitor control on Apple Silicon, through the private IOAVService calls
//! that m1ddc and MonitorControl use. Only external displays answer.
//!
//! Safety rules, because monitors behind a dock can drop off when pushed:
//! - discovery reads IORegistry properties only, with no EDID or I2C traffic;
//! - a monitor is targeted only by a serial saved by the user, which no other monitor shares.
//!   Whether it is the main display doesn't matter: the PS5 monitor usually is, and detaching
//!   it moves the main display elsewhere, as the PC's receiver hooks already do;
//! - before every single I2C transaction the monitors are looked up again, and the operation
//!   stops if the serial no longer maps to the same display port;
//! - one DDC operation at a time across processes (a lock file); only writes, each sent once.
//!   Nothing reads the monitor: one showing another input can't be trusted to answer.
//!
//! In a dry run (`HOTSWITCH_DRY_RUN=1`) every rule above still applies, but no DDC service
//! is ever opened and writes are only logged.

use crate::settings::{dry_run, support_dir};
use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_void, CStr};
use std::fs::File;
use std::ptr;

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
    fn IOAVServiceWriteI2C(
        service: CFTypeRef,
        chip: u32,
        addr: u32,
        buf: *const u8,
        len: u32,
    ) -> i32;
}

/// The display's DDC/CI address (7-bit form of 0x6E).
const CHIP: u32 = 0x37;
/// Destination address as written on the wire.
const DEST: u8 = 0x6E;
/// The host's source address; it is sent and is part of the checksum.
const SOURCE: u8 = 0x51;

/// A monitor as the IORegistry describes it. Persisted in the hand-back record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    /// The alphanumeric serial, or the numeric one as text. Empty when the monitor has none.
    pub serial: String,
    pub serial_number: i64,
    pub vendor: i64,
    pub product: i64,
}

impl Identity {
    pub fn label(&self) -> String {
        format!("{} · {}", self.name, self.serial)
    }
}

/// A screen macOS is showing: which monitor, how far left it starts, and whether it has the
/// menu bar.
pub struct Screen {
    vendor: i64,
    product: i64,
    serial: i64,
    x: f64,
    main: bool,
}

pub fn screens() -> Vec<Screen> {
    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let d = CGDisplay::new(id);
            Screen {
                vendor: d.vendor_number() as i64,
                product: d.model_number() as i64,
                serial: d.serial_number() as i64,
                x: d.bounds().origin.x,
                main: d.is_main(),
            }
        })
        .collect()
}

/// Where a monitor is, in words a person can match to the desk: "Left, menu bar", "Right",
/// "Only screen" or "Not showing" (switched away or detached).
pub fn placement(id: &Identity, screens: &[Screen]) -> String {
    let Some(me) = screens
        .iter()
        .find(|s| s.vendor == id.vendor && s.product == id.product && s.serial == id.serial_number)
    else {
        return "Not showing".into();
    };
    let left_of_me = screens.iter().filter(|s| s.x < me.x).count();
    let position = match (screens.len(), left_of_me) {
        (1, _) => "Only screen",
        (_, 0) => "Left",
        (n, i) if i == n - 1 => "Right",
        _ => "Middle",
    };
    if me.main {
        format!("{position}, menu bar")
    } else {
        position.to_string()
    }
}

/// Monitors that can be targeted: they have a serial, and no other monitor shares it.
pub fn targetable(found: &[Identity]) -> Vec<Identity> {
    found
        .iter()
        .filter(|id| !id.serial.is_empty())
        .filter(|id| found.iter().filter(|o| o.serial == id.serial).count() == 1)
        .cloned()
        .collect()
}

/// The monitor saved for PS5 mode. Never picks one on its own: with no serial saved it
/// refuses, since any monitor could be the wrong one.
pub fn resolve(found: &[Identity], wanted: Option<&str>) -> Result<Identity, String> {
    let Some(serial) = wanted.filter(|s| !s.is_empty()) else {
        return Err("Choose the PS5 monitor in Settings".into());
    };
    let mut matches = found.iter().filter(|id| id.serial == serial);
    let id = matches
        .next()
        .ok_or_else(|| format!("Monitor {serial} isn't connected"))?;
    if matches.next().is_some() {
        return Err(format!("Another monitor shares serial {serial}"));
    }
    Ok(id.clone())
}

/// Which monitor sits on which display port. Compared before every transaction, so a write
/// never reaches a different monitor than the one opened.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// (framebuffer port like "dispext0", identity), sorted.
    framebuffers: Vec<(String, Identity)>,
}

impl Layout {
    pub fn read() -> Layout {
        let mut framebuffers = Vec::new();
        unsafe {
            for_each_service(c"IOMobileFramebufferShim", |entry| {
                if let (Some(port), Some(id)) = (parent_name(entry), identity(entry)) {
                    framebuffers.push((port, id));
                }
            });
        }
        framebuffers.sort_by(|a, b| a.0.cmp(&b.0));
        Layout { framebuffers }
    }

    /// Monitors with a DDC service (registry only).
    pub fn found(&self) -> Vec<Identity> {
        let mut out = Vec::new();
        unsafe {
            for_each_service(c"DCPAVServiceProxy", |entry| {
                if let Some(id) = self.proxy_identity(entry) {
                    out.push(id.clone());
                }
            });
        }
        out
    }

    pub fn targetable(&self) -> Vec<Identity> {
        targetable(&self.found())
    }

    unsafe fn proxy_identity(&self, entry: IoObject) -> Option<&Identity> {
        if registry_string(entry, "Location").as_deref() != Some("External") {
            return None;
        }
        // The proxy's parent is "dispextN:dcpav-service-epic:0"; its framebuffer's is "dispextN".
        let port = parent_name(entry)?.split(':').next()?.to_owned();
        self.framebuffers
            .iter()
            .find(|(p, _)| *p == port)
            .map(|(_, id)| id)
    }
}

/// An open DDC target. Holds the cross-process DDC lock while it lives.
pub struct Target {
    pub identity: Identity,
    layout: Layout,
    service: CFTypeRef,
    _lock: File,
}

// The IOAVService handle is a plain CF object; it is used from one thread at a time.
unsafe impl Send for Target {}

impl Drop for Target {
    fn drop(&mut self) {
        if !self.service.is_null() {
            unsafe { CFRelease(self.service) }
        }
    }
}

impl Target {
    /// Open `identity` for DDC if it is connected and the only monitor with its serial.
    pub fn open(identity: &Identity) -> Result<Target, String> {
        let lock = lock()?;
        let layout = Layout::read();
        if !layout.targetable().contains(identity) {
            return Err(format!(
                "{} isn't connected, or another monitor shares its serial",
                identity.name
            ));
        }
        let mut service: CFTypeRef = ptr::null();
        if !dry_run() {
            unsafe {
                for_each_service(c"DCPAVServiceProxy", |entry| {
                    if service.is_null() && layout.proxy_identity(entry) == Some(identity) {
                        service = IOAVServiceCreateWithService(ptr::null(), entry);
                    }
                });
            }
            if service.is_null() {
                return Err(format!("{} has no DDC service", identity.name));
            }
        }
        Ok(Target {
            identity: identity.clone(),
            layout,
            service,
            _lock: lock,
        })
    }

    /// Stop when anything about the displays changed since the target was opened.
    fn guard(&self) -> Result<(), String> {
        if Layout::read() == self.layout {
            Ok(())
        } else {
            Err("The displays changed, so DDC stopped".into())
        }
    }

    /// One write, no retries.
    pub fn write(&self, vcp: u8, value: u16) -> Result<(), String> {
        self.guard()?;
        if dry_run() {
            eprintln!(
                "ddc dry run: {} vcp {vcp:#04x} = {value}",
                self.identity.label()
            );
            return Ok(());
        }
        if unsafe { self.send(&set_request(vcp, value)) } == 0 {
            Ok(())
        } else {
            Err(format!("DDC write to {} failed", self.identity.name))
        }
    }

    unsafe fn send(&self, packet: &[u8]) -> i32 {
        if self.service.is_null() {
            return -1;
        }
        IOAVServiceWriteI2C(
            self.service,
            CHIP,
            SOURCE as u32,
            packet.as_ptr(),
            packet.len() as u32,
        )
    }
}

fn checksum(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |c, b| c ^ b)
}

/// Set VCP Feature request.
fn set_request(vcp: u8, value: u16) -> [u8; 6] {
    let mut p = [0x84, 0x03, vcp, (value >> 8) as u8, value as u8, 0];
    p[5] = checksum(DEST ^ SOURCE, &p[..5]);
    p
}

/// One DDC operation at a time, across the app, the CLI and hooks.
fn lock() -> Result<File, String> {
    let dir = support_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("ddc.lock"))
        .map_err(|e| format!("DDC lock: {e}"))?;
    file.lock().map_err(|e| format!("DDC lock: {e}"))?;
    Ok(file)
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
    let serial_number = number("SerialNumber").unwrap_or(0);
    let serial = text("AlphanumericSerialNumber")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match serial_number {
            0 => String::new(),
            n => n.to_string(),
        });
    Some(Identity {
        name: text("ProductName").unwrap_or_else(|| "External display".into()),
        serial,
        serial_number,
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

/// `hotswitch-sender ddc [set <vcp> <value> <serial>]`, for debugging
/// and hooks. The plain listing sends nothing to any monitor and says which monitors can be
/// targeted. The same rules apply here as in the app.
pub fn cli(args: &[String]) {
    let parse = |s: &str| match s.strip_prefix("0x") {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    };
    let open = |serial: Option<&String>| -> Result<Target, String> {
        let layout = Layout::read();
        Target::open(&resolve(&layout.found(), serial.map(String::as_str))?)
    };
    let run = || -> Result<(), String> {
        match args {
            [] => {
                let found = Layout::read().found();
                let targetable = targetable(&found);
                for id in &found {
                    let note = match () {
                        _ if id.serial.is_empty() => "not used: no serial",
                        _ if !targetable.contains(id) => "not used: serial not unique",
                        _ => "usable",
                    };
                    println!("{}\tserial {}\t{note}", id.name, id.serial);
                }
                Ok(())
            }
            [cmd, vcp, value, serial] if cmd == "set" => {
                let (vcp, value) = parse(vcp).zip(parse(value)).ok_or("bad arguments")?;
                open(Some(serial))?.write(vcp as u8, value)
            }
            _ => Err("usage: hotswitch-sender ddc [set <vcp> <value> <serial>]".into()),
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

    fn monitor(serial: &str, serial_number: i64) -> Identity {
        Identity {
            name: "Example 27".into(),
            serial: serial.into(),
            serial_number,
            vendor: 4242,
            product: 1001,
        }
    }

    #[test]
    fn set_request_checksum() {
        assert_eq!(
            set_request(0x60, 0x0F),
            [0x84, 0x03, 0x60, 0x00, 0x0F, 0xD7]
        );
        assert_eq!(
            set_request(0x60, 0x11),
            [0x84, 0x03, 0x60, 0x00, 0x11, 0xC9]
        );
    }

    #[test]
    fn duplicate_or_missing_serials_are_never_targets() {
        let found = [
            monitor("SAME", 1),
            monitor("SAME", 2),
            monitor("OTHER", 3),
            monitor("", 0),
        ];
        assert_eq!(targetable(&found), vec![monitor("OTHER", 3)]);
        assert!(resolve(&found, Some("SAME")).is_err());
        assert!(resolve(&found, Some("")).is_err());
        assert!(resolve(&found, Some("MISSING")).is_err());
        assert_eq!(resolve(&found, Some("OTHER")), Ok(monitor("OTHER", 3)));
    }

    #[test]
    fn placement_reads_like_the_desk() {
        let screen = |serial, x, main| Screen {
            vendor: 4242,
            product: 1001,
            serial,
            x,
            main,
        };
        let (left, right) = (monitor("L", 111), monitor("R", 222));
        let both = [screen(222, 2560.0, false), screen(111, 0.0, true)];
        assert_eq!(placement(&left, &both), "Left, menu bar");
        assert_eq!(placement(&right, &both), "Right");
        let one = [screen(222, 0.0, true)];
        assert_eq!(placement(&right, &one), "Only screen, menu bar");
        assert_eq!(placement(&left, &one), "Not showing");
    }

    #[test]
    fn ps5_mode_targets_exactly_the_saved_monitor() {
        // 23 Nov: with nothing saved, the only other candidate was the wrong screen. Never guess.
        let ps5 = monitor("PS5MON", 111);
        let found = [ps5.clone(), monitor("OTHERMON", 222)];
        assert_eq!(
            resolve(&found, None),
            Err("Choose the PS5 monitor in Settings".into())
        );
        assert_eq!(
            resolve(&[ps5.clone()], None),
            Err("Choose the PS5 monitor in Settings".into())
        );
        // The saved monitor is used whether or not it shows the menu bar: in Artur's setup the
        // PS5 monitor usually is the main display.
        assert_eq!(resolve(&found, Some("PS5MON")), Ok(ps5));
    }
}
