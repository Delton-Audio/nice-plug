// DELTON FORK DELTA: au-4b, AUv2 wrapper
//! MIDI in and MIDI out of the AUv2 wrapper, driven through Apple's own host API like
//! `auv2_host.rs`: `MusicDeviceMIDIEvent` and `MusicDeviceSysEx` in, the host's
//! `kAudioUnitProperty_MIDIOutputCallback` out. Runs on macOS only.
#![cfg(target_os = "macos")]
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};

use nice_plug::midi::{Key, VoiceID};
use nice_plug::prelude::*;
use objc2_audio_toolbox::{
    kAudioUnitErr_PropertyNotWritable, kAudioUnitProperty_MIDIOutputCallback,
    kAudioUnitProperty_MIDIOutputCallbackInfo, kAudioUnitProperty_MaximumFramesPerSlice,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Output,
    kMusicDeviceProperty_InstrumentCount, AudioComponent, AudioComponentDescription,
    AudioComponentFactoryFunction, AudioComponentFindNext, AudioComponentInstance,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioComponentPlugInInterface,
    AudioComponentRegister, AudioUnitGetProperty, AudioUnitInitialize, AudioUnitRender,
    AudioUnitRenderActionFlags, AudioUnitReset, AudioUnitSetProperty, AudioUnitUninitialize,
    MusicDeviceMIDIEvent, MusicDeviceSysEx,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, AudioBuffer, AudioBufferList, AudioStreamBasicDescription,
    AudioTimeStamp, AudioTimeStampFlags,
};
use objc2_core_foundation::{CFArray, CFRetained, CFString};

const AU_INSTRUMENT: u32 = 0x6175_6D75; // 'aumu'
const SUBTYPE: u32 = 0x5473_796E; // 'Tsyn'
const MANUFACTURER: u32 = 0x4E70_6C67; // 'Nplg'

const KIND_NOTE_ON: u64 = 1;
const KIND_NOTE_OFF: u64 = 2;
const KIND_OTHER: u64 = 3;

/// What `process` saw: `kind << 32 | timing << 8 | key`. The tests share it, so each holds `SERIAL`.
static LOG: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
static LOG_LEN: AtomicUsize = AtomicUsize::new(0);
/// What the MIDI out callback saw: `timestamp << 16 | data[0] << 8 | data[1]`.
static OUT_LOG: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
static OUT_LEN: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Params, Default)]
struct TestSynthParams {}

#[derive(Default)]
struct TestSynth {
    params: Arc<TestSynthParams>,
}

impl Plugin for TestSynth {
    const NAME: &'static str = "TestSynth";
    const VENDOR: &'static str = "Nice";
    const URL: &'static str = "https://example.com";
    const EMAIL: &'static str = "info@example.com";
    const VERSION: &'static str = "1.0.0";

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::Basic;

    type Editor = ();
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                *sample = 0.0;
            }
        }

        // No allocation and no lock in here: the log is a static array of atomics
        while let Some(event) = context.next_event() {
            let (kind, key) = match event {
                NoteEvent::NoteOn { key, .. } => (KIND_NOTE_ON, key.number().unwrap_or(0)),
                NoteEvent::NoteOff { key, .. } => (KIND_NOTE_OFF, key.number().unwrap_or(0)),
                _ => (KIND_OTHER, 0),
            };
            let index = LOG_LEN.load(Ordering::Relaxed);
            if index < LOG.len() {
                LOG[index].store(
                    (kind << 32) | ((event.timing() as u64) << 8) | key as u64,
                    Ordering::Relaxed,
                );
                LOG_LEN.store(index + 1, Ordering::Relaxed);
            }

            if let NoteEvent::NoteOn {
                timing,
                channel,
                key,
                velocity,
                ..
            } = event
            {
                let _ = context.try_send_event(NoteEvent::NoteOn {
                    timing,
                    voice_id: VoiceID::Wildcard,
                    channel,
                    key: Key::Number(key.number().unwrap_or(0) + 12),
                    velocity,
                });
            }
        }

        ProcessStatus::Normal
    }
}

impl AuPlugin for TestSynth {
    const AU_MANUFACTURER: [u8; 4] = *b"Nplg";
    const AU_SUBTYPE: [u8; 4] = *b"Tsyn";
}

nice_plug::nice_export_auv2!(@unexported TestSynth);

/// Register our factory with the AudioComponent system, once for the process.
fn ensure_registered() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let mut description = description();
        let name = CFString::from_str("Nice: TestSynth");
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
        componentType: AU_INSTRUMENT,
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

    /// Get a property whose all-zero bit pattern is a valid value, and the size it reported.
    fn get<T: Copy>(&self, id: u32, scope: u32, element: u32) -> (i32, T, u32) {
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
            (status, value, size)
        }
    }

    /// Output format, max frames and initialize: a unit that renders `frames` at most.
    fn start(&self, frames: u32) {
        let status = self.set(
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            0,
            &format(2, 44100.0),
        );
        assert_eq!(status, 0, "setting the stream format failed");
        let status = self.set(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &frames,
        );
        assert_eq!(status, 0);
        assert_eq!(unsafe { AudioUnitInitialize(self.0) }, 0);
    }

    fn midi(&self, status: u32, data1: u32, data2: u32, offset: u32) {
        let result = unsafe { MusicDeviceMIDIEvent(self.0, status, data1, data2, offset) };
        assert_eq!(result, 0, "MusicDeviceMIDIEvent failed");
    }

    fn render(&self, frames: u32) -> i32 {
        let mut list = TestList::new(2, frames);
        unsafe {
            let mut flags = AudioUnitRenderActionFlags(0);
            let mut timestamp: AudioTimeStamp = zeroed();
            timestamp.mFlags = AudioTimeStampFlags::SampleTimeValid;
            AudioUnitRender(
                self.0,
                &mut flags,
                NonNull::from(&mut timestamp),
                0,
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

/// An `AudioBufferList` of `channels` one-channel buffers backed by our memory.
struct TestList {
    storage: Vec<u64>,
    _channels: Vec<Vec<f32>>,
}

impl TestList {
    fn new(channels: u32, frames: u32) -> Self {
        let bytes = 8 + channels as usize * size_of::<AudioBuffer>();
        let mut list = Self {
            storage: vec![0u64; bytes.div_ceil(8)],
            _channels: vec![vec![f32::NAN; frames as usize]; channels as usize],
        };

        unsafe {
            let ptr = list.as_ptr();
            (*ptr).mNumberBuffers = channels;
            let first = (&raw mut (*ptr).mBuffers) as *mut AudioBuffer;
            for (i, channel) in list._channels.iter_mut().enumerate() {
                first.add(i).write(AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: frames * 4,
                    mData: channel.as_mut_ptr() as *mut c_void,
                });
            }
        }

        list
    }

    fn as_ptr(&mut self) -> *mut AudioBufferList {
        self.storage.as_mut_ptr() as *mut AudioBufferList
    }
}

/// `AUMIDIOutputCallbackStruct`, which is all the test needs of the host's side.
#[repr(C)]
#[derive(Clone, Copy)]
struct MidiOutputCallbackStruct {
    proc: Option<
        unsafe extern "C-unwind" fn(*mut c_void, *const AudioTimeStamp, u32, *const c_void) -> i32,
    >,
    user_data: *mut c_void,
}

/// The host's MIDI out callback: walks the `MIDIPacketList` by its documented layout (`numPackets:
/// u32` at offset 0; packets from offset 4, each `timeStamp: u64`, `length: u16`, `data[length]`,
/// packed) and logs `(timestamp, data[0], data[1])` per packet.
unsafe extern "C-unwind" fn midi_out_callback(
    _user_data: *mut c_void,
    _timestamp: *const AudioTimeStamp,
    _midi_out_num: u32,
    packet_list: *const c_void,
) -> i32 {
    unsafe {
        let base = packet_list as *const u8;
        let count = (base as *const u32).read_unaligned();
        let mut packet = base.add(4);
        for _ in 0..count {
            let time_stamp = (packet as *const u64).read_unaligned();
            let length = (packet.add(8) as *const u16).read_unaligned() as usize;
            let data = packet.add(10);

            let index = OUT_LEN.load(Ordering::Relaxed);
            if index < OUT_LOG.len() {
                let first = if length > 0 { *data } else { 0 } as u64;
                let second = if length > 1 { *data.add(1) } else { 0 } as u64;
                OUT_LOG[index].store(
                    (time_stamp << 16) | (first << 8) | second,
                    Ordering::Relaxed,
                );
                OUT_LEN.store(index + 1, Ordering::Relaxed);
            }

            // `MIDIPacketNext`: ARM rounds the next packet up to 4 bytes, x86 does not
            let end = data.add(length);
            packet = if cfg!(target_arch = "aarch64") {
                ((end as usize + 3) & !3) as *const u8
            } else {
                end
            };
        }
    }
    0
}

fn logged() -> Vec<(u64, u64, u64)> {
    (0..LOG_LEN.load(Ordering::Relaxed))
        .map(|i| {
            let entry = LOG[i].load(Ordering::Relaxed);
            (entry >> 32, (entry >> 8) & 0xFF_FFFF, entry & 0xFF)
        })
        .collect()
}

fn logged_out() -> Vec<(u64, u64, u64)> {
    (0..OUT_LEN.load(Ordering::Relaxed))
        .map(|i| {
            let entry = OUT_LOG[i].load(Ordering::Relaxed);
            (entry >> 16, (entry >> 8) & 0xFF, entry & 0xFF)
        })
        .collect()
}

fn clear_logs() {
    LOG_LEN.store(0, Ordering::Relaxed);
    OUT_LEN.store(0, Ordering::Relaxed);
}

#[test]
fn midi_reaches_process_sorted_clamped_and_filtered() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_logs();
    let unit = Unit::new();
    unit.start(512);

    unit.midi(0x90, 60, 100, 100);
    unit.midi(0x90, 62, 100, 5);
    // A CC: `Basic` drops it
    unit.midi(0xB0, 7, 90, 0);
    unit.midi(0x80, 60, 0, 9999);
    assert_eq!(unit.render(512), 0);

    assert_eq!(
        logged(),
        vec![
            (KIND_NOTE_ON, 5, 62),
            (KIND_NOTE_ON, 100, 60),
            (KIND_NOTE_OFF, 511, 60)
        ]
    );
}

#[test]
fn midi_out_reaches_the_host_callback() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_logs();
    let unit = Unit::new();
    unit.start(512);

    let callback = MidiOutputCallbackStruct {
        proc: Some(midi_out_callback),
        user_data: ptr::null_mut(),
    };
    let status = unit.set(
        kAudioUnitProperty_MIDIOutputCallback,
        kAudioUnitScope_Global,
        0,
        &callback,
    );
    assert_eq!(status, 0);

    unit.midi(0x90, 60, 100, 100);
    unit.midi(0x90, 62, 100, 5);
    assert_eq!(unit.render(512), 0);

    assert_eq!(logged_out(), vec![(5, 0x90, 74), (100, 0x90, 72)]);
}

#[test]
fn midi_out_info_and_instrument_count() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_logs();
    let unit = Unit::new();

    let (status, names, size) = unit.get::<*const c_void>(
        kAudioUnitProperty_MIDIOutputCallbackInfo,
        kAudioUnitScope_Global,
        0,
    );
    assert_eq!(status, 0);
    assert_eq!(size as usize, size_of::<*const c_void>());
    // The host releases the array
    let names =
        unsafe { CFRetained::from_raw(NonNull::new(names as *mut CFArray<CFString>).unwrap()) };
    assert_eq!(names.len(), 1);
    assert_eq!(names.get(0).unwrap().to_string(), "MIDI Out");

    let (status, count, size) = unit.get::<u32>(
        kMusicDeviceProperty_InstrumentCount,
        kAudioUnitScope_Global,
        0,
    );
    assert_eq!(status, 0);
    assert_eq!(count, 0);
    assert_eq!(size, 4);

    let status = unit.set(
        kMusicDeviceProperty_InstrumentCount,
        kAudioUnitScope_Global,
        0,
        &1u32,
    );
    assert_eq!(status, kAudioUnitErr_PropertyNotWritable);
}

#[test]
fn reset_drops_pending_midi() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_logs();
    let unit = Unit::new();
    unit.start(512);

    unit.midi(0x90, 60, 100, 0);
    assert_eq!(
        unsafe { AudioUnitReset(unit.0, kAudioUnitScope_Global, 0) },
        0
    );
    assert_eq!(unit.render(512), 0);

    assert!(logged().is_empty(), "{:?}", logged());
}

#[test]
fn sysex_is_accepted_and_dropped_for_unit_sysex_type() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    clear_logs();
    let unit = Unit::new();
    unit.start(512);

    let mut message = [0xF0u8, 0x7D, 0x01, 0xF7];
    let status = unsafe {
        MusicDeviceSysEx(
            unit.0,
            NonNull::from(&mut message).cast(),
            message.len() as u32,
        )
    };
    assert_eq!(status, 0);
    assert_eq!(unit.render(512), 0);

    // `()` parses no SysEx
    assert!(logged().is_empty(), "{:?}", logged());
}
