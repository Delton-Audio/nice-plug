// DELTON FORK DELTA: au-1, AUv2 wrapper
//! The AudioToolbox / CoreAudio surface the wrapper uses. Everything the objc2 crates generate is
//! re-exported from here; what they lack (or declare with non-null pointers where C allows null)
//! is defined here.

use std::ffi::c_void;

pub(crate) use objc2_audio_toolbox::{
    kAudioUnitAddPropertyListenerSelect, kAudioUnitAddRenderNotifySelect,
    kAudioUnitErr_CannotDoInCurrentContext, kAudioUnitErr_FailedInitialization,
    kAudioUnitErr_FormatNotSupported, kAudioUnitErr_Initialized, kAudioUnitErr_InvalidElement,
    kAudioUnitErr_InvalidParameter, kAudioUnitErr_InvalidProperty,
    kAudioUnitErr_InvalidPropertyValue, kAudioUnitErr_InvalidScope, kAudioUnitErr_NoConnection,
    kAudioUnitErr_PropertyNotWritable, kAudioUnitErr_TooManyFramesToProcess,
    kAudioUnitErr_Uninitialized, kAudioUnitGetParameterSelect, kAudioUnitGetPropertyInfoSelect,
    kAudioUnitGetPropertySelect, kAudioUnitInitializeSelect, kAudioUnitProperty_BypassEffect,
    kAudioUnitProperty_ClassInfo, kAudioUnitProperty_ClassInfoFromDocument,
    kAudioUnitProperty_ElementCount, kAudioUnitProperty_ElementName,
    kAudioUnitProperty_FactoryPresets, kAudioUnitProperty_HostCallbacks,
    kAudioUnitProperty_InPlaceProcessing, kAudioUnitProperty_LastRenderError,
    kAudioUnitProperty_Latency, kAudioUnitProperty_MakeConnection,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_OfflineRender,
    kAudioUnitProperty_ParameterClumpName, kAudioUnitProperty_ParameterInfo,
    kAudioUnitProperty_ParameterList, kAudioUnitProperty_ParameterStringFromValue,
    kAudioUnitProperty_ParameterValueFromString, kAudioUnitProperty_ParameterValueStrings,
    kAudioUnitProperty_PresentPreset, kAudioUnitProperty_RenderContextObserver,
    kAudioUnitProperty_RenderQuality, kAudioUnitProperty_SampleRate,
    kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_ShouldAllocateBuffer,
    kAudioUnitProperty_StreamFormat, kAudioUnitProperty_SupportedNumChannels,
    kAudioUnitProperty_TailTime, kAudioUnitRemovePropertyListenerSelect,
    kAudioUnitRemovePropertyListenerWithUserDataSelect, kAudioUnitRemoveRenderNotifySelect,
    kAudioUnitRenderSelect, kAudioUnitResetSelect, kAudioUnitScheduleParametersSelect,
    kAudioUnitScope_Global, kAudioUnitScope_Input, kAudioUnitScope_Output,
    kAudioUnitSetParameterSelect, kAudioUnitSetPropertySelect, kAudioUnitUninitializeSelect,
    kRenderQuality_Max, AUChannelInfo, AUParameterEventType, AUParameterListenerNotify, AUPreset,
    AudioComponentInstance, AudioComponentMethod, AudioComponentPlugInInterface, AudioUnit,
    AudioUnitConnection, AudioUnitElement, AudioUnitParameter, AudioUnitParameterID,
    AudioUnitParameterInfo, AudioUnitParameterNameInfo, AudioUnitParameterOptions,
    AudioUnitParameterUnit, AudioUnitPropertyID, AudioUnitRender, AudioUnitRenderActionFlags,
    AudioUnitScope, HostCallbackInfo, HostCallback_GetBeatAndTempo,
    HostCallback_GetMusicalTimeLocation, HostCallback_GetTransportState2,
};
// DELTON FORK DELTA: au-4b
pub(crate) use objc2_audio_toolbox::{
    kAudioUnitProperty_MIDIOutputCallback, kAudioUnitProperty_MIDIOutputCallbackInfo,
    kMusicDeviceMIDIEventSelect, kMusicDeviceProperty_InstrumentCount, kMusicDeviceSysExSelect,
};
pub(crate) use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM, AudioBuffer,
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};
pub(crate) use objc2_core_foundation::{
    CFArray, CFData, CFDictionary, CFMutableDictionary, CFNumber, CFRetained, CFString, CFType,
};

// DELTON FORK DELTA: au-4a. `AudioUnitProperties.h`; not in the generated crate.
/// `AudioUnitRenderContext`: what the OS passes the render-context observer block.
#[repr(C)]
pub(crate) struct AudioUnitRenderContext {
    pub workgroup: *mut c_void,
    pub reserved: [u32; 6],
}

// DELTON FORK DELTA: au-4a
// The `os_object` reference count functions (libSystem, macOS 10.12+).
unsafe extern "C" {
    pub(crate) fn os_retain(object: *mut c_void) -> *mut c_void;
    pub(crate) fn os_release(object: *mut c_void);
}

/// Apple's `OSStatus`.
pub(crate) type OSStatus = i32;

/// `noErr`.
pub(crate) const NO_ERR: OSStatus = 0;
/// `paramErr` (`kAudio_ParamError`).
pub(crate) const PARAM_ERR: OSStatus = -50;

/// The private property that returns the wrapper pointer (for the au-2 view factory). JUCE uses the
/// same trick. 'DTN1'.
pub(crate) const PRIVATE_WRAPPER_PROPERTY: AudioUnitPropertyID = 0x4454_4E31;

// The method selectors, as the `i16` that `Lookup` receives
pub(crate) const SELECT_INITIALIZE: i16 = kAudioUnitInitializeSelect as i16;
pub(crate) const SELECT_UNINITIALIZE: i16 = kAudioUnitUninitializeSelect as i16;
pub(crate) const SELECT_GET_PROPERTY_INFO: i16 = kAudioUnitGetPropertyInfoSelect as i16;
pub(crate) const SELECT_GET_PROPERTY: i16 = kAudioUnitGetPropertySelect as i16;
pub(crate) const SELECT_SET_PROPERTY: i16 = kAudioUnitSetPropertySelect as i16;
pub(crate) const SELECT_ADD_PROPERTY_LISTENER: i16 = kAudioUnitAddPropertyListenerSelect as i16;
pub(crate) const SELECT_REMOVE_PROPERTY_LISTENER: i16 =
    kAudioUnitRemovePropertyListenerSelect as i16;
pub(crate) const SELECT_REMOVE_PROPERTY_LISTENER_WITH_USER_DATA: i16 =
    kAudioUnitRemovePropertyListenerWithUserDataSelect as i16;
pub(crate) const SELECT_ADD_RENDER_NOTIFY: i16 = kAudioUnitAddRenderNotifySelect as i16;
pub(crate) const SELECT_REMOVE_RENDER_NOTIFY: i16 = kAudioUnitRemoveRenderNotifySelect as i16;
pub(crate) const SELECT_GET_PARAMETER: i16 = kAudioUnitGetParameterSelect as i16;
pub(crate) const SELECT_SET_PARAMETER: i16 = kAudioUnitSetParameterSelect as i16;
pub(crate) const SELECT_SCHEDULE_PARAMETERS: i16 = kAudioUnitScheduleParametersSelect as i16;
pub(crate) const SELECT_RENDER: i16 = kAudioUnitRenderSelect as i16;
pub(crate) const SELECT_RESET: i16 = kAudioUnitResetSelect as i16;
// DELTON FORK DELTA: au-4b
pub(crate) const SELECT_MIDI_EVENT: i16 = kMusicDeviceMIDIEventSelect as i16;
// DELTON FORK DELTA: au-4b
pub(crate) const SELECT_SYSEX: i16 = kMusicDeviceSysExSelect as i16;

/// A render callback / render notify, with C's nullable pointers. The generated `AURenderCallback`
/// declares the refcon and the flags as `NonNull`, but C hosts pass a null refcon for callbacks that
/// need none, and the layout is identical.
pub(crate) type RenderProc = unsafe extern "C-unwind" fn(
    *mut c_void,
    *mut AudioUnitRenderActionFlags,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> OSStatus;

// DELTON FORK DELTA: au-4b. AudioUnitProperties.h / CoreMIDI MIDIServices.h.
/// `AUMIDIOutputCallback`, with C's nullable pointers. `packet_list` is a `const MIDIPacketList*`.
pub(crate) type MidiOutputProc = unsafe extern "C-unwind" fn(
    user_data: *mut c_void,
    timestamp: *const AudioTimeStamp,
    midi_out_num: u32,
    packet_list: *const c_void,
) -> OSStatus;

// DELTON FORK DELTA: au-4b
/// `AUMIDIOutputCallbackStruct`, with a nullable user data pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MidiOutputCallbackStruct {
    pub proc: Option<MidiOutputProc>,
    pub user_data: *mut c_void,
}

// DELTON FORK DELTA: au-4b
// MIDIPacketList and MIDIPacket are opaque here: CoreMIDI builds them (their layout is packed and
// differs between arm64 and x86_64).
#[link(name = "CoreMIDI", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn MIDIPacketListInit(list: *mut c_void) -> *mut c_void;
    pub(crate) fn MIDIPacketListAdd(
        list: *mut c_void,
        list_size: usize,
        current: *mut c_void,
        time: u64,
        length: usize,
        data: *const u8,
    ) -> *mut c_void;
}

/// A property listener, with C's nullable user data pointer (see [`RenderProc`]).
pub(crate) type PropertyListenerProc = unsafe extern "C-unwind" fn(
    *mut c_void,
    AudioUnit,
    AudioUnitPropertyID,
    AudioUnitScope,
    AudioUnitElement,
);

/// `AURenderCallbackStruct`, with a nullable refcon (see [`RenderProc`]).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RenderCallbackStruct {
    pub input_proc: Option<RenderProc>,
    pub input_proc_ref_con: *mut c_void,
}

/// `AudioUnitParameterStringFromValue`, with nullable pointers: `inValue` may be null, meaning the
/// parameter's current value.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ParameterStringFromValue {
    pub in_param_id: AudioUnitParameterID,
    pub in_value: *const f32,
    pub out_string: *const CFString,
}

/// `AudioUnitParameterValueFromString`, with a nullable `inString`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ParameterValueFromString {
    pub in_param_id: AudioUnitParameterID,
    pub in_string: *const CFString,
    pub out_value: f32,
}

/// The ramp half of [`ParameterEventValues`].
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RampEvent {
    pub start_buffer_offset: u32,
    pub duration_in_frames: u32,
    pub start_value: f32,
    pub end_value: f32,
}

/// The immediate half of [`ParameterEventValues`].
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ImmediateEvent {
    pub buffer_offset: u32,
    pub value: f32,
}

/// The union inside `AudioUnitParameterEvent`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union ParameterEventValues {
    pub ramp: RampEvent,
    pub immediate: ImmediateEvent,
}

/// `AudioUnitParameterEvent`, which the generated bindings lack.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AudioUnitParameterEvent {
    pub scope: AudioUnitScope,
    pub element: AudioUnitElement,
    pub parameter: AudioUnitParameterID,
    pub event_type: u32,
    pub values: ParameterEventValues,
}

const _: () = assert!(std::mem::size_of::<AudioUnitParameterEvent>() == 32);

/// The unit handle the host gave `Open`, kept so property listeners and parameter listeners can be
/// told about changes.
#[derive(Clone, Copy)]
pub(crate) struct UnitHandle(pub AudioComponentInstance);

// SAFETY: An `AudioComponentInstance` is an opaque handle that Apple's own APIs accept from any
// thread. We never dereference it, we only pass it back to Apple's notification functions and to the
// host's listener callbacks.
unsafe impl Send for UnitHandle {}
// SAFETY: See above.
unsafe impl Sync for UnitHandle {}

// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
pub(crate) use objc2_audio_toolbox::{
    kAudioUnitProperty_CocoaUI, AudioComponentDescription, AudioComponentGetDescription,
    AudioComponentInstanceGetComponent, AudioUnitCocoaViewInfo, AudioUnitEventType,
    AudioUnitGetProperty, AudioUnitProperty,
};
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
pub(crate) use objc2_core_foundation::{CFURLPathStyle, CFURL};

/// The argument union of [`AudioUnitEvent`].
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union AudioUnitEventArgument {
    pub parameter: AudioUnitParameter,
    pub property: AudioUnitProperty,
}

/// `AudioUnitEvent`, which the generated bindings lack.
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AudioUnitEvent {
    pub event_type: AudioUnitEventType,
    pub argument: AudioUnitEventArgument,
}

#[cfg(feature = "editor")]
const _: () = assert!(std::mem::size_of::<AudioUnitEvent>() == 32);

/// `Dl_info`, for `dladdr`.
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
#[repr(C)]
pub(crate) struct DlInfo {
    pub dli_fname: *const std::ffi::c_char,
    pub dli_fbase: *mut c_void,
    pub dli_sname: *const std::ffi::c_char,
    pub dli_saddr: *mut c_void,
}

// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
unsafe extern "C-unwind" {
    /// `AUEventListenerNotify` (AudioToolbox). A null listener and object tell every listener.
    pub(crate) fn AUEventListenerNotify(
        sending_listener: *mut c_void,
        sending_object: *mut c_void,
        event: *const AudioUnitEvent,
    ) -> OSStatus;
}

// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
unsafe extern "C" {
    /// `dladdr` (libSystem).
    pub(crate) fn dladdr(addr: *const c_void, info: *mut DlInfo) -> std::ffi::c_int;
}
