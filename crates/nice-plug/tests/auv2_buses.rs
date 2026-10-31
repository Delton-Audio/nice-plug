// DELTON FORK DELTA: au-4c
//! The AUv2 wrapper's buses: aux outputs and the sidechain, driven through Apple's own host API
//! the same way `auv2_host.rs` drives it. Runs on macOS only.
#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once};

use nice_plug::prelude::*;
use objc2_audio_toolbox::{
    kAudioUnitErr_InvalidElement, kAudioUnitProperty_ElementCount, kAudioUnitProperty_ElementName,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, AURenderCallbackStruct, AudioComponent, AudioComponentDescription,
    AudioComponentFactoryFunction, AudioComponentFindNext, AudioComponentInstance,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioComponentPlugInInterface,
    AudioComponentRegister, AudioUnitGetProperty, AudioUnitInitialize, AudioUnitRender,
    AudioUnitRenderActionFlags, AudioUnitReset, AudioUnitSetProperty, AudioUnitUninitialize,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_core_foundation::{CFRetained, CFString};

const AU_EFFECT: u32 = 0x6175_6678; // 'aufx'
const SUBTYPE: u32 = 0x5462_7573; // 'Tbus'
const MANUFACTURER: u32 = 0x4E70_6C67; // 'Nplg'

// DELTON FORK DELTA: au-4c
/// Every `process` call of the test plugin. The tests hold `SERIAL` for their whole body, so only
/// one unit renders at a time and the count is exact.
static PROCESS_CALLS: AtomicU32 = AtomicU32::new(0);
// DELTON FORK DELTA: au-4c
static SERIAL: Mutex<()> = Mutex::new(());

// DELTON FORK DELTA: au-4c
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poison| poison.into_inner())
}

// DELTON FORK DELTA: au-4c
/// The input values: the main input is 0.5, the sidechain 0.25.
static MAIN_IN: f32 = 0.5;
static SIDECHAIN_IN: f32 = 0.25;

#[derive(Params)]
struct TestBusesParams {}

// DELTON FORK DELTA: au-4c
/// Stereo main, stereo sidechain, stereo aux output. The main output is `main + sidechain`; the aux
/// output is `sidechain * 2`.
struct TestBuses {
    params: Arc<TestBusesParams>,
}

impl Default for TestBuses {
    fn default() -> Self {
        Self {
            params: Arc::new(TestBusesParams {}),
        }
    }
}

impl Plugin for TestBuses {
    const NAME: &'static str = "TestBuses";
    const VENDOR: &'static str = "Nice";
    const URL: &'static str = "https://example.com";
    const EMAIL: &'static str = "info@example.com";
    const VERSION: &'static str = "1.0.0";

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[new_nonzero_u32(2)],
        aux_output_ports: &[new_nonzero_u32(2)],
        names: PortNames {
            layout: None,
            main_input: None,
            main_output: None,
            aux_inputs: &["Sidechain"],
            aux_outputs: &["Aux"],
        },
    }];

    type Editor = ();
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        PROCESS_CALLS.fetch_add(1, Ordering::SeqCst);

        let sidechain = aux.inputs[0].as_slice_immutable();
        let main = buffer.as_slice();
        for (out, side) in main.iter_mut().zip(sidechain.iter()) {
            for (sample, &extra) in out.iter_mut().zip(side.iter()) {
                *sample += extra;
            }
        }

        let aux_out = aux.outputs[0].as_slice();
        for (out, side) in aux_out.iter_mut().zip(sidechain.iter()) {
            for (sample, &extra) in out.iter_mut().zip(side.iter()) {
                *sample = extra * 2.0;
            }
        }

        ProcessStatus::Normal
    }
}

impl AuPlugin for TestBuses {
    const AU_MANUFACTURER: [u8; 4] = *b"Nplg";
    const AU_SUBTYPE: [u8; 4] = *b"Tbus";
}

nice_plug::nice_export_auv2!(@unexported TestBuses);

/// Register our factory with the AudioComponent system, once for the process.
fn ensure_registered() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let mut description = description();
        let name = CFString::from_str("Nice: TestBuses");
        // The macro's factory takes the description as a plain pointer, so the user crate needs no
        // objc2 import; it is the same function to the loader
        let factory: AudioComponentFactoryFunction = Some(std::mem::transmute::<
            unsafe extern "C-unwind" fn(*const c_void) -> *mut c_void,
            unsafe extern "C-unwind" fn(
                NonNull<AudioComponentDescription>,
            ) -> *mut AudioComponentPlugInInterface,
        >(auv2::factory));
        let component =
            AudioComponentRegister(NonNull::from(&mut description), &name, 0x0001_0000, factory);
        assert!(!component.is_null(), "AudioComponentRegister failed");
    });
}

fn description() -> AudioComponentDescription {
    AudioComponentDescription {
        componentType: AU_EFFECT,
        componentSubType: SUBTYPE,
        componentManufacturer: MANUFACTURER,
        componentFlags: 0,
        componentFlagsMask: 0,
    }
}

/// An instance of the test plugin, disposed on drop.
struct Unit(AudioComponentInstance);

impl Unit {
    fn new() -> Self {
        ensure_registered();
        unsafe {
            let mut description = description();
            let component: AudioComponent =
                AudioComponentFindNext(ptr::null_mut(), NonNull::from(&mut description));
            assert!(
                !component.is_null(),
                "the registered component was not found"
            );

            let mut instance: AudioComponentInstance = ptr::null_mut();
            let status = AudioComponentInstanceNew(component, NonNull::from(&mut instance));
            assert_eq!(status, 0, "AudioComponentInstanceNew failed");
            assert!(!instance.is_null());
            Unit(instance)
        }
    }

    fn set<T>(&self, id: u32, scope: u32, element: u32, value: &T) -> i32 {
        unsafe {
            AudioUnitSetProperty(
                self.0,
                id,
                scope,
                element,
                value as *const T as *const c_void,
                size_of::<T>() as u32,
            )
        }
    }

    /// Get a property whose all-zero bit pattern is a valid value.
    fn get<T: Copy>(&self, id: u32, scope: u32, element: u32) -> (i32, T) {
        unsafe {
            let mut value: T = zeroed();
            let mut size = size_of::<T>() as u32;
            let status = AudioUnitGetProperty(
                self.0,
                id,
                scope,
                element,
                NonNull::from(&mut value).cast(),
                NonNull::from(&mut size),
            );
            (status, value)
        }
    }

    /// Two channels at 44.1 kHz on both input elements and both output elements.
    fn set_format_all(&self) {
        for (scope, element) in [
            (kAudioUnitScope_Input, 0),
            (kAudioUnitScope_Input, 1),
            (kAudioUnitScope_Output, 0),
            (kAudioUnitScope_Output, 1),
        ] {
            let status = self.set(
                kAudioUnitProperty_StreamFormat,
                scope,
                element,
                &format(2, 44100.0),
            );
            assert_eq!(
                status, 0,
                "setting the stream format failed for scope {scope} element {element}"
            );
        }
    }

    fn set_max_frames(&self, frames: u32) -> i32 {
        self.set(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &frames,
        )
    }

    /// Feed `value` to input element `element`.
    fn set_callback(&self, element: u32, value: &'static f32) {
        let callback = AURenderCallbackStruct {
            inputProc: Some(value_callback),
            inputProcRefCon: value as *const f32 as *mut c_void,
        };
        let status = self.set(
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input,
            element,
            &callback,
        );
        assert_eq!(status, 0, "setting the render callback failed");
    }

    fn initialize(&self) -> i32 {
        unsafe { AudioUnitInitialize(self.0) }
    }

    /// Render `frames` of `bus` with the given sample time.
    fn render_at(&self, frames: u32, bus: u32, sample_time: f64, list: &mut TestList) -> i32 {
        unsafe {
            let mut flags = AudioUnitRenderActionFlags(0);
            let mut timestamp: AudioTimeStamp = zeroed();
            timestamp.mFlags = AudioTimeStampFlags::SampleTimeValid;
            timestamp.mSampleTime = sample_time;
            AudioUnitRender(
                self.0,
                &mut flags,
                NonNull::from(&mut timestamp),
                bus,
                frames,
                NonNull::new_unchecked(list.as_ptr()),
            )
        }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        unsafe {
            AudioUnitUninitialize(self.0);
            AudioComponentInstanceDispose(self.0);
        }
    }
}

/// A unit with the main input at 0.5, the sidechain at 0.25 when `sidechain` is set, and 512
/// frames.
fn new_unit(sidechain: bool) -> Unit {
    let unit = Unit::new();
    unit.set_format_all();
    assert_eq!(unit.set_max_frames(512), 0);
    unit.set_callback(0, &MAIN_IN);
    if sidechain {
        unit.set_callback(1, &SIDECHAIN_IN);
    }
    assert_eq!(unit.initialize(), 0);
    unit
}

/// The canonical format: deinterleaved 32-bit float.
fn format(channels: u32, sample_rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// Fills every buffer it is given with the f32 its refcon points at.
unsafe extern "C-unwind" fn value_callback(
    refcon: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _timestamp: NonNull<AudioTimeStamp>,
    _bus: u32,
    frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    unsafe {
        let value = *refcon.cast::<f32>().as_ptr();
        let count = (*io_data).mNumberBuffers as usize;
        let first = (&raw mut (*io_data).mBuffers) as *mut AudioBuffer;
        for i in 0..count {
            let data = (*first.add(i)).mData as *mut f32;
            for frame in 0..frames as usize {
                *data.add(frame) = value;
            }
        }
    }
    0
}

/// An `AudioBufferList` of `channels` one-channel buffers, either backed by our memory or with null
/// `mData`, which asks the unit to supply the buffers.
struct TestList {
    storage: Vec<u64>,
    channels: Vec<Vec<f32>>,
    frames: u32,
}

impl TestList {
    fn new(channels: u32, frames: u32, own_buffers: bool) -> Self {
        let bytes = 8 + channels as usize * size_of::<AudioBuffer>();
        let mut list = Self {
            storage: vec![0u64; bytes.div_ceil(8)],
            channels: vec![vec![f32::NAN; frames as usize]; channels as usize],
            frames,
        };

        unsafe {
            let ptr = list.as_ptr();
            (*ptr).mNumberBuffers = channels;
            let first = (&raw mut (*ptr).mBuffers) as *mut AudioBuffer;
            for (i, channel) in list.channels.iter_mut().enumerate() {
                first.add(i).write(AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: frames * 4,
                    mData: if own_buffers {
                        channel.as_mut_ptr() as *mut c_void
                    } else {
                        ptr::null_mut()
                    },
                });
            }
        }

        list
    }

    fn as_ptr(&mut self) -> *mut AudioBufferList {
        self.storage.as_mut_ptr() as *mut AudioBufferList
    }

    /// Every sample of every buffer, wherever the unit left the buffers.
    fn samples(&mut self) -> Vec<f32> {
        let mut samples = Vec::new();
        unsafe {
            let ptr = self.as_ptr();
            let count = (*ptr).mNumberBuffers as usize;
            let first = (&raw mut (*ptr).mBuffers) as *mut AudioBuffer;
            for i in 0..count {
                let data = (*first.add(i)).mData as *const f32;
                assert!(!data.is_null(), "buffer {i} has no data after the render");
                samples.extend_from_slice(std::slice::from_raw_parts(data, self.frames as usize));
            }
        }
        samples
    }
}

/// The name of an element, read as the `CFString` the unit returns.
fn element_name(unit: &Unit, scope: u32, element: u32) -> String {
    let (status, name) = unit.get::<*const c_void>(kAudioUnitProperty_ElementName, scope, element);
    assert_eq!(status, 0, "reading the element name failed");
    let name = unsafe { CFRetained::from_raw(NonNull::new(name as *mut CFString).unwrap()) };
    name.to_string()
}

// DELTON FORK DELTA: au-4c
#[test]
fn elements_and_names() {
    let _serial = serial();
    let unit = new_unit(true);

    let (status, inputs) =
        unit.get::<u32>(kAudioUnitProperty_ElementCount, kAudioUnitScope_Input, 0);
    assert_eq!(status, 0);
    assert_eq!(inputs, 2);
    let (status, outputs) =
        unit.get::<u32>(kAudioUnitProperty_ElementCount, kAudioUnitScope_Output, 0);
    assert_eq!(status, 0);
    assert_eq!(outputs, 2);

    assert_eq!(element_name(&unit, kAudioUnitScope_Input, 1), "Sidechain");
    assert_eq!(element_name(&unit, kAudioUnitScope_Output, 1), "Aux");
}

// DELTON FORK DELTA: au-4c
#[test]
fn the_sidechain_reaches_process() {
    let _serial = serial();
    let unit = new_unit(true);

    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 0.0, &mut list), 0);
    let samples = list.samples();
    assert!(samples.iter().all(|&s| s == 0.75), "{samples:?}");
}

// DELTON FORK DELTA: au-4c
#[test]
fn an_unconnected_sidechain_is_silence() {
    let _serial = serial();
    let unit = new_unit(false);

    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 0.0, &mut list), 0);
    let samples = list.samples();
    assert!(samples.iter().all(|&s| s == 0.5), "{samples:?}");
}

// DELTON FORK DELTA: au-4c
#[test]
fn each_bus_comes_from_one_process() {
    let _serial = serial();
    let unit = new_unit(true);

    let before = PROCESS_CALLS.load(Ordering::SeqCst);
    let mut main = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 0.0, &mut main), 0);
    let samples = main.samples();
    assert!(samples.iter().all(|&s| s == 0.75), "{samples:?}");

    let mut aux = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 1, 0.0, &mut aux), 0);
    let samples = aux.samples();
    assert!(samples.iter().all(|&s| s == 0.5), "{samples:?}");

    assert_eq!(PROCESS_CALLS.load(Ordering::SeqCst) - before, 1);
}

// DELTON FORK DELTA: au-4c
#[test]
fn a_new_sample_time_processes_again() {
    let _serial = serial();
    let unit = new_unit(true);

    let before = PROCESS_CALLS.load(Ordering::SeqCst);
    let mut aux = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 1, 512.0, &mut aux), 0);
    let samples = aux.samples();
    assert!(samples.iter().all(|&s| s == 0.5), "{samples:?}");

    let mut main = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 512.0, &mut main), 0);
    let samples = main.samples();
    assert!(samples.iter().all(|&s| s == 0.75), "{samples:?}");
    assert_eq!(PROCESS_CALLS.load(Ordering::SeqCst) - before, 1);

    let mut main = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 1024.0, &mut main), 0);
    assert_eq!(PROCESS_CALLS.load(Ordering::SeqCst) - before, 2);
}

// DELTON FORK DELTA: au-4c
#[test]
fn a_different_frame_count_processes_again() {
    let _serial = serial();
    let unit = new_unit(true);

    let before = PROCESS_CALLS.load(Ordering::SeqCst);
    let mut main = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 0.0, &mut main), 0);

    let mut aux = TestList::new(2, 256, false);
    assert_eq!(unit.render_at(256, 1, 0.0, &mut aux), 0);
    let samples = aux.samples();
    assert_eq!(samples.len(), 512);
    assert!(samples.iter().all(|&s| s == 0.5), "{samples:?}");
    assert_eq!(PROCESS_CALLS.load(Ordering::SeqCst) - before, 2);
}

// DELTON FORK DELTA: au-4c
#[test]
fn a_missing_bus_is_refused() {
    let _serial = serial();
    let unit = new_unit(true);

    let mut list = TestList::new(2, 512, false);
    assert_eq!(
        unit.render_at(512, 2, 0.0, &mut list),
        kAudioUnitErr_InvalidElement
    );
}

// DELTON FORK DELTA: au-4c
#[test]
fn a_null_buffer_is_lent_our_storage() {
    let _serial = serial();
    let unit = new_unit(true);

    let mut main = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 0.0, &mut main), 0);

    let mut aux = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 1, 0.0, &mut aux), 0);
    let samples = aux.samples();
    assert!(samples.iter().all(|&s| s == 0.5), "{samples:?}");
}

// DELTON FORK DELTA: au-4c
#[test]
fn reset_forgets_the_block() {
    let _serial = serial();
    let unit = new_unit(true);

    let before = PROCESS_CALLS.load(Ordering::SeqCst);
    let mut main = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 0, 0.0, &mut main), 0);
    assert_eq!(PROCESS_CALLS.load(Ordering::SeqCst) - before, 1);

    assert_eq!(
        unsafe { AudioUnitReset(unit.0, kAudioUnitScope_Global, 0) },
        0
    );

    let mut aux = TestList::new(2, 512, false);
    assert_eq!(unit.render_at(512, 1, 0.0, &mut aux), 0);
    assert_eq!(PROCESS_CALLS.load(Ordering::SeqCst) - before, 2);
}
