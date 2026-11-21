//! Low-latency audio passthrough: a local input (the dock's "Microphone In", fed by the
//! monitor's audio out) played to the default output. Two HAL units joined by a lock-free
//! ring that trims clock drift, ported from the PS5 Loop app it replaces.

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers::{
    audio_unit_from_device_id, get_audio_device_ids_for_scope, get_default_device_id,
    get_device_name,
};
use coreaudio::audio_unit::render_callback::{data, Args};
use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
use coreaudio::sys;
use objc2::msg_send;
use objc2::runtime::AnyClass;
use objc2_foundation::ns_string;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

const RING_FRAMES: usize = 16384;
/// IO buffer asked of both devices; they may round it up.
const IO_FRAMES: u32 = 64;
/// The dock's preamp distorts above about +18 dB, so the rest of the level comes from here.
const GAIN_DB: f32 = 6.0;

/// Interleaved stereo ring, one writer (input thread) and one reader (output thread).
struct Ring {
    samples: Box<[AtomicU32]>,
    write: AtomicUsize,
    read: AtomicUsize,
    /// Frames kept queued, about two IO periods.
    target: AtomicUsize,
    reprime: AtomicBool,
    /// Input peak since the last status read, as f32 bits.
    peak: AtomicU32,
}

pub struct Passthrough {
    input_name: String,
    input_id: u32,
    in_rate: f64,
    in_frames: u32,
    in_latency_ms: f64,
    output_id: u32,
    output_name: String,
    ring: Arc<Ring>,
    // Dropping a unit stops it; the input goes first.
    input_unit: AudioUnit,
    output_unit: AudioUnit,
}

pub struct Status {
    /// Input peak in dBFS since the last read; `None` when silent.
    pub peak_db: Option<f32>,
    pub output: String,
    /// What the loop itself adds: input device plus ring. The output device's own latency
    /// (about 165 ms for AirPods) comes on top and is in the log.
    pub loop_ms: f64,
}

impl Passthrough {
    pub fn start(input_name: &str) -> Result<Self, String> {
        let input_id = input_devices()
            .into_iter()
            .find(|(_, name)| name == input_name)
            .map(|(id, _)| id)
            .ok_or_else(|| format!("{input_name} not found"))?;
        let in_rate = get::<f64>(
            input_id,
            sys::kAudioDevicePropertyNominalSampleRate,
            sys::kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or(48000.0);
        let in_frames = set_buffer_frames(input_id, sys::kAudioObjectPropertyScopeInput);
        let asbd = StreamFormat {
            sample_rate: in_rate,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels: 2,
        }
        .to_asbd();
        let ring = Arc::new(Ring {
            samples: (0..RING_FRAMES * 2).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            target: AtomicUsize::new(256),
            reprime: AtomicBool::new(false),
            peak: AtomicU32::new(0),
        });

        let mut input = audio_unit_from_device_id(input_id, true).map_err(err("input unit"))?;
        input.uninitialize().map_err(err("input unit"))?;
        input
            .set_property(
                sys::kAudioUnitProperty_StreamFormat,
                Scope::Output,
                Element::Input,
                Some(&asbd),
            )
            .map_err(err("input format"))?;
        let gain = 10f32.powf(GAIN_DB / 20.0);
        let writer = ring.clone();
        input
            .set_input_callback(move |args: Args<data::Interleaved<f32>>| {
                let ring = &writer;
                let channels = args.data.channels.max(1);
                let mut w = ring.write.load(Ordering::Relaxed);
                let mut peak = 0f32;
                for frame in args.data.buffer.chunks_exact(channels) {
                    let (l, r) = (frame[0], *frame.get(1).unwrap_or(&frame[0]));
                    peak = peak.max(l.abs()).max(r.abs());
                    let p = (w % RING_FRAMES) * 2;
                    ring.samples[p].store(limit(l * gain).to_bits(), Ordering::Relaxed);
                    ring.samples[p + 1].store(limit(r * gain).to_bits(), Ordering::Relaxed);
                    w += 1;
                }
                ring.write.store(w, Ordering::Release);
                ring.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
                Ok(())
            })
            .map_err(err("input callback"))?;

        // DefaultOutput follows the system default device and converts the sample rate.
        let mut output = AudioUnit::new(IOType::DefaultOutput).map_err(err("output unit"))?;
        output.uninitialize().map_err(err("output unit"))?;
        output
            .set_property(
                sys::kAudioUnitProperty_StreamFormat,
                Scope::Input,
                Element::Output,
                Some(&asbd),
            )
            .map_err(err("output format"))?;
        let reader = ring.clone();
        let mut primed = false;
        output
            .set_render_callback(move |args: Args<data::Interleaved<f32>>| {
                let ring = &reader;
                let channels = args.data.channels.max(1);
                let n = args.num_frames;
                let out = args.data.buffer;
                if ring.reprime.swap(false, Ordering::Relaxed) {
                    primed = false;
                }
                let w = ring.write.load(Ordering::Acquire);
                let mut r = ring.read.load(Ordering::Relaxed);
                let target = ring.target.load(Ordering::Relaxed);
                let mut avail = w.wrapping_sub(r);
                if !primed {
                    if avail < target + n {
                        out.fill(0.0);
                        return Ok(());
                    }
                    primed = true;
                }
                // The dock and the output run on different clocks: drop back to target when behind.
                if avail > target * 2 + n {
                    r = w - target - n;
                    avail = target + n;
                }
                for (i, frame) in out.chunks_exact_mut(channels).enumerate() {
                    frame.fill(0.0);
                    if i < avail {
                        let p = (r % RING_FRAMES) * 2;
                        for (c, sample) in frame.iter_mut().take(2).enumerate() {
                            *sample = f32::from_bits(ring.samples[p + c].load(Ordering::Relaxed));
                        }
                        r += 1;
                    }
                }
                if avail < n {
                    primed = false;
                }
                ring.read.store(r, Ordering::Release);
                Ok(())
            })
            .map_err(err("output callback"))?;

        input.initialize().map_err(err("input unit"))?;
        output.initialize().map_err(err("output unit"))?;

        let mut this = Self {
            input_name: input_name.to_string(),
            input_id,
            in_rate,
            in_frames,
            in_latency_ms: latency_ms(input_id, sys::kAudioObjectPropertyScopeInput, in_rate).0,
            output_id: 0,
            output_name: String::new(),
            ring,
            input_unit: input,
            output_unit: output,
        };
        this.apply_output();
        this.input_unit.start().map_err(err("input start"))?;
        this.output_unit.start().map_err(err("output start"))?;
        Ok(this)
    }

    /// Re-targets the ring when the default output has changed. The DefaultOutput unit
    /// itself already follows it.
    pub fn follow_default_output(&mut self) {
        if get_default_device_id(false).is_some_and(|id| id != self.output_id) {
            self.apply_output();
        }
    }

    /// True when the devices this loop is bound to are gone or replaced, so it must be
    /// dropped and started again: the input's name now belongs to another device (a dock
    /// that rebooted comes back under a new id), or either device is no longer alive.
    pub fn needs_rebind(&self) -> bool {
        let input_now = input_devices()
            .into_iter()
            .find(|(_, name)| *name == self.input_name)
            .map(|(id, _)| id);
        input_now != Some(self.input_id) || !self.is_alive()
    }

    /// Cheap check that both bound devices still exist.
    pub fn is_alive(&self) -> bool {
        is_alive(self.input_id) && is_alive(self.output_id)
    }

    pub fn status(&self) -> Status {
        let peak = f32::from_bits(self.ring.peak.swap(0, Ordering::Relaxed));
        let target = self.ring.target.load(Ordering::Relaxed);
        Status {
            peak_db: (peak > 1e-4).then(|| 20.0 * peak.log10()),
            output: self.output_name.clone(),
            loop_ms: self.in_latency_ms + target as f64 / self.in_rate * 1000.0,
        }
    }

    fn apply_output(&mut self) {
        let Some(out_id) = get_default_device_id(false) else {
            return;
        };
        let out_frames = set_buffer_frames(out_id, sys::kAudioObjectPropertyScopeOutput);
        let out_rate = get::<f64>(
            out_id,
            sys::kAudioDevicePropertyNominalSampleRate,
            sys::kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or(48000.0);
        // The output pulls in input-rate frames after conversion; keep two IO periods queued.
        let pulled = (out_frames as f64 * self.in_rate / out_rate) as u32;
        let target = self.in_frames.max(pulled) as usize * 2;
        self.ring.target.store(target, Ordering::Relaxed);
        self.ring.reprime.store(true, Ordering::Relaxed);
        self.output_id = out_id;
        self.output_name = get_device_name(out_id).unwrap_or_default();
        let (out_ms, out_detail) =
            latency_ms(out_id, sys::kAudioObjectPropertyScopeOutput, out_rate);
        let (in_ms, in_detail) = latency_ms(
            self.input_id,
            sys::kAudioObjectPropertyScopeInput,
            self.in_rate,
        );
        let ring_ms = target as f64 / self.in_rate * 1000.0;
        eprintln!(
            "passthrough: in: {} [{in_detail}] {in_ms:.1} ms | ring {target} frames {ring_ms:.1} ms | out: {} [{out_detail}] {out_ms:.1} ms | total reported {:.1} ms",
            self.input_name,
            self.output_name,
            in_ms + ring_ms + out_ms
        );
    }
}

fn err(what: &'static str) -> impl Fn(coreaudio::Error) -> String {
    move |e| format!("{what}: {e:?}")
}

/// Soft limiter above -0.45 dBFS.
#[inline(always)]
fn limit(y: f32) -> f32 {
    if y > 0.95 {
        0.95 + 0.05 * ((y - 0.95) / 0.05).tanh()
    } else if y < -0.95 {
        -0.95 - 0.05 * ((-y - 0.95) / 0.05).tanh()
    } else {
        y
    }
}

fn is_alive(id: u32) -> bool {
    get::<u32>(
        id,
        sys::kAudioDevicePropertyDeviceIsAlive,
        sys::kAudioObjectPropertyScopeGlobal,
    ) == Some(1)
}

/// Calls `on_change`, on a CoreAudio thread, whenever audio devices come or go or the
/// default output changes. Registered once for the life of the app.
pub fn watch_devices(on_change: impl Fn() + Send + Sync + 'static) {
    type Callback = Box<dyn Fn() + Send + Sync>;
    unsafe extern "C" fn listener(
        _object: u32,
        _count: u32,
        _addresses: *const sys::AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> i32 {
        (*(data as *const Callback))();
        0
    }
    let data = Box::into_raw(Box::new(Box::new(on_change) as Callback)) as *mut c_void;
    for selector in [
        sys::kAudioHardwarePropertyDevices,
        sys::kAudioHardwarePropertyDefaultOutputDevice,
    ] {
        let status = unsafe {
            sys::AudioObjectAddPropertyListener(
                sys::kAudioObjectSystemObject,
                &address(selector, sys::kAudioObjectPropertyScopeGlobal),
                Some(listener),
                data,
            )
        };
        if status != 0 {
            eprintln!("passthrough: failed to watch audio devices ({status})");
        }
    }
}

/// Input devices as (id, name).
pub fn input_devices() -> Vec<(u32, String)> {
    get_audio_device_ids_for_scope(Scope::Input)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| get_device_name(id).ok().map(|name| (id, name)))
        .collect()
}

pub enum MicAccess {
    Granted,
    Denied,
    Undetermined,
}

/// Microphone permission (TCC). Capture asks for it on first use; until then it records silence.
pub fn mic_access() -> MicAccess {
    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {}
    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        return MicAccess::Granted;
    };
    let status: isize =
        unsafe { msg_send![class, authorizationStatusForMediaType: ns_string!("soun")] };
    match status {
        0 => MicAccess::Undetermined,
        3 => MicAccess::Granted,
        _ => MicAccess::Denied,
    }
}

/// Ask for the microphone now, so the prompt shows before the monitor switches away.
pub fn request_mic_access() {
    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        return;
    };
    let done = block2::RcBlock::new(|_granted: objc2::runtime::Bool| {});
    unsafe {
        let _: () = msg_send![class, requestAccessForMediaType: ns_string!("soun"), completionHandler: &*done];
    }
}

fn address(selector: u32, scope: u32) -> sys::AudioObjectPropertyAddress {
    sys::AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: 0,
    }
}

fn get<T: Copy>(id: u32, selector: u32, scope: u32) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe {
        sys::AudioObjectGetPropertyData(
            id,
            &address(selector, scope),
            0,
            ptr::null(),
            &mut size,
            value.as_mut_ptr() as *mut c_void,
        )
    };
    (status == 0).then(|| unsafe { value.assume_init() })
}

fn set_buffer_frames(id: u32, scope: u32) -> u32 {
    let min = get::<sys::AudioValueRange>(id, sys::kAudioDevicePropertyBufferFrameSizeRange, scope)
        .map_or(0, |r| r.mMinimum as u32);
    let frames = IO_FRAMES.max(min);
    unsafe {
        sys::AudioObjectSetPropertyData(
            id,
            &address(sys::kAudioDevicePropertyBufferFrameSize, scope),
            0,
            ptr::null(),
            4,
            &frames as *const u32 as *const c_void,
        );
    }
    get::<u32>(id, sys::kAudioDevicePropertyBufferFrameSize, scope).unwrap_or(frames)
}

/// Reported latency of one side of a device, in ms, with the parts that make it up.
fn latency_ms(id: u32, scope: u32, rate: f64) -> (f64, String) {
    let frames = |selector| get::<u32>(id, selector, scope).unwrap_or(0);
    let device = frames(sys::kAudioDevicePropertyLatency);
    let safety = frames(sys::kAudioDevicePropertySafetyOffset);
    let buffer = frames(sys::kAudioDevicePropertyBufferFrameSize);
    let stream = first_stream(id, scope)
        .and_then(|s| {
            get::<u32>(
                s,
                sys::kAudioStreamPropertyLatency,
                sys::kAudioObjectPropertyScopeGlobal,
            )
        })
        .unwrap_or(0);
    let total = (device + safety + buffer + stream) as f64 / rate * 1000.0;
    (
        total,
        format!(
            "device {device} + safety {safety} + buffer {buffer} + stream {stream} frames @ {} Hz",
            rate as u32
        ),
    )
}

fn first_stream(id: u32, scope: u32) -> Option<u32> {
    let addr = address(sys::kAudioDevicePropertyStreams, scope);
    let mut size = 0u32;
    unsafe {
        if sys::AudioObjectGetPropertyDataSize(id, &addr, 0, ptr::null(), &mut size) != 0 {
            return None;
        }
        let mut streams = vec![0u32; size as usize / 4];
        if streams.is_empty()
            || sys::AudioObjectGetPropertyData(
                id,
                &addr,
                0,
                ptr::null(),
                &mut size,
                streams.as_mut_ptr() as *mut c_void,
            ) != 0
        {
            return None;
        }
        streams.first().copied()
    }
}
