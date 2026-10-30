// DELTON FORK DELTA: au-1, AUv2 wrapper
//! The AudioComponent factory: the plugin interface (Open, Close, Lookup) and the method
//! trampolines the host reaches through `Lookup`.
//!
//! Every trampoline catches panics. A panic must never unwind into the host, so it becomes an
//! error code (and for Render, silence).

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};

use super::convert::{four_cc_valid, manufacturer_valid};
use super::sys::*;
use super::wrapper::Wrapper;
use super::{au_type_of, AuPlugin};

/// One AU instance, as the host sees it: a pointer to this struct is a pointer to the
/// `AudioComponentPlugInInterface`, so the interface MUST stay the first field.
#[repr(C)]
pub(crate) struct AuInstance<P: AuPlugin> {
    interface: AudioComponentPlugInInterface,
    /// `Some` after `Open`.
    wrapper: Option<Arc<Wrapper<P>>>,
}

/// The factory function named in the bundle's `Info.plist`. Allocates the plugin interface for one
/// instance; the host then calls `Open` on it.
///
/// Returns null if the plugin's four-char codes are not valid, so the host reports "cannot open"
/// instead of registering a component it could not tell apart from Apple's.
///
/// # Safety
///
/// `desc` is the description the loader is instantiating, or null. It is not otherwise needed.
pub unsafe fn factory<P: AuPlugin>(_desc: *const c_void) -> *mut c_void {
    static SETUP_LOGGER: Once = Once::new();
    static WARNED_TYPE: AtomicBool = AtomicBool::new(false);

    let result = catch_unwind(AssertUnwindSafe(|| {
        SETUP_LOGGER.call_once(crate::wrapper::util::setup_logger::<P>);

        if !four_cc_valid(P::AU_SUBTYPE) || !manufacturer_valid(P::AU_MANUFACTURER) {
            crate::nice_error!(
                "AU: the plugin's codes are not valid ({:?} {:?}): every byte must be printable \
                 ASCII, and the manufacturer needs a byte that is not a lowercase letter",
                P::AU_MANUFACTURER,
                P::AU_SUBTYPE
            );
            return ptr::null_mut();
        }

        if P::AU_TYPE.is_none() {
            let has_main_input = P::AUDIO_IO_LAYOUTS
                .first()
                .is_some_and(|layout| layout.main_input_channels.is_some());
            let has_midi_input = P::MIDI_INPUT != nice_plug_core::midi::MidiConfig::None;
            if !has_main_input && !has_midi_input && !WARNED_TYPE.swap(true, Ordering::Relaxed) {
                crate::nice_warn!(
                    "AU: the plugin has no main input and no MIDI input, so it registers as an \
                     effect ({:?}). It should set AU_TYPE.",
                    au_type_of::<P>()
                );
            }
        }

        let instance = Box::new(AuInstance::<P> {
            interface: AudioComponentPlugInInterface {
                Open: open::<P>,
                Close: close::<P>,
                Lookup: lookup::<P>,
                reserved: ptr::null_mut(),
            },
            wrapper: None,
        });

        Box::into_raw(instance) as *mut c_void
    }));

    result.unwrap_or(ptr::null_mut())
}

/// The instance behind a `self` pointer the host passed to a method, once `Open` has run.
///
/// # Safety
///
/// `this` must be null or a pointer `factory` returned that `Close` has not freed.
unsafe fn wrapper_of<'a, P: AuPlugin>(this: *mut c_void) -> Option<&'a Wrapper<P>> {
    // SAFETY: See above; `interface` is the first field of a `repr(C)` struct.
    let instance = unsafe { (this as *const AuInstance<P>).as_ref()? };
    instance.wrapper.as_deref()
}

/// Run a method body, turning a panic into an error code. A null instance, or one `Open` has not
/// set up, gives `no_wrapper`.
///
/// # Safety
///
/// See [`wrapper_of`].
unsafe fn guarded<P: AuPlugin>(
    this: *mut c_void,
    no_wrapper: OSStatus,
    body: impl FnOnce(&Wrapper<P>) -> OSStatus,
) -> OSStatus {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Forwarded from the caller.
        match unsafe { wrapper_of::<P>(this) } {
            Some(wrapper) => body(wrapper),
            None => no_wrapper,
        }
    }));

    result.unwrap_or(kAudioUnitErr_CannotDoInCurrentContext)
}

unsafe extern "C-unwind" fn open<P: AuPlugin>(
    this: NonNull<c_void>,
    unit: AudioComponentInstance,
) -> OSStatus {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `this` is the pointer `factory` returned.
        let instance = unsafe { &mut *(this.as_ptr() as *mut AuInstance<P>) };
        match Wrapper::<P>::new(unit) {
            Ok(wrapper) => {
                instance.wrapper = Some(wrapper);
                NO_ERR
            }
            Err(status) => status,
        }
    }));

    result.unwrap_or(PARAM_ERR)
}

unsafe extern "C-unwind" fn close<P: AuPlugin>(this: NonNull<c_void>) -> OSStatus {
    // The AU contract: Close frees the instance
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `this` is the pointer `factory` returned, and the host does not use it again.
        let instance = unsafe { Box::from_raw(this.as_ptr() as *mut AuInstance<P>) };
        if let Some(wrapper) = &instance.wrapper {
            // DELTON FORK DELTA: au-2 (the editor goes before the plugin is deactivated)
            #[cfg(feature = "editor")]
            wrapper.close_editor();
            wrapper.uninitialize();
        }
        drop(instance);
    }));

    if result.is_ok() {
        NO_ERR
    } else {
        PARAM_ERR
    }
}

/// Turn a typed trampoline into the untyped method pointer `Lookup` returns.
///
/// # Safety
///
/// `F` must be a function pointer type, and the host must call it with the signature `F` has, which
/// is what the AU dispatch code does for the selector the trampoline was looked up for.
unsafe fn method<F: Copy>(trampoline: F) -> AudioComponentMethod {
    debug_assert_eq!(
        std::mem::size_of::<F>(),
        std::mem::size_of::<AudioComponentMethod>()
    );
    // SAFETY: Both are function pointers of the same size.
    unsafe { std::mem::transmute_copy::<F, AudioComponentMethod>(&trampoline) }
}

unsafe extern "C-unwind" fn lookup<P: AuPlugin>(selector: i16) -> AudioComponentMethod {
    // Each trampoline has the signature of the `AudioUnit*Proc` type for its selector, except that
    // the pointer arguments are raw pointers instead of `NonNull`, so a null never becomes UB.
    unsafe {
        match selector {
            SELECT_INITIALIZE => method(initialize::<P> as unsafe extern "C-unwind" fn(_) -> _),
            SELECT_UNINITIALIZE => method(uninitialize::<P> as unsafe extern "C-unwind" fn(_) -> _),
            SELECT_GET_PROPERTY_INFO => {
                method(get_property_info::<P> as unsafe extern "C-unwind" fn(_, _, _, _, _, _) -> _)
            }
            SELECT_GET_PROPERTY => {
                method(get_property::<P> as unsafe extern "C-unwind" fn(_, _, _, _, _, _) -> _)
            }
            SELECT_SET_PROPERTY => {
                method(set_property::<P> as unsafe extern "C-unwind" fn(_, _, _, _, _, _) -> _)
            }
            SELECT_ADD_PROPERTY_LISTENER => {
                method(add_property_listener::<P> as unsafe extern "C-unwind" fn(_, _, _, _) -> _)
            }
            SELECT_REMOVE_PROPERTY_LISTENER => {
                method(remove_property_listener::<P> as unsafe extern "C-unwind" fn(_, _, _) -> _)
            }
            SELECT_REMOVE_PROPERTY_LISTENER_WITH_USER_DATA => method(
                remove_property_listener_with_user_data::<P>
                    as unsafe extern "C-unwind" fn(_, _, _, _) -> _,
            ),
            SELECT_ADD_RENDER_NOTIFY => {
                method(add_render_notify::<P> as unsafe extern "C-unwind" fn(_, _, _) -> _)
            }
            SELECT_REMOVE_RENDER_NOTIFY => {
                method(remove_render_notify::<P> as unsafe extern "C-unwind" fn(_, _, _) -> _)
            }
            SELECT_GET_PARAMETER => {
                method(get_parameter::<P> as unsafe extern "C-unwind" fn(_, _, _, _, _) -> _)
            }
            SELECT_SET_PARAMETER => {
                method(set_parameter::<P> as unsafe extern "C-unwind" fn(_, _, _, _, _, _) -> _)
            }
            SELECT_SCHEDULE_PARAMETERS => {
                method(schedule_parameters::<P> as unsafe extern "C-unwind" fn(_, _, _) -> _)
            }
            SELECT_RENDER => {
                method(render::<P> as unsafe extern "C-unwind" fn(_, _, _, _, _, _) -> _)
            }
            SELECT_RESET => method(reset::<P> as unsafe extern "C-unwind" fn(_, _, _) -> _),
            _ => None,
        }
    }
}

unsafe extern "C-unwind" fn initialize<P: AuPlugin>(this: *mut c_void) -> OSStatus {
    unsafe { guarded::<P>(this, PARAM_ERR, |wrapper| wrapper.initialize()) }
}

unsafe extern "C-unwind" fn uninitialize<P: AuPlugin>(this: *mut c_void) -> OSStatus {
    unsafe { guarded::<P>(this, PARAM_ERR, |wrapper| wrapper.uninitialize()) }
}

unsafe extern "C-unwind" fn get_property_info<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    out_data_size: *mut u32,
    out_writable: *mut u8,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.get_property_info(id, scope, element, out_data_size, out_writable)
        })
    }
}

unsafe extern "C-unwind" fn get_property<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    out_data: *mut c_void,
    io_data_size: *mut u32,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.get_property(id, scope, element, out_data, io_data_size)
        })
    }
}

unsafe extern "C-unwind" fn set_property<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    in_data: *const c_void,
    in_data_size: u32,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.set_property(id, scope, element, in_data, in_data_size)
        })
    }
}

unsafe extern "C-unwind" fn add_property_listener<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitPropertyID,
    proc: Option<PropertyListenerProc>,
    user_data: *mut c_void,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.add_property_listener(id, proc, user_data as usize)
        })
    }
}

unsafe extern "C-unwind" fn remove_property_listener<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitPropertyID,
    proc: Option<PropertyListenerProc>,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.remove_property_listener(id, proc, None)
        })
    }
}

unsafe extern "C-unwind" fn remove_property_listener_with_user_data<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitPropertyID,
    proc: Option<PropertyListenerProc>,
    user_data: *mut c_void,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.remove_property_listener(id, proc, Some(user_data as usize))
        })
    }
}

unsafe extern "C-unwind" fn add_render_notify<P: AuPlugin>(
    this: *mut c_void,
    proc: Option<RenderProc>,
    refcon: *mut c_void,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.add_render_notify(proc, refcon)
        })
    }
}

unsafe extern "C-unwind" fn remove_render_notify<P: AuPlugin>(
    this: *mut c_void,
    proc: Option<RenderProc>,
    refcon: *mut c_void,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.remove_render_notify(proc, refcon)
        })
    }
}

unsafe extern "C-unwind" fn get_parameter<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitParameterID,
    scope: AudioUnitScope,
    _element: AudioUnitElement,
    out_value: *mut f32,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.get_parameter(id, scope, out_value)
        })
    }
}

unsafe extern "C-unwind" fn set_parameter<P: AuPlugin>(
    this: *mut c_void,
    id: AudioUnitParameterID,
    scope: AudioUnitScope,
    _element: AudioUnitElement,
    value: f32,
    buffer_offset_in_frames: u32,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.set_parameter(id, scope, value, buffer_offset_in_frames)
        })
    }
}

unsafe extern "C-unwind" fn schedule_parameters<P: AuPlugin>(
    this: *mut c_void,
    events: *const AudioUnitParameterEvent,
    event_count: u32,
) -> OSStatus {
    unsafe {
        guarded::<P>(this, PARAM_ERR, |wrapper| {
            wrapper.schedule_parameters(events, event_count)
        })
    }
}

unsafe extern "C-unwind" fn reset<P: AuPlugin>(
    this: *mut c_void,
    _scope: AudioUnitScope,
    _element: AudioUnitElement,
) -> OSStatus {
    unsafe { guarded::<P>(this, PARAM_ERR, |wrapper| wrapper.reset()) }
}

unsafe extern "C-unwind" fn render<P: AuPlugin>(
    this: *mut c_void,
    io_action_flags: *mut AudioUnitRenderActionFlags,
    timestamp: *const AudioTimeStamp,
    output_bus_number: u32,
    number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: See `wrapper_of`.
        match unsafe { wrapper_of::<P>(this) } {
            Some(wrapper) => unsafe {
                wrapper.render(
                    io_action_flags,
                    timestamp,
                    output_bus_number,
                    number_frames,
                    io_data,
                )
            },
            None => kAudioUnitErr_Uninitialized,
        }
    }));

    match result {
        Ok(status) => status,
        Err(_) => {
            // SAFETY: `io_data` is the host's list; only buffers with a data pointer are touched.
            unsafe { silence(io_data) };
            kAudioUnitErr_CannotDoInCurrentContext
        }
    }
}

/// Zero every output buffer the host passed, after a panic in render.
///
/// # Safety
///
/// `list` must be null or a valid `AudioBufferList`.
unsafe fn silence(list: *mut AudioBufferList) {
    if list.is_null() {
        return;
    }

    // SAFETY: Checked for null above; the host's list holds `mNumberBuffers` buffers.
    unsafe {
        let count = (*list).mNumberBuffers as usize;
        let first = (&raw mut (*list).mBuffers) as *mut AudioBuffer;
        for i in 0..count {
            let buffer = &*first.add(i);
            if !buffer.mData.is_null() {
                ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
            }
        }
    }
}
