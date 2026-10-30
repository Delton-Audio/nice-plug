// DELTON FORK DELTA: au-1, AUv2 wrapper
//! Drives the AUv2 wrapper through Apple's own host API: `AudioComponentRegister` puts our factory
//! in this process, and `AudioComponentInstanceNew`, `AudioUnitInitialize`, `AudioUnitRender` and
//! the rest talk to it exactly as a DAW would. Runs on macOS only.
#![cfg(target_os = "macos")]
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Once};

use nice_plug::prelude::*;
use objc2_audio_toolbox::{
    kAudioUnitErr_FormatNotSupported, kAudioUnitErr_Initialized, kAudioUnitErr_InvalidElement,
    kAudioUnitErr_InvalidPropertyValue, kAudioUnitErr_TooManyFramesToProcess,
    kAudioUnitErr_Uninitialized, kAudioUnitProperty_ClassInfo, kAudioUnitProperty_Latency,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_ParameterInfo,
    kAudioUnitProperty_ParameterList, kAudioUnitProperty_ParameterStringFromValue,
    kAudioUnitProperty_ParameterValueStrings, kAudioUnitProperty_SampleRate,
    kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_StreamFormat,
    kAudioUnitProperty_TailTime, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, AURenderCallbackStruct, AudioComponent, AudioComponentDescription,
    AudioComponentFactoryFunction, AudioComponentFindNext, AudioComponentInstance,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioComponentPlugInInterface,
    AudioComponentRegister, AudioUnitAddRenderNotify, AudioUnitGetParameter, AudioUnitGetProperty,
    AudioUnitGetPropertyInfo, AudioUnitInitialize, AudioUnitParameterInfo,
    AudioUnitParameterOptions, AudioUnitParameterStringFromValue, AudioUnitParameterUnit,
    AudioUnitRemoveRenderNotify, AudioUnitRender, AudioUnitRenderActionFlags,
    AudioUnitSetParameter, AudioUnitSetProperty, AudioUnitUninitialize,
};
// DELTON FORK DELTA: au-2
use objc2_audio_toolbox::{kAudioUnitErr_InvalidProperty, kAudioUnitProperty_CocoaUI};
// DELTON FORK DELTA: au-3
use objc2_audio_toolbox::{AudioUnit, AudioUnitAddPropertyListener};
// DELTON FORK DELTA: au-4a
use objc2_audio_toolbox::{
    kAudioUnitErr_PropertyNotWritable, kAudioUnitProperty_RenderContextObserver,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_core_foundation::{CFArray, CFMutableDictionary, CFNumber, CFRetained, CFString, CFType};

const AU_EFFECT: u32 = 0x6175_6678; // 'aufx'
const SUBTYPE: u32 = 0x5467_616E; // 'Tgan'
const MANUFACTURER: u32 = 0x4E70_6C67; // 'Nplg'

#[derive(Params)]
struct TestGainParams {
    #[id = "gain"]
    gain: FloatParam,
    #[id = "mute"]
    mute: BoolParam,
    #[id = "mode"]
    mode: IntParam,
    // DELTON FORK DELTA: au-3, an enum is the AU menu; an integer no longer is
    #[id = "shape"]
    shape: EnumParam<Shape>,
}

#[derive(Enum, PartialEq)]
enum Shape {
    Sine,
    Square,
    Saw,
}

impl Default for TestGainParams {
    fn default() -> Self {
        Self {
            gain: FloatParam::new("Gain", 1.0, FloatRange::Linear { min: 0.0, max: 2.0 }),
            mute: BoolParam::new("Mute", false),
            mode: IntParam::new("Mode", 0, IntRange::Linear { min: 0, max: 3 }),
            shape: EnumParam::new("Shape", Shape::Sine),
        }
    }
}

// DELTON FORK DELTA: au-4a
/// `mode` at this value marks the unit of the workgroup test, the only one whose `process`
/// records (no other test sets it; libtest runs the rendering tests in parallel).
const RECORD_WORKGROUP_MODE: i32 = 3;
// DELTON FORK DELTA: au-4a
static SEEN_WORKGROUP: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

struct TestGain {
    params: Arc<TestGainParams>,
}

impl Default for TestGain {
    fn default() -> Self {
        Self {
            params: Arc::new(TestGainParams::default()),
        }
    }
}

impl Plugin for TestGain {
    const NAME: &'static str = "TestGain";
    const VENDOR: &'static str = "Nice";
    const URL: &'static str = "https://example.com";
    const EMAIL: &'static str = "info@example.com";
    const VERSION: &'static str = "1.0.0";

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    type Editor = ();
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn activate(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        _buffer_config: &BufferConfig,
        context: &mut impl ActivateContext<Self>,
    ) -> bool {
        context.set_latency_samples(64);
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // DELTON FORK DELTA: au-4a
        // Only the workgroup test's unit records: libtest runs the other rendering tests in parallel
        if self.params.mode.value() == RECORD_WORKGROUP_MODE {
            SEEN_WORKGROUP.store(
                context
                    .audio_workgroup()
                    .map_or(ptr::null_mut(), |w| w.as_ptr()),
                Ordering::SeqCst,
            );
        }

        // The plain, unsmoothed values: the test compares exact samples
        let gain = self.params.gain.value();
        let mute = self.params.mute.value();
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                *sample = if mute { 0.0 } else { *sample * gain };
            }
        }

        ProcessStatus::Tail(4410)
    }
}

impl AuPlugin for TestGain {
    const AU_MANUFACTURER: [u8; 4] = *b"Nplg";
    const AU_SUBTYPE: [u8; 4] = *b"Tgan";
}

nice_plug::nice_export_auv2!(@unexported TestGain);

/// JUCE's `String::hashCode` with the top bit cleared, which is what the wrapper uses as an AU
/// parameter ID.
fn param_id(id: &str) -> u32 {
    id.bytes().fold(0u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as u32)
    }) & !(1 << 31)
}

/// Register our factory with the AudioComponent system, once for the process.
fn ensure_registered() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let mut description = description();
        let name = CFString::from_str("Nice: TestGain");
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

    fn set_format(&self, channels: u32, sample_rate: f64) {
        for scope in [kAudioUnitScope_Input, kAudioUnitScope_Output] {
            let status = self.set(
                kAudioUnitProperty_StreamFormat,
                scope,
                0,
                &format(channels, sample_rate),
            );
            assert_eq!(status, 0, "setting the stream format failed");
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

    fn set_input_callback(&self) {
        let callback = AURenderCallbackStruct {
            inputProc: Some(input_callback),
            inputProcRefCon: &INPUT_REFCON as *const u8 as *mut c_void,
        };
        let status = self.set(
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input,
            0,
            &callback,
        );
        assert_eq!(status, 0, "setting the render callback failed");
    }

    fn initialize(&self) -> i32 {
        unsafe { AudioUnitInitialize(self.0) }
    }

    fn set_parameter(&self, id: &str, value: f32) {
        let status = unsafe {
            AudioUnitSetParameter(self.0, param_id(id), kAudioUnitScope_Global, 0, value, 0)
        };
        assert_eq!(status, 0, "AudioUnitSetParameter failed for {id}");
    }

    fn parameter(&self, id: &str) -> f32 {
        let mut value = 0.0f32;
        let status = unsafe {
            AudioUnitGetParameter(
                self.0,
                param_id(id),
                kAudioUnitScope_Global,
                0,
                NonNull::from(&mut value),
            )
        };
        assert_eq!(status, 0, "AudioUnitGetParameter failed for {id}");
        value
    }

    fn render(&self, frames: u32, bus: u32, list: &mut TestList) -> i32 {
        unsafe {
            let mut flags = AudioUnitRenderActionFlags(0);
            let mut timestamp: AudioTimeStamp = zeroed();
            timestamp.mFlags = AudioTimeStampFlags::SampleTimeValid;
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

static INPUT_REFCON: u8 = 0;

/// The input every test feeds: 0.5 in every sample of every channel.
unsafe extern "C-unwind" fn input_callback(
    _refcon: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _timestamp: NonNull<AudioTimeStamp>,
    _bus: u32,
    frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    unsafe {
        let count = (*io_data).mNumberBuffers as usize;
        let first = (&raw mut (*io_data).mBuffers) as *mut AudioBuffer;
        for i in 0..count {
            let data = (*first.add(i)).mData as *mut f32;
            for frame in 0..frames as usize {
                *data.add(frame) = 0.5;
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

#[test]
fn renders_gain() {
    let unit = Unit::new();
    unit.set_format(2, 44100.0);
    assert_eq!(unit.set_max_frames(512), 0);
    unit.set_input_callback();
    assert_eq!(unit.initialize(), 0);

    // The unit supplies the buffers
    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render(512, 0, &mut list), 0);
    let samples = list.samples();
    assert_eq!(samples.len(), 1024);
    assert!(samples.iter().all(|&sample| sample == 0.5), "{samples:?}");

    // 0.25 normalized is 0.5 plain, and 0.5 * 0.5 is 0.25
    unit.set_parameter("gain", 0.25);
    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render(512, 0, &mut list), 0);
    assert!(list.samples().iter().all(|&sample| sample == 0.25));

    // Our own buffers give the same result
    let mut list = TestList::new(2, 512, true);
    assert_eq!(unit.render(512, 0, &mut list), 0);
    assert!(list.samples().iter().all(|&sample| sample == 0.25));
}

#[test]
fn parameter_info() {
    let unit = Unit::new();

    let gain_id = param_id("gain");
    let mute_id = param_id("mute");
    let mode_id = param_id("mode");
    let shape_id = param_id("shape");
    assert_eq!(gain_id, 3165055);

    // ParameterList: four ids
    let mut size = 0u32;
    let mut writable = 0u8;
    let status = unsafe {
        AudioUnitGetPropertyInfo(
            unit.0,
            kAudioUnitProperty_ParameterList,
            kAudioUnitScope_Global,
            0,
            &mut size,
            &mut writable,
        )
    };
    assert_eq!(status, 0);
    assert_eq!(size, 16);
    let mut ids = [0u32; 4];
    let mut io_size = size;
    let status = unsafe {
        AudioUnitGetProperty(
            unit.0,
            kAudioUnitProperty_ParameterList,
            kAudioUnitScope_Global,
            0,
            NonNull::new_unchecked(ids.as_mut_ptr().cast()),
            NonNull::from(&mut io_size),
        )
    };
    assert_eq!(status, 0);
    for id in [gain_id, mute_id, mode_id, shape_id] {
        assert!(ids.contains(&id), "{id} missing from {ids:?}");
    }

    let info = |id: u32| -> AudioUnitParameterInfo {
        let (status, info) = unit.get::<AudioUnitParameterInfo>(
            kAudioUnitProperty_ParameterInfo,
            kAudioUnitScope_Global,
            id,
        );
        assert_eq!(status, 0);
        info
    };

    let gain = info(gain_id);
    assert_eq!(gain.unit, AudioUnitParameterUnit::Generic);
    assert_eq!(gain.minValue, 0.0);
    assert_eq!(gain.maxValue, 1.0);
    assert_eq!(gain.defaultValue, 0.5);
    assert!(gain
        .flags
        .contains(AudioUnitParameterOptions::Flag_ValuesHaveStrings));
    assert!(gain
        .flags
        .contains(AudioUnitParameterOptions::Flag_HasCFNameString));

    // An integer is a stepped generic value with one string at a time, never a list
    let mode = info(mode_id);
    assert_eq!(mode.unit, AudioUnitParameterUnit::Generic);
    assert_eq!(mode.maxValue, 3.0);
    assert!(mode
        .flags
        .contains(AudioUnitParameterOptions::Flag_ValuesHaveStrings));
    let (status, _) = unit.get::<*const c_void>(
        kAudioUnitProperty_ParameterValueStrings,
        kAudioUnitScope_Global,
        mode_id,
    );
    assert_eq!(status, kAudioUnitErr_InvalidProperty);

    let shape = info(shape_id);
    assert_eq!(shape.unit, AudioUnitParameterUnit::Indexed);
    assert_eq!(shape.maxValue, 2.0);

    let mute = info(mute_id);
    assert_eq!(mute.unit, AudioUnitParameterUnit::Boolean);

    // The names of an enum's values: the host's menu
    let (status, strings) = unit.get::<*const c_void>(
        kAudioUnitProperty_ParameterValueStrings,
        kAudioUnitScope_Global,
        shape_id,
    );
    assert_eq!(status, 0);
    let strings = unsafe { CFRetained::from_raw(NonNull::new(strings as *mut CFArray).unwrap()) };
    assert_eq!(strings.len(), 3);

    // The host shows our text for a continuous value
    let mut value = 0.25f32;
    let request = AudioUnitParameterStringFromValue {
        inParamID: gain_id,
        inValue: NonNull::from(&mut value),
        outString: ptr::null(),
    };
    let (status, request) = unit.get_in_out(
        kAudioUnitProperty_ParameterStringFromValue,
        kAudioUnitScope_Global,
        0,
        request,
    );
    assert_eq!(status, 0);
    let text =
        unsafe { CFRetained::from_raw(NonNull::new(request.outString as *mut CFString).unwrap()) };
    let expected = TestGain::default()
        .params
        .gain
        .normalized_value_to_string(0.25, true);
    assert_eq!(text.to_string(), expected);
}

impl Unit {
    /// Get a property whose value is also an input.
    fn get_in_out<T: Copy>(&self, id: u32, scope: u32, element: u32, mut value: T) -> (i32, T) {
        unsafe {
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
}

#[test]
fn latency_and_tail() {
    let unit = Unit::new();
    unit.set_format(2, 44100.0);
    assert_eq!(unit.set_max_frames(512), 0);
    unit.set_input_callback();
    assert_eq!(unit.initialize(), 0);

    let (status, latency) = unit.get::<f64>(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
    assert_eq!(status, 0);
    assert!((latency - 64.0 / 44100.0).abs() < 1e-12, "{latency}");

    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render(512, 0, &mut list), 0);
    let (status, tail) = unit.get::<f64>(kAudioUnitProperty_TailTime, kAudioUnitScope_Global, 0);
    assert_eq!(status, 0);
    assert!((tail - 0.1).abs() < 1e-12, "{tail}");

    // A new sample rate needs an uninitialized unit
    assert_eq!(unsafe { AudioUnitUninitialize(unit.0) }, 0);
    let rate = 96000.0f64;
    assert_eq!(
        unit.set(
            kAudioUnitProperty_SampleRate,
            kAudioUnitScope_Output,
            0,
            &rate
        ),
        0
    );
    assert_eq!(unit.initialize(), 0);
    let (status, latency) = unit.get::<f64>(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
    assert_eq!(status, 0);
    assert!((latency - 64.0 / 96000.0).abs() < 1e-12, "{latency}");
}

#[test]
fn render_errors() {
    let unit = Unit::new();
    unit.set_format(2, 44100.0);
    assert_eq!(unit.set_max_frames(512), 0);
    unit.set_input_callback();

    // Before Initialize
    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render(512, 0, &mut list), kAudioUnitErr_Uninitialized);

    assert_eq!(unit.initialize(), 0);

    let mut list = TestList::new(2, 513, false);
    assert_eq!(
        unit.render(513, 0, &mut list),
        kAudioUnitErr_TooManyFramesToProcess
    );
    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render(512, 1, &mut list), kAudioUnitErr_InvalidElement);

    // While initialized the formats are frozen
    assert_eq!(
        unit.set(
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            0,
            &format(2, 44100.0)
        ),
        kAudioUnitErr_Initialized
    );

    assert_eq!(unsafe { AudioUnitUninitialize(unit.0) }, 0);

    // Three channels is not a layout the plugin offers
    assert_eq!(
        unit.set(
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            0,
            &format(3, 44100.0)
        ),
        kAudioUnitErr_FormatNotSupported
    );
    assert_eq!(unit.set_max_frames(0), kAudioUnitErr_InvalidPropertyValue);
}

#[test]
fn class_info_round_trip() {
    let unit = Unit::new();
    unit.set_parameter("gain", 0.25);
    unit.set_parameter("mute", 1.0);
    unit.set_parameter("mode", 2.0);

    let (status, class_info) =
        unit.get::<*const c_void>(kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0);
    assert_eq!(status, 0);
    assert!(!class_info.is_null());
    // The caller owns a reference
    let class_info =
        unsafe { CFRetained::from_raw(NonNull::new(class_info as *mut CFType).unwrap()) };

    unit.set_parameter("gain", 1.0);
    unit.set_parameter("mute", 0.0);
    unit.set_parameter("mode", 0.0);

    let plist: *const c_void = CFRetained::as_ptr(&class_info).as_ptr() as *const c_void;
    assert_eq!(
        unit.set(
            kAudioUnitProperty_ClassInfo,
            kAudioUnitScope_Global,
            0,
            &plist
        ),
        0
    );
    assert_eq!(unit.parameter("gain"), 0.25);
    assert_eq!(unit.parameter("mute"), 1.0);
    assert_eq!(unit.parameter("mode"), 2.0);

    // State from another plugin is refused and changes nothing
    unit.set_parameter("gain", 0.75);
    let foreign = CFMutableDictionary::<CFString, CFType>::empty();
    foreign.set(
        &CFString::from_static_str("subtype"),
        &CFNumber::new_i32(0x5858_5858),
    );
    let foreign_ptr: *const c_void = CFRetained::as_ptr(&foreign).as_ptr() as *const c_void;
    assert_eq!(
        unit.set(
            kAudioUnitProperty_ClassInfo,
            kAudioUnitScope_Global,
            0,
            &foreign_ptr
        ),
        kAudioUnitErr_InvalidPropertyValue
    );
    assert_eq!(unit.parameter("gain"), 0.75);
    assert_eq!(unit.parameter("mute"), 1.0);
    assert_eq!(unit.parameter("mode"), 2.0);
}

#[test]
fn mono_layout() {
    let unit = Unit::new();
    unit.set_format(1, 44100.0);
    assert_eq!(unit.set_max_frames(256), 0);
    unit.set_input_callback();
    assert_eq!(unit.initialize(), 0);

    let mut list = TestList::new(1, 256, false);
    assert_eq!(unit.render(256, 0, &mut list), 0);
    let samples = list.samples();
    assert_eq!(samples.len(), 256);
    assert!(samples.iter().all(|&sample| sample == 0.5), "{samples:?}");
}

// DELTON FORK DELTA: au-3, auval fails a unit that does not report a new MaximumFramesPerSlice
static MAX_FRAMES_CHANGES: AtomicU32 = AtomicU32::new(0);

/// Counts MaximumFramesPerSlice notifications on the global element; never panics (it is called
/// from inside AudioToolbox).
unsafe extern "C-unwind" fn property_changed(
    refcon: NonNull<c_void>,
    _unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
) {
    if (id, scope, element)
        == (
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
        )
    {
        unsafe { &*(refcon.as_ptr() as *const AtomicU32) }.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn max_frames_change_notifies_listeners() {
    let unit = Unit::new();
    let refcon = &MAX_FRAMES_CHANGES as *const AtomicU32 as *mut c_void;
    let status = unsafe {
        AudioUnitAddPropertyListener(
            unit.0,
            kAudioUnitProperty_MaximumFramesPerSlice,
            Some(property_changed),
            refcon,
        )
    };
    assert_eq!(status, 0);

    assert_eq!(unit.set_max_frames(1024), 0);
    assert_eq!(MAX_FRAMES_CHANGES.load(Ordering::SeqCst), 1);
    // The same value again is not a change
    assert_eq!(unit.set_max_frames(1024), 0);
    assert_eq!(MAX_FRAMES_CHANGES.load(Ordering::SeqCst), 1);
    assert_eq!(unit.set_max_frames(256), 0);
    assert_eq!(MAX_FRAMES_CHANGES.load(Ordering::SeqCst), 2);
}

static NOTIFY_COUNT: AtomicU32 = AtomicU32::new(0);

unsafe extern "C-unwind" fn notify(
    refcon: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _timestamp: NonNull<AudioTimeStamp>,
    _bus: u32,
    _frames: u32,
    _io_data: *mut AudioBufferList,
) -> i32 {
    unsafe { &*(refcon.as_ptr() as *const AtomicU32) }.fetch_add(1, Ordering::SeqCst);
    0
}

#[test]
fn render_notify() {
    let unit = Unit::new();
    unit.set_format(2, 44100.0);
    assert_eq!(unit.set_max_frames(512), 0);
    unit.set_input_callback();
    assert_eq!(unit.initialize(), 0);

    let refcon = &NOTIFY_COUNT as *const AtomicU32 as *mut c_void;
    assert_eq!(
        unsafe { AudioUnitAddRenderNotify(unit.0, Some(notify), refcon) },
        0
    );

    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render(512, 0, &mut list), 0);
    // Once before the render and once after
    assert_eq!(NOTIFY_COUNT.load(Ordering::SeqCst), 2);

    assert_eq!(
        unsafe { AudioUnitRemoveRenderNotify(unit.0, Some(notify), refcon) },
        0
    );
    let mut list = TestList::new(2, 512, false);
    assert_eq!(unit.render(512, 0, &mut list), 0);
    assert_eq!(NOTIFY_COUNT.load(Ordering::SeqCst), 2);
}

// DELTON FORK DELTA: au-4a
// os_workgroup_interval_create (macOS 11): a process-made workgroup to hand the observer
unsafe extern "C" {
    fn os_workgroup_interval_create(
        name: *const std::ffi::c_char,
        clock: u32,
        attr: *const c_void,
    ) -> *mut c_void;
    fn os_release(object: *mut c_void);
}
const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;

// DELTON FORK DELTA: au-4a
#[repr(C)]
struct RenderContext {
    workgroup: *mut c_void,
    reserved: [u32; 6],
}

// DELTON FORK DELTA: au-4a
#[test]
fn render_context_observer_hands_the_workgroup_to_process() {
    let unit = Unit::new();
    unit.set_parameter("mode", RECORD_WORKGROUP_MODE as f32);
    unit.set_format(2, 44100.0);
    assert_eq!(unit.set_max_frames(512), 0);
    unit.set_input_callback();
    assert_eq!(unit.initialize(), 0);

    let mut size = 0u32;
    let mut writable = 1u8;
    let status = unsafe {
        AudioUnitGetPropertyInfo(
            unit.0,
            kAudioUnitProperty_RenderContextObserver,
            kAudioUnitScope_Global,
            0,
            &mut size,
            &mut writable,
        )
    };
    assert_eq!(status, 0);
    assert_eq!(size as usize, size_of::<*const c_void>());
    assert_eq!(writable, 0);

    let (status, block) = unit.get::<*const c_void>(
        kAudioUnitProperty_RenderContextObserver,
        kAudioUnitScope_Global,
        0,
    );
    assert_eq!(status, 0);
    assert!(!block.is_null());
    // SAFETY: The property returns a pointer to a live block, which lives as long as the unit. Its
    // argument is a pointer to the render context.
    let block = unsafe { &*(block as *const block2::Block<dyn Fn(*const c_void)>) };

    let wg = unsafe {
        os_workgroup_interval_create(
            c"au-4a test".as_ptr(),
            OS_CLOCK_MACH_ABSOLUTE_TIME,
            ptr::null(),
        )
    };
    assert!(!wg.is_null());

    // Render one block and return the workgroup `process` saw
    let render = || {
        let mut list = TestList::new(2, 512, false);
        assert_eq!(unit.render(512, 0, &mut list), 0);
        SEEN_WORKGROUP.load(Ordering::SeqCst)
    };

    let context = RenderContext {
        workgroup: wg,
        reserved: [0; 6],
    };
    block.call((&context as *const RenderContext as *const c_void,));
    assert_eq!(render(), wg);

    // The host switches the instance to no workgroup, by a context without one
    let empty = RenderContext {
        workgroup: ptr::null_mut(),
        reserved: [0; 6],
    };
    block.call((&empty as *const RenderContext as *const c_void,));
    assert!(render().is_null());

    // and by no context at all
    block.call((&context as *const RenderContext as *const c_void,));
    assert_eq!(render(), wg);
    block.call((ptr::null(),));
    assert!(render().is_null());

    // The observer is read-only
    let status = unit.set(
        kAudioUnitProperty_RenderContextObserver,
        kAudioUnitScope_Global,
        0,
        &block,
    );
    assert_eq!(status, kAudioUnitErr_PropertyNotWritable);

    // The wrapper releases its reference when the unit closes (nothing copied the block, so it
    // goes with the unit); this one is the test's own
    drop(unit);
    unsafe { os_release(wg) };
}

// A property listener on `Latency` is called from the wrapper's event loop on the main thread,
// which a test process does not run. That path is checked by auval and on a Mac with a DAW.

// DELTON FORK DELTA: au-2
#[test]
fn no_editor_means_no_cocoa_ui() {
    // TestGain has `Editor = ()`
    let unit = Unit::new();
    let mut size = 0u32;
    let mut writable = 0u8;
    let status = unsafe {
        AudioUnitGetPropertyInfo(
            unit.0,
            kAudioUnitProperty_CocoaUI,
            kAudioUnitScope_Global,
            0,
            &mut size,
            &mut writable,
        )
    };
    assert_eq!(status, kAudioUnitErr_InvalidProperty);
}
