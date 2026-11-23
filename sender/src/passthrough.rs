//! Low-latency audio passthrough: a local input (the dock's "Microphone In", fed by the
//! monitor's audio out) played to the default output. Two HAL units joined by a lock-free
//! ring that trims clock drift, ported from the PS5 Loop app it replaces.
//!
//! The render callbacks are plain C functions over state this module owns: nothing in them
//! allocates or locks, the input buffer has a fixed size, and the state is freed only after
//! both units have been stopped and disposed, also when setup fails halfway.

use coreaudio::audio_unit::macos_helpers::{
    get_audio_device_ids, get_default_device_id, get_device_name,
};
use coreaudio::sys;
use objc2::msg_send;
use objc2::runtime::AnyClass;
use objc2_foundation::ns_string;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

const RING_FRAMES: usize = 16384;
/// Largest input callback served. A bigger one is skipped rather than allocating on the
/// audio thread; devices deliver far less (64 to 1024 frames).
const MAX_FRAMES: usize = 4096;
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

/// An output-type audio unit, stopped and disposed when dropped.
struct Unit(sys::AudioUnit);

impl Unit {
    fn new(sub_type: u32) -> Result<Unit, String> {
        let desc = sys::AudioComponentDescription {
            componentType: sys::kAudioUnitType_Output as u32,
            componentSubType: sub_type,
            componentManufacturer: sys::kAudioUnitManufacturer_Apple as u32,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        unsafe {
            let component = sys::AudioComponentFindNext(ptr::null_mut(), &desc);
            if component.is_null() {
                return Err("no audio output component".into());
            }
            let mut unit: sys::AudioUnit = ptr::null_mut();
            check(
                sys::AudioComponentInstanceNew(component, &mut unit),
                "audio unit",
            )?;
            Ok(Unit(unit))
        }
    }

    fn set<T>(
        &self,
        property: u32,
        scope: u32,
        element: u32,
        value: &T,
        what: &str,
    ) -> Result<(), String> {
        check(
            unsafe {
                sys::AudioUnitSetProperty(
                    self.0,
                    property,
                    scope,
                    element,
                    value as *const T as *const c_void,
                    std::mem::size_of::<T>() as u32,
                )
            },
            what,
        )
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        unsafe {
            sys::AudioOutputUnitStop(self.0);
            sys::AudioUnitUninitialize(self.0);
            sys::AudioComponentInstanceDispose(self.0);
        }
    }
}

fn check(status: sys::OSStatus, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    }
}

struct InputState {
    unit: sys::AudioUnit,
    buffer: Box<[f32]>,
    ring: Arc<Ring>,
    gain: f32,
}

struct OutputState {
    ring: Arc<Ring>,
    primed: bool,
}

/// The callbacks' state. Freed on drop, which must come after both units are gone.
struct Callbacks {
    input: *mut InputState,
    output: *mut OutputState,
}

impl Callbacks {
    fn new(ring: &Arc<Ring>, gain: f32) -> Self {
        Callbacks {
            input: Box::into_raw(Box::new(InputState {
                unit: ptr::null_mut(),
                buffer: vec![0.0; MAX_FRAMES * 2].into_boxed_slice(),
                ring: ring.clone(),
                gain,
            })),
            output: Box::into_raw(Box::new(OutputState {
                ring: ring.clone(),
                primed: false,
            })),
        }
    }
}

impl Drop for Callbacks {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.input));
            drop(Box::from_raw(self.output));
        }
    }
}

unsafe extern "C" fn input_proc(
    refcon: *mut c_void,
    flags: *mut sys::AudioUnitRenderActionFlags,
    time: *const sys::AudioTimeStamp,
    bus: u32,
    frames: u32,
    _data: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    let state = &mut *(refcon as *mut InputState);
    let n = frames as usize;
    if n > MAX_FRAMES {
        return -50; // paramErr: skip this block
    }
    let mut list = sys::AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [sys::AudioBuffer {
            mNumberChannels: 2,
            mDataByteSize: (n * 8) as u32,
            mData: state.buffer.as_mut_ptr() as *mut c_void,
        }],
    };
    let status = sys::AudioUnitRender(state.unit, flags, time, bus, frames, &mut list);
    if status != 0 {
        return status;
    }
    let ring = &state.ring;
    let mut w = ring.write.load(Ordering::Relaxed);
    let mut peak = 0f32;
    for frame in state.buffer[..n * 2].chunks_exact(2) {
        let (l, r) = (frame[0], frame[1]);
        peak = peak.max(l.abs()).max(r.abs());
        let p = (w % RING_FRAMES) * 2;
        ring.samples[p].store(limit(l * state.gain).to_bits(), Ordering::Relaxed);
        ring.samples[p + 1].store(limit(r * state.gain).to_bits(), Ordering::Relaxed);
        w += 1;
    }
    ring.write.store(w, Ordering::Release);
    ring.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
    0
}

unsafe extern "C" fn output_proc(
    refcon: *mut c_void,
    _flags: *mut sys::AudioUnitRenderActionFlags,
    _time: *const sys::AudioTimeStamp,
    _bus: u32,
    frames: u32,
    data: *mut sys::AudioBufferList,
) -> sys::OSStatus {
    let state = &mut *(refcon as *mut OutputState);
    if data.is_null() || (*data).mNumberBuffers == 0 {
        return 0;
    }
    let buffer = &mut (*data).mBuffers[0];
    let channels = (buffer.mNumberChannels as usize).max(1);
    let n = (frames as usize).min(buffer.mDataByteSize as usize / (4 * channels));
    let out = std::slice::from_raw_parts_mut(buffer.mData as *mut f32, n * channels);
    let ring = &state.ring;
    if ring.reprime.swap(false, Ordering::Relaxed) {
        state.primed = false;
    }
    let w = ring.write.load(Ordering::Acquire);
    let mut r = ring.read.load(Ordering::Relaxed);
    let target = ring.target.load(Ordering::Relaxed);
    let mut avail = w.wrapping_sub(r);
    if !state.primed {
        if avail < target + n {
            out.fill(0.0);
            return 0;
        }
        state.primed = true;
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
        state.primed = false;
    }
    ring.read.store(r, Ordering::Release);
    0
}

fn stereo_float(rate: f64) -> sys::AudioStreamBasicDescription {
    sys::AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: sys::kAudioFormatLinearPCM as u32,
        mFormatFlags: (sys::kAudioFormatFlagIsFloat | sys::kAudioFormatFlagIsPacked) as u32,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

pub struct Passthrough {
    // Field order matters: the units stop and go before the callback state they point at.
    input_unit: Unit,
    output_unit: Unit,
    _callbacks: Callbacks,
    input_name: String,
    input_id: u32,
    in_rate: f64,
    in_frames: u32,
    in_latency_ms: f64,
    output_id: u32,
    output_name: String,
    ring: Arc<Ring>,
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
        let format = stereo_float(in_rate);
        let ring = Arc::new(Ring {
            samples: (0..RING_FRAMES * 2).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            target: AtomicUsize::new(256),
            reprime: AtomicBool::new(false),
            peak: AtomicU32::new(0),
        });

        // Declared before the units, so on any early return the units are dropped first.
        let callbacks = Callbacks::new(&ring, 10f32.powf(GAIN_DB / 20.0));
        let (global, input_scope, output_scope) = (
            sys::kAudioUnitScope_Global as u32,
            sys::kAudioUnitScope_Input as u32,
            sys::kAudioUnitScope_Output as u32,
        );

        let input = Unit::new(sys::kAudioUnitSubType_HALOutput as u32)?;
        let (on, off) = (1u32, 0u32);
        let enable_io = sys::kAudioOutputUnitProperty_EnableIO as u32;
        input.set(enable_io, input_scope, 1, &on, "enable input")?;
        input.set(enable_io, output_scope, 0, &off, "disable output")?;
        input.set(
            sys::kAudioOutputUnitProperty_CurrentDevice as u32,
            global,
            0,
            &input_id,
            "input device",
        )?;
        let stream_format = sys::kAudioUnitProperty_StreamFormat as u32;
        input.set(stream_format, output_scope, 1, &format, "input format")?;
        unsafe { (*callbacks.input).unit = input.0 };
        let input_callback = sys::AURenderCallbackStruct {
            inputProc: Some(input_proc),
            inputProcRefCon: callbacks.input as *mut c_void,
        };
        input.set(
            sys::kAudioOutputUnitProperty_SetInputCallback as u32,
            global,
            0,
            &input_callback,
            "input callback",
        )?;

        // DefaultOutput follows the system default device and converts the sample rate.
        let output = Unit::new(sys::kAudioUnitSubType_DefaultOutput as u32)?;
        output.set(stream_format, input_scope, 0, &format, "output format")?;
        let output_callback = sys::AURenderCallbackStruct {
            inputProc: Some(output_proc),
            inputProcRefCon: callbacks.output as *mut c_void,
        };
        output.set(
            sys::kAudioUnitProperty_SetRenderCallback as u32,
            input_scope,
            0,
            &output_callback,
            "output callback",
        )?;
        check(unsafe { sys::AudioUnitInitialize(input.0) }, "input init")?;
        check(unsafe { sys::AudioUnitInitialize(output.0) }, "output init")?;

        let mut this = Self {
            input_unit: input,
            output_unit: output,
            _callbacks: callbacks,
            input_name: input_name.to_string(),
            input_id,
            in_rate,
            in_frames,
            in_latency_ms: latency_ms(input_id, sys::kAudioObjectPropertyScopeInput, in_rate).0,
            output_id: 0,
            output_name: String::new(),
            ring,
        };
        this.apply_output();
        check(
            unsafe { sys::AudioOutputUnitStart(this.input_unit.0) },
            "input start",
        )?;
        check(
            unsafe { sys::AudioOutputUnitStart(this.output_unit.0) },
            "output start",
        )?;
        Ok(this)
    }

    /// Frames the input has delivered so far; a watchdog checks that it moves.
    pub fn frames_in(&self) -> usize {
        self.ring.write.load(Ordering::Relaxed)
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

/// Devices with at least one input stream, as (id, name), one per name (the first wins, as
/// in `Passthrough::start`). Output-only devices such as a dock's "Headphone" are left out.
pub fn input_devices() -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    for id in get_audio_device_ids().unwrap_or_default() {
        if first_stream(id, sys::kAudioObjectPropertyScopeInput).is_none() {
            continue;
        }
        if let Ok(name) = get_device_name(id) {
            if !out.iter().any(|(_, n)| *n == name) {
                out.push((id, name));
            }
        }
    }
    out
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
