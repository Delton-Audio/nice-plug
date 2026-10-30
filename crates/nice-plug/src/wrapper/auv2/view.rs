// DELTON FORK DELTA: au-2, AUv2 editor
//! The Cocoa side of the AUv2 editor. An AU host asks the unit for the `CocoaUI` property, which
//! names a view factory class. The host instantiates that class and asks it for a view; the view is
//! a container `NSView` that the plugin's editor is spawned into.
//!
//! The classes are made at runtime, per plugin and per loaded image: Objective-C class names are
//! process-wide, and two builds of one plugin can be loaded into one host. See
//! [`view_names`](super::view_names) for how the names are derived.
//!
//! Everything here runs on the main thread. A panic must never unwind into Objective-C, so every
//! method implementation catches panics and returns a neutral value.

use std::error::Error;
use std::ffi::{c_void, CStr, CString, OsStr};
use std::mem::{zeroed, ManuallyDrop};
use std::os::unix::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Weak};

use fragile::Fragile;
use nice_plug_core::editor::dpi::{NativeSize, Size};
use nice_plug_core::editor::{
    Editor, EditorHandle, HostCallbacks, HostMainThreadCaller, HostMethods, ParentWindowHandle,
};
use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, ClassBuilder, ProtocolBuilder, Sel};
use objc2::{msg_send, sel};
use parking_lot::Mutex;

use super::convert::four_cc;
use super::sys::*;
use super::view_names::{build_hash, bundle_dir_of, class_name, macho_uuid};
use super::wrapper::{EditorSlot, Task, Wrapper};
use super::AuPlugin;

// AppKit is not linked by anything else in this crate. NSView lives there.
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

/// `CGSize`. The name must be exactly this: objc2 checks an override's encoding against NSView's in
/// debug builds, and NSView's `setFrameSize:` takes `{CGSize=dd}`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CGSize {
    pub width: f64,
    pub height: f64,
}

// SAFETY: The layout and the encoding are those of Apple's `CGSize`.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

// SAFETY: See above.
unsafe impl RefEncode for CGSize {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// `CGPoint`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CGPoint {
    pub x: f64,
    pub y: f64,
}

// SAFETY: The layout and the encoding are those of Apple's `CGPoint`.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

// SAFETY: See above.
unsafe impl RefEncode for CGPoint {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// `CGRect`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

// SAFETY: The layout and the encoding are those of Apple's `CGRect`.
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

// SAFETY: See above.
unsafe impl RefEncode for CGRect {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

// ------------------------------------------------------------------------------------------------
// Image identity and names
// ------------------------------------------------------------------------------------------------

/// The loaded image (dylib or executable) this code lives in.
struct Image {
    path: PathBuf,
    /// The address of the image's mach header.
    base: *const u8,
}

/// `dladdr` on an address inside this crate's code.
fn this_image(addr: *const c_void) -> Option<Image> {
    let mut info = DlInfo {
        dli_fname: ptr::null(),
        dli_fbase: ptr::null_mut(),
        dli_sname: ptr::null(),
        dli_saddr: ptr::null_mut(),
    };

    // SAFETY: `dladdr` accepts any address and writes only to `info`.
    if unsafe { dladdr(addr, &mut info) } == 0
        || info.dli_fname.is_null()
        || info.dli_fbase.is_null()
    {
        return None;
    }

    // SAFETY: `dli_fname` is a NUL-terminated path owned by the dynamic linker.
    let path = unsafe { CStr::from_ptr(info.dli_fname) };
    Some(Image {
        path: PathBuf::from(OsStr::from_bytes(path.to_bytes())),
        base: info.dli_fbase as *const u8,
    })
}

/// The LC_UUID of the image: read the 32 header bytes at `base`; if the magic is 0xfeedfacf, read
/// `32 + sizeofcmds` bytes (`sizeofcmds` is the u32 at offset 20, capped at 1 MiB) and look for the
/// UUID command.
///
/// # Safety
///
/// `base` must be the mach header of a loaded image.
unsafe fn image_uuid(base: *const u8) -> Option<[u8; 16]> {
    const HEADER_SIZE: usize = 32;
    const MAX_COMMANDS_SIZE: usize = 1 << 20;

    // SAFETY: The header of a loaded image is readable.
    let header = unsafe { std::slice::from_raw_parts(base, HEADER_SIZE) };
    if u32::from_le_bytes(header[0..4].try_into().ok()?) != 0xfeed_facf {
        return None;
    }
    let commands_size = u32::from_le_bytes(header[20..24].try_into().ok()?) as usize;
    if commands_size > MAX_COMMANDS_SIZE {
        return None;
    }

    // SAFETY: The load commands of a loaded image directly follow its header.
    let image = unsafe { std::slice::from_raw_parts(base, HEADER_SIZE + commands_size) };
    macho_uuid(image)
}

/// The names of the factory class and the container class for `P` in this image.
fn class_names<P: AuPlugin>(image: &Image) -> (String, String) {
    // SAFETY: `image.base` came from `dladdr`, so it is the mach header of a loaded image.
    let uuid = unsafe { image_uuid(image.base) };
    let hash = build_hash(
        P::AU_MANUFACTURER,
        P::AU_SUBTYPE,
        P::VERSION,
        uuid,
        image.base as usize as u64,
    );

    (
        class_name("NiceAuv2View", P::AU_MANUFACTURER, P::AU_SUBTYPE, hash),
        class_name("NiceAuv2Container", P::AU_MANUFACTURER, P::AU_SUBTYPE, hash),
    )
}

// ------------------------------------------------------------------------------------------------
// Class registration
// ------------------------------------------------------------------------------------------------

/// The `AUCocoaUIBase` protocol. Hosts check `conformsToProtocol:`, but the protocol only exists at
/// runtime if some loaded image compiled `@protocol(AUCocoaUIBase)`, which a test process may not
/// have, so it is built here when it is missing.
fn cocoa_ui_protocol() -> Option<&'static AnyProtocol> {
    if let Some(protocol) = AnyProtocol::get(c"AUCocoaUIBase") {
        return Some(protocol);
    }

    match ProtocolBuilder::new(c"AUCocoaUIBase") {
        Some(mut builder) => {
            builder.add_method_description::<(), u32>(sel!(interfaceVersion), true);
            builder.add_method_description::<(*mut c_void, CGSize), *mut AnyObject>(
                sel!(uiViewForAudioUnit:withSize:),
                true,
            );
            Some(builder.register())
        }
        // Registered by someone else in the meantime
        None => AnyProtocol::get(c"AUCocoaUIBase"),
    }
}

/// The factory and container classes for `P`, registering them on first use. Idempotent: if a class
/// of that name already exists it is reused (the name includes this image's identity, so an
/// existing class was registered by this same image).
fn classes<P: AuPlugin>(image: &Image) -> Option<(&'static AnyClass, &'static AnyClass)> {
    // Registration is two steps (look up, then register), so it is serialized
    static REGISTRATION: Mutex<()> = Mutex::new(());
    let _guard = REGISTRATION.lock();

    let (factory_name, container_name) = class_names::<P>(image);
    let factory_name = CString::new(factory_name).ok()?;
    let container_name = CString::new(container_name).ok()?;

    let factory = match AnyClass::get(&factory_name) {
        Some(class) => class,
        None => register_factory::<P>(&factory_name)?,
    };
    let container = match AnyClass::get(&container_name) {
        Some(class) => class,
        None => register_container::<P>(&container_name)?,
    };

    Some((factory, container))
}

fn register_factory<P: AuPlugin>(name: &CStr) -> Option<&'static AnyClass> {
    let Some(superclass) = AnyClass::get(c"NSObject") else {
        crate::nice_error!("AU: NSObject is not available");
        return None;
    };
    let protocol = cocoa_ui_protocol();
    let Some(mut builder) = ClassBuilder::new(name, superclass) else {
        return AnyClass::get(name);
    };
    if let Some(protocol) = protocol {
        builder.add_protocol(protocol);
    }

    // SAFETY: The function signatures match the selectors' Objective-C signatures.
    unsafe {
        builder.add_method(
            sel!(interfaceVersion),
            interface_version as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(uiViewForAudioUnit:withSize:),
            ui_view_for_audio_unit::<P> as extern "C-unwind" fn(_, _, _, _) -> _,
        );
    }

    Some(builder.register())
}

fn register_container<P: AuPlugin>(name: &CStr) -> Option<&'static AnyClass> {
    let Some(superclass) = AnyClass::get(c"NSView") else {
        crate::nice_error!("AU: NSView is not available");
        return None;
    };
    let Some(mut builder) = ClassBuilder::new(name, superclass) else {
        return AnyClass::get(name);
    };

    // A `Weak<Wrapper<P>>` as a raw pointer, and whether the editor itself is resizing the view
    builder.add_ivar::<*const c_void>(c"niceWrapper");
    builder.add_ivar::<u8>(c"niceEditorResize");

    // SAFETY: The function signatures match the selectors' Objective-C signatures.
    unsafe {
        builder.add_method(
            sel!(setFrameSize:),
            container_set_frame_size::<P> as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(viewDidMoveToWindow),
            container_view_did_move_to_window::<P> as extern "C-unwind" fn(_, _),
        );
        builder.add_method(
            sel!(dealloc),
            container_dealloc::<P> as extern "C-unwind" fn(_, _),
        );
    }

    Some(builder.register())
}

// ------------------------------------------------------------------------------------------------
// The container's instance variables
// ------------------------------------------------------------------------------------------------

/// A pointer to one of the container's instance variables.
///
/// # Safety
///
/// `this` must be an instance of a class made by [`register_container`] (or a subclass of one), and
/// `T` must be the type the instance variable was added with.
unsafe fn container_ivar<T: Encode>(this: &AnyObject, name: &CStr) -> Option<*mut T> {
    let ivar = this.class().instance_variable(name)?;
    // SAFETY: Upheld by the caller. Subclasses (such as a KVO subclass) keep the layout of their
    // superclass.
    Some(unsafe { ivar.load_ptr::<T>(this) })
}

/// The wrapper a container's `niceWrapper` instance variable points at, if it is set and alive.
///
/// # Safety
///
/// `raw` must be null or a pointer made by `Weak::into_raw` that has not been dropped.
unsafe fn upgrade_wrapper<P: AuPlugin>(raw: *const c_void) -> Option<Arc<Wrapper<P>>> {
    if raw.is_null() {
        return None;
    }

    // SAFETY: Upheld by the caller. The `Weak` is not dropped here: it belongs to the container.
    let weak = ManuallyDrop::new(unsafe { Weak::from_raw(raw as *const Wrapper<P>) });
    weak.upgrade()
}

/// NSView, which the container's overrides forward to.
fn ns_view_class() -> &'static AnyClass {
    AnyClass::get(c"NSView").expect("NSView is not available")
}

// ------------------------------------------------------------------------------------------------
// The factory class
// ------------------------------------------------------------------------------------------------

extern "C-unwind" fn interface_version(_this: &AnyObject, _sel: Sel) -> u32 {
    0
}

extern "C-unwind" fn ui_view_for_audio_unit<P: AuPlugin>(
    _this: &AnyObject,
    _sel: Sel,
    unit: *mut c_void,
    _preferred_size: CGSize,
) -> *mut AnyObject {
    match catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The host passes the unit it got the factory from.
        unsafe { make_view::<P>(unit) }
    })) {
        Ok(view) => view,
        Err(_) => {
            crate::nice_error!("AU: panic while creating the editor view");
            ptr::null_mut()
        }
    }
}

/// `-[factory uiViewForAudioUnit:withSize:]`. Returns an autoreleased container view with the
/// editor spawned inside it, or null.
///
/// # Safety
///
/// `unit` must be null or an `AudioUnit` that is alive for the call.
unsafe fn make_view<P: AuPlugin>(unit: *mut c_void) -> *mut AnyObject {
    // 1. The unit
    if unit.is_null() {
        return ptr::null_mut();
    }
    let unit = unit as AudioUnit;

    // 2. It must be this plugin's unit
    // SAFETY: `unit` is alive.
    let component = unsafe { AudioComponentInstanceGetComponent(unit) };
    // SAFETY: An all-zero component description is valid.
    let mut description: AudioComponentDescription = unsafe { zeroed() };
    let status = if component.is_null() {
        PARAM_ERR
    } else {
        // SAFETY: `component` is not null and `description` is valid for writes.
        unsafe { AudioComponentGetDescription(component, NonNull::from(&mut description)) }
    };
    if status != NO_ERR
        || description.componentSubType != four_cc(P::AU_SUBTYPE)
        || description.componentManufacturer != four_cc(P::AU_MANUFACTURER)
    {
        crate::nice_error!(
            "AU: the editor view was requested for a unit that is not this plugin's"
        );
        return ptr::null_mut();
    }

    // 3. The wrapper, through the private property
    let mut wrapper_ptr: *const Wrapper<P> = ptr::null();
    let mut size = size_of::<*const Wrapper<P>>() as u32;
    // SAFETY: The property writes one pointer into `wrapper_ptr`.
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            PRIVATE_WRAPPER_PROPERTY,
            kAudioUnitScope_Global,
            0,
            NonNull::from(&mut wrapper_ptr).cast(),
            NonNull::from(&mut size),
        )
    };
    if status != NO_ERR || wrapper_ptr.is_null() {
        crate::nice_error!(
            "AU: could not get the wrapper from the unit (status {})",
            status
        );
        return ptr::null_mut();
    }
    // SAFETY: The unit is alive, so its wrapper is.
    let Some(wrapper) = (unsafe { (*wrapper_ptr).this.upgrade() }) else {
        return ptr::null_mut();
    };

    if !wrapper.is_main_thread() {
        crate::nice_error!("AU: the editor view must be created on the main thread");
        return ptr::null_mut();
    }

    // 4. The editor
    let Some(editor) = wrapper.editor.borrow().clone() else {
        return ptr::null_mut();
    };

    // 5. One editor per unit
    if wrapper
        .editor_slot
        .try_borrow()
        .is_ok_and(|slot| slot.is_some())
    {
        crate::nice_warn!("AU: a second editor view was requested; closing the first");
        wrapper.close_editor();
    }

    // 6. The container
    let Some(image) = this_image(make_view::<P> as *const c_void) else {
        crate::nice_error!("AU: could not find the plugin's image");
        return ptr::null_mut();
    };
    let Some((_, container_class)) = classes::<P>(&image) else {
        return ptr::null_mut();
    };

    let editor_size = editor.lock().size();
    let frame = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: editor_size.width as f64,
            height: editor_size.height as f64,
        },
    };
    // SAFETY: `alloc` and `initWithFrame:` are NSView's.
    let container: Option<Retained<AnyObject>> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![container_class, alloc];
        msg_send![allocated, initWithFrame: frame]
    };
    let Some(container) = container else {
        crate::nice_error!("AU: could not create the editor's container view");
        return ptr::null_mut();
    };
    let container_ptr = Retained::as_ptr(&container) as *mut AnyObject;

    // SAFETY: The ivars exist on the container class, with these types.
    unsafe {
        if let (Some(wrapper_ivar), Some(resize_ivar)) = (
            container_ivar::<*const c_void>(&container, c"niceWrapper"),
            container_ivar::<u8>(&container, c"niceEditorResize"),
        ) {
            // The container's dealloc drops this `Weak`
            *wrapper_ivar = Weak::into_raw(Arc::downgrade(&wrapper)) as *const c_void;
            *resize_ivar = 0;
        } else {
            crate::nice_error!("AU: the container view has no instance variables");
            return ptr::null_mut();
        }
    }

    // 7. Spawn the editor into the container
    let Some(parent) = NonNull::new(container_ptr.cast::<c_void>()) else {
        return ptr::null_mut();
    };
    let spawned = editor.lock().spawn(
        Some(ParentWindowHandle::AppKitNsView(parent)),
        false,
        None,
        wrapper.clone().make_gui_context(),
        Some(HostMethods {
            callbacks: Box::new(AuHostCallbacks::<P> {
                wrapper: Arc::downgrade(&wrapper),
            }),
            main_thread_caller: Box::new(AuMainThreadCaller::<P> {
                wrapper: Arc::downgrade(&wrapper),
            }),
        }),
    );
    let spawned = match spawned {
        Ok(spawned) => spawned,
        Err(err) => {
            // Releasing the container runs its dealloc, which finds no slot of its own
            crate::nice_error!("Failed to create editor: {}", err);
            return ptr::null_mut();
        }
    };

    // 8. Show it
    if let Err(err) = spawned.handle.show(&spawned.window) {
        crate::nice_error!("Failed to show editor: {}", err);
        drop(spawned);
        return ptr::null_mut();
    }

    // 9. The slot owns the editor from here on
    let slot = Fragile::new(EditorSlot::<P> {
        container: container_ptr as usize,
        hidden: std::cell::Cell::new(false),
        spawned,
    });
    let previous = match wrapper.editor_slot.try_borrow_mut() {
        Ok(mut current) => current.replace(slot),
        Err(_) => {
            crate::nice_error!("AU: the editor slot is busy, cannot open the editor");
            return ptr::null_mut();
        }
    };
    // Dropped after the borrow is gone, in case dropping it reaches back into the wrapper
    drop(previous);

    // 10. The editor may have asked for a size during spawn or show, before the slot existed
    let now = editor.lock().size();
    if now != editor_size {
        set_container_size_from_editor(container_ptr as usize, now);
    }

    // 11. The host owns the view now
    Retained::autorelease_ptr(container)
}

// ------------------------------------------------------------------------------------------------
// The container class
// ------------------------------------------------------------------------------------------------

extern "C-unwind" fn container_set_frame_size<P: AuPlugin>(
    this: &AnyObject,
    _sel: Sel,
    size: CGSize,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: NSView implements `setFrameSize:` with this signature.
        unsafe {
            let _: () = msg_send![super(this, ns_view_class()), setFrameSize: size];
        }

        // SAFETY: `this` is a container, and the ivars have these types.
        let (wrapper_raw, editor_resize) = unsafe {
            let wrapper = container_ivar::<*const c_void>(this, c"niceWrapper")
                .map_or(ptr::null(), |ivar| *ivar);
            let editor_resize =
                container_ivar::<u8>(this, c"niceEditorResize").map_or(0, |ivar| *ivar);
            (wrapper, editor_resize)
        };
        // `initWithFrame:` can end up here before the instance variable is set
        if editor_resize != 0 {
            return;
        }

        // SAFETY: The pointer is the container's own `Weak`.
        if let Some(wrapper) = unsafe { upgrade_wrapper::<P>(wrapper_raw) } {
            wrapper.container_resized_by_host(
                this as *const AnyObject as usize,
                NativeSize {
                    width: size.width.round().max(0.0) as u32,
                    height: size.height.round().max(0.0) as u32,
                },
            );
        }
    }));

    if result.is_err() {
        crate::nice_error!("AU: panic in the container's setFrameSize:");
    }
}

extern "C-unwind" fn container_view_did_move_to_window<P: AuPlugin>(this: &AnyObject, _sel: Sel) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: NSView implements `viewDidMoveToWindow` with this signature.
        unsafe {
            let _: () = msg_send![super(this, ns_view_class()), viewDidMoveToWindow];
        }

        // SAFETY: `window` is an NSView method returning an object or nil.
        let window: *mut AnyObject = unsafe { msg_send![this, window] };

        // SAFETY: `this` is a container, and the ivar has this type.
        let wrapper_raw = unsafe {
            container_ivar::<*const c_void>(this, c"niceWrapper").map_or(ptr::null(), |ivar| *ivar)
        };
        // SAFETY: The pointer is the container's own `Weak`.
        if let Some(wrapper) = unsafe { upgrade_wrapper::<P>(wrapper_raw) } {
            wrapper.container_moved_to_window(this as *const AnyObject as usize, !window.is_null());
        }
    }));

    if result.is_err() {
        crate::nice_error!("AU: panic in the container's viewDidMoveToWindow");
    }
}

extern "C-unwind" fn container_dealloc<P: AuPlugin>(this: &AnyObject, _sel: Sel) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `this` is a container, and the ivar has this type.
        let Some(ivar) = (unsafe { container_ivar::<*const c_void>(this, c"niceWrapper") }) else {
            return;
        };
        // SAFETY: See above.
        let raw = unsafe { *ivar };
        if raw.is_null() {
            return;
        }

        // The editor goes first if it is still in this container
        // SAFETY: The pointer is the container's own `Weak`.
        if let Some(wrapper) = unsafe { upgrade_wrapper::<P>(raw) } {
            wrapper.close_editor_for(this as *const AnyObject as usize);
        }

        // SAFETY: The pointer came from `Weak::into_raw` and is dropped exactly once: the ivar is
        // cleared right after.
        unsafe {
            drop(Weak::from_raw(raw as *const Wrapper<P>));
            *ivar = ptr::null();
        }
    }));

    if result.is_err() {
        crate::nice_error!("AU: panic in the container's dealloc");
    }

    // SAFETY: NSView (through NSObject) implements `dealloc`.
    unsafe {
        let _: () = msg_send![super(this, ns_view_class()), dealloc];
    }
}

/// Set `container`'s frame size because the editor asked; the container does not forward this one.
pub(super) fn set_container_size_from_editor(container: usize, size: NativeSize<u32>) {
    /// Clears `niceEditorResize` again, even if the resize panics.
    struct ClearOnDrop(*mut u8);
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            // SAFETY: The pointer is the container's instance variable, and the container outlives
            // this guard.
            unsafe { *self.0 = 0 };
        }
    }

    // SAFETY: The editor slot closes before the container is deallocated, so a container the slot
    // knows about is alive.
    let container = unsafe { &*(container as *const AnyObject) };
    // SAFETY: The ivar has this type.
    let Some(flag) = (unsafe { container_ivar::<u8>(container, c"niceEditorResize") }) else {
        return;
    };

    // SAFETY: See above.
    unsafe { *flag = 1 };
    let _guard = ClearOnDrop(flag);
    // SAFETY: `setFrameSize:` is NSView's.
    unsafe {
        let _: () = msg_send![
            container,
            setFrameSize: CGSize {
                width: size.width as f64,
                height: size.height as f64,
            }
        ];
    }
}

// ------------------------------------------------------------------------------------------------
// Host methods
// ------------------------------------------------------------------------------------------------

/// What the editor uses to talk to the host (here: to its own container view).
struct AuHostCallbacks<P: AuPlugin> {
    wrapper: Weak<Wrapper<P>>,
}

impl<P: AuPlugin> HostCallbacks for AuHostCallbacks<P> {
    fn request_resize(&mut self, new_size: Size, scale_factor: f64) -> Result<(), Box<dyn Error>> {
        let Some(wrapper) = self.wrapper.upgrade() else {
            return Err("plugin closed".into());
        };

        // Posted, or executed right away when this is the main thread
        if !wrapper.schedule_gui(Task::RequestResize {
            size: new_size,
            scale_factor,
        }) {
            return Err("failed to post the resize task".into());
        }

        Ok(())
    }

    fn destroyed(&mut self) {
        crate::nice_warn!("AU: the editor window was destroyed; AU cannot tell the host");
    }
}

/// Wakes the main thread for the editor.
struct AuMainThreadCaller<P: AuPlugin> {
    wrapper: Weak<Wrapper<P>>,
}

impl<P: AuPlugin> HostMainThreadCaller for AuMainThreadCaller<P> {
    fn call_main_thread(&mut self) {
        if let Some(wrapper) = self.wrapper.upgrade()
            && !wrapper.schedule_gui(Task::CallMainThread)
        {
            crate::nice_error!("Failed to post Task::CallMainThread");
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The property value
// ------------------------------------------------------------------------------------------------

/// The `CocoaUI` property value, registering the classes first. Both references are +1; the host
/// releases them.
pub(crate) fn cocoa_view_info<P: AuPlugin>() -> Option<AudioUnitCocoaViewInfo> {
    let Some(image) = this_image(cocoa_view_info::<P> as *const c_void) else {
        crate::nice_error!("AU: could not find the plugin's image");
        return None;
    };
    let (factory, _) = classes::<P>(&image)?;

    let directory = bundle_dir_of(&image.path);
    let (Some(directory), Ok(factory_name)) = (directory.to_str(), factory.name().to_str()) else {
        crate::nice_error!("AU: the plugin's path or class name is not valid UTF-8");
        return None;
    };

    let directory = CFString::from_str(directory);
    let Some(url) = CFURL::with_file_system_path(
        None,
        Some(&directory),
        CFURLPathStyle::CFURLPOSIXPathStyle,
        true,
    ) else {
        crate::nice_error!("AU: could not make a URL for the plugin's bundle");
        return None;
    };
    let factory_name = CFString::from_str(factory_name);

    Some(AudioUnitCocoaViewInfo {
        mCocoaAUViewBundleLocation: CFRetained::into_raw(url),
        mCocoaAUViewClass: [CFRetained::into_raw(factory_name)],
    })
}
