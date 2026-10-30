// DELTON FORK DELTA: au-2, AUv2 editor
//! Drives the AUv2 editor through Apple's own host API: the unit's `CocoaUI` property, the view
//! factory class it names, and the container view the factory returns.
//!
//! This test has its own `main` (`harness = false`): AppKit and the wrapper's main-thread tasks
//! need the real main thread, and libtest runs tests on other threads. Runs on macOS only.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(target_os = "macos")]
#[allow(non_upper_case_globals)]
mod macos {
    use std::cell::RefCell;
    use std::error::Error;
    use std::ffi::{c_void, CString};
    use std::mem::{size_of, MaybeUninit};
    use std::path::PathBuf;
    use std::ptr::{self, NonNull};
    use std::sync::{Arc, Once};
    use std::time::{Duration, Instant};

    use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoopGetMain, CFRunLoopRunInMode};
    use nice_plug::context::gui::GuiContext;
    use nice_plug::editor::dpi::{LogicalSize, NativeSize, Size};
    use nice_plug::editor::{
        EditorHandle, HostCallbacks, HostMethods, ParentWindowHandle, SpawnedEditor,
    };
    use nice_plug::plugin::ParamValue;
    use nice_plug::prelude::*;
    use objc2::encode::{Encode, Encoding, RefEncode};
    use objc2::msg_send;
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::{AnyClass, AnyObject, AnyProtocol};
    use objc2_audio_toolbox::{
        kAudioUnitProperty_ClassInfo, kAudioUnitProperty_CocoaUI,
        kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Global, kAudioUnitScope_Input, kAudioUnitScope_Output, AUListenerBase,
        AUListenerDispose, AudioComponent, AudioComponentDescription,
        AudioComponentFactoryFunction, AudioComponentFindNext, AudioComponentInstance,
        AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioComponentPlugInInterface,
        AudioComponentRegister, AudioUnitCocoaViewInfo, AudioUnitEventType, AudioUnitGetParameter,
        AudioUnitGetProperty, AudioUnitGetPropertyInfo, AudioUnitInitialize, AudioUnitParameter,
        AudioUnitProperty, AudioUnitSetParameter, AudioUnitSetProperty, AudioUnitUninitialize,
    };
    use objc2_core_audio_types::{
        kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
        kAudioFormatLinearPCM, AudioStreamBasicDescription,
    };
    use objc2_core_foundation::{CFRetained, CFString, CFType, CFURLPathStyle};

    const AU_EFFECT: u32 = 0x6175_6678; // 'aufx'
    const SUBTYPE: u32 = 0x5476_6577; // 'Tvew'
    const MANUFACTURER: u32 = 0x4E70_6C67; // 'Nplg'

    // --------------------------------------------------------------------------------------------
    // Geometry, with the encodings of Apple's types
    // --------------------------------------------------------------------------------------------

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    unsafe impl Encode for CGSize {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl RefEncode for CGSize {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    unsafe impl Encode for CGPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl RefEncode for CGPoint {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    unsafe impl Encode for CGRect {
        const ENCODING: Encoding =
            Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
    }
    unsafe impl RefEncode for CGRect {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
    }

    // --------------------------------------------------------------------------------------------
    // The test plugin and its editor
    // --------------------------------------------------------------------------------------------

    #[derive(Params)]
    struct TestViewParams {
        #[id = "gain"]
        gain: FloatParam,
        #[id = "mode"]
        mode: IntParam,
    }

    impl Default for TestViewParams {
        fn default() -> Self {
            Self {
                gain: FloatParam::new("Gain", 1.0, FloatRange::Linear { min: 0.0, max: 2.0 }),
                mode: IntParam::new("Mode", 0, IntRange::Linear { min: 0, max: 3 }),
            }
        }
    }

    struct TestView {
        params: Arc<TestViewParams>,
    }

    impl Default for TestView {
        fn default() -> Self {
            Self {
                params: Arc::new(TestViewParams::default()),
            }
        }
    }

    impl Plugin for TestView {
        const NAME: &'static str = "TestView";
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

        type Editor = TestEditor;
        type SysExMessage = ();
        type BackgroundTask = ();

        fn params(&self) -> Arc<dyn Params> {
            self.params.clone()
        }

        fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<TestEditor> {
            Some(TestEditor {
                params: self.params.clone(),
            })
        }

        fn process(
            &mut self,
            buffer: &mut Buffer,
            _aux: &mut AuxiliaryBuffers,
            _context: &mut impl ProcessContext<Self>,
        ) -> ProcessStatus {
            let gain = self.params.gain.value();
            for channel_samples in buffer.iter_samples() {
                for sample in channel_samples {
                    *sample *= gain;
                }
            }

            ProcessStatus::Normal
        }
    }

    impl AuPlugin for TestView {
        const AU_MANUFACTURER: [u8; 4] = *b"Nplg";
        const AU_SUBTYPE: [u8; 4] = *b"Tvew";
    }

    nice_plug::nice_export_auv2!(@unexported TestView);

    /// What the editor saw, for the cases to look at. Everything runs on the main thread.
    struct Recorded {
        spawns: u32,
        drops: u32,
        shows: u32,
        hides: u32,
        state_changes: u32,
        drops_at_last_spawn: u32,
        last_parent: usize,
        size: (u32, u32),
        set_sizes: Vec<(u32, u32)>,
        value_changes: Vec<(String, f32)>,
        context: Option<GuiContext>,
        params: Option<Arc<TestViewParams>>,
        callbacks: Option<Box<dyn HostCallbacks>>,
        resizable: bool,
    }

    impl Recorded {
        fn new() -> Self {
            Self {
                spawns: 0,
                drops: 0,
                shows: 0,
                hides: 0,
                state_changes: 0,
                drops_at_last_spawn: 0,
                last_parent: 0,
                size: (400, 300),
                set_sizes: Vec::new(),
                value_changes: Vec::new(),
                context: None,
                params: None,
                callbacks: None,
                resizable: true,
            }
        }
    }

    thread_local! {
        static RECORDED: RefCell<Recorded> = RefCell::new(Recorded::new());
        /// `(event type, parameter id)` of the events the AU event listener received.
        static EVENTS: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
    }

    fn recorded<R>(f: impl FnOnce(&mut Recorded) -> R) -> R {
        RECORDED.with(|recorded| f(&mut recorded.borrow_mut()))
    }

    fn reset() {
        // The old state is dropped outside the borrow: dropping a context or callbacks is free to
        // reach back into the wrapper
        let old = RECORDED.with(|recorded| recorded.replace(Recorded::new()));
        drop(old);
        EVENTS.with(|events| events.borrow_mut().clear());
    }

    struct TestEditor {
        params: Arc<TestViewParams>,
    }

    struct TestHandle;

    impl Editor for TestEditor {
        type Handle = TestHandle;

        fn spawn(
            &self,
            parent: Option<ParentWindowHandle>,
            _wait_for_parent: bool,
            _fallback_scale_factor: Option<f64>,
            gui_context: GuiContext,
            host: Option<HostMethods>,
        ) -> Result<SpawnedEditor<TestHandle>, Box<dyn Error>> {
            let Some(ParentWindowHandle::AppKitNsView(parent)) = parent else {
                panic!("the editor needs an AppKit parent view");
            };

            recorded(|recorded| {
                recorded.spawns += 1;
                recorded.drops_at_last_spawn = recorded.drops;
                recorded.last_parent = parent.as_ptr() as usize;
                recorded.context = Some(gui_context);
                recorded.params = Some(self.params.clone());
                recorded.callbacks = host.map(|host| host.callbacks);
            });

            Ok(SpawnedEditor {
                handle: TestHandle,
                window: (),
            })
        }

        fn size(&self) -> NativeSize<u32> {
            let (width, height) = recorded(|recorded| recorded.size);
            NativeSize { width, height }
        }

        fn resize_hint(&self) -> ResizeHint {
            ResizeHint {
                can_resize: recorded(|recorded| recorded.resizable),
                ..Default::default()
            }
        }
    }

    impl EditorHandle for TestHandle {
        type Window = ();
        type Error = std::fmt::Error;

        fn run_until_closed(_window: Self::Window) -> Result<(), Self::Error> {
            Ok(())
        }

        fn set_parent(
            &self,
            _parent: ParentWindowHandle,
            _window: &Self::Window,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn show(&self, _window: &Self::Window) -> Result<(), Self::Error> {
            recorded(|recorded| recorded.shows += 1);
            Ok(())
        }

        fn hide(&self, _window: &Self::Window) -> Result<(), Self::Error> {
            recorded(|recorded| recorded.hides += 1);
            Ok(())
        }

        fn set_size(
            &self,
            new_size: NativeSize<u32>,
            _window: &Self::Window,
        ) -> Result<(), Self::Error> {
            recorded(|recorded| {
                recorded.set_sizes.push((new_size.width, new_size.height));
                recorded.size = (new_size.width, new_size.height);
            });
            Ok(())
        }

        fn host_main_thread_callback(&self, _window: &Self::Window) {}

        fn state_changed(&self) {
            recorded(|recorded| recorded.state_changes += 1);
        }

        fn param_value_changed(&self, id: &str, normalized_value: f32) {
            recorded(|recorded| {
                recorded
                    .value_changes
                    .push((id.to_string(), normalized_value))
            });
        }

        fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
    }

    impl Drop for TestHandle {
        fn drop(&mut self) {
            recorded(|recorded| recorded.drops += 1);
        }
    }

    // --------------------------------------------------------------------------------------------
    // The host side
    // --------------------------------------------------------------------------------------------

    /// JUCE's `String::hashCode` with the top bit cleared, which is what the wrapper uses as an AU
    /// parameter ID.
    fn param_id(id: &str) -> u32 {
        id.bytes().fold(0u32, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(byte as u32)
        }) & !(1 << 31)
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

    /// Register our factory with the AudioComponent system, once for the process.
    fn ensure_registered() {
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| unsafe {
            let mut description = description();
            let name = CFString::from_str("Nice: TestView");
            // The macro's factory takes the description as a plain pointer, so the user crate needs
            // no objc2 import; it is the same function to the loader
            let factory: AudioComponentFactoryFunction = Some(std::mem::transmute::<
                unsafe extern "C-unwind" fn(*const c_void) -> *mut c_void,
                unsafe extern "C-unwind" fn(
                    NonNull<AudioComponentDescription>,
                ) -> *mut AudioComponentPlugInInterface,
            >(auv2::factory));
            let component = AudioComponentRegister(
                NonNull::from(&mut description),
                &name,
                0x0001_0000,
                factory,
            );
            assert!(!component.is_null(), "AudioComponentRegister failed");
        });
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

        /// Get a property into a buffer that may hold any bit pattern, and check it for validity
        /// only if the status says it was written.
        fn get_raw<T>(&self, id: u32, scope: u32, element: u32) -> (i32, MaybeUninit<T>) {
            let mut value = MaybeUninit::<T>::zeroed();
            let mut size = size_of::<T>() as u32;
            let status = unsafe {
                AudioUnitGetProperty(
                    self.0,
                    id,
                    scope,
                    element,
                    NonNull::new_unchecked(value.as_mut_ptr().cast::<c_void>()),
                    NonNull::from(&mut size),
                )
            };
            (status, value)
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

        fn set_max_frames(&self, frames: u32) {
            let status = self.set(
                kAudioUnitProperty_MaximumFramesPerSlice,
                kAudioUnitScope_Global,
                0,
                &frames,
            );
            assert_eq!(status, 0, "setting the maximum frames failed");
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

    /// The bundle path and the factory class name of the unit's `CocoaUI` property. The host owns
    /// both references, so they are released here.
    fn view_info(unit: &Unit) -> (String, String) {
        let (status, info) = unit.get_raw::<AudioUnitCocoaViewInfo>(
            kAudioUnitProperty_CocoaUI,
            kAudioUnitScope_Global,
            0,
        );
        assert_eq!(status, 0, "getting the CocoaUI property failed");
        // SAFETY: The status says the wrapper wrote the whole struct.
        let info = unsafe { info.assume_init() };

        // SAFETY: Both are +1 references that this test now owns.
        let (url, class) = unsafe {
            (
                CFRetained::from_raw(info.mCocoaAUViewBundleLocation),
                CFRetained::from_raw(info.mCocoaAUViewClass[0]),
            )
        };
        let path = url
            .file_system_path(CFURLPathStyle::CFURLPOSIXPathStyle)
            .expect("the bundle URL has a path")
            .to_string();

        (path, class.to_string())
    }

    /// Ask the factory class for a view, the way a host does: `[[factory new]
    /// uiViewForAudioUnit:unit withSize:size]`. Returns the view retained (+1).
    fn make_view(unit: &Unit, factory_class: &str) -> *mut AnyObject {
        autoreleasepool(|_| {
            let name = CString::new(factory_class).unwrap();
            let class = AnyClass::get(&name).expect("the factory class is registered");
            // SAFETY: The factory is an NSObject subclass; the selector takes a unit and a size.
            unsafe {
                let factory: Retained<AnyObject> = msg_send![class, new];
                let view: *mut AnyObject = msg_send![
                    &*factory,
                    uiViewForAudioUnit: unit.0 as *mut c_void,
                    withSize: CGSize { width: 0.0, height: 0.0 }
                ];
                // The factory (and with it nothing else) is released when the pool drains
                Retained::retain(view).map_or(ptr::null_mut(), Retained::into_raw)
            }
        })
    }

    fn release(view: *mut AnyObject) {
        // SAFETY: `view` is a +1 reference from `make_view`.
        drop(unsafe { Retained::from_raw(view) });
    }

    fn frame_size(view: *mut AnyObject) -> (f64, f64) {
        // SAFETY: `frame` is an NSView method.
        let frame: CGRect = unsafe { msg_send![view, frame] };
        (frame.size.width, frame.size.height)
    }

    fn class_name_of(view: *mut AnyObject) -> String {
        // SAFETY: `view` is a live object.
        unsafe { &*view }
            .class()
            .name()
            .to_str()
            .unwrap()
            .to_string()
    }

    fn set_frame_size(view: *mut AnyObject, width: f64, height: f64) {
        // SAFETY: `setFrameSize:` is an NSView method.
        let _: () = unsafe { msg_send![view, setFrameSize: CGSize { width, height }] };
    }

    /// Run the main run loop for `seconds`.
    fn spin(seconds: f64) {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < deadline {
            // SAFETY: The default mode is a valid run loop mode.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.01, 0) };
        }
    }

    // --------------------------------------------------------------------------------------------
    // The cases
    // --------------------------------------------------------------------------------------------

    fn cocoa_ui_names_a_registered_view_factory() {
        let unit = Unit::new();

        let mut size = 0u32;
        let mut writable = 1u8;
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
        assert_eq!(status, 0);
        assert_eq!(size, 16);
        assert_eq!(writable, 0);

        let (path, class_name) = view_info(&unit);
        let rest = class_name
            .strip_prefix("NiceAuv2View_NplgTvew_")
            .unwrap_or_else(|| panic!("unexpected class name {class_name}"));
        assert_eq!(rest.len(), 16);
        assert!(rest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));

        // There is no `.component`, so the "bundle" is the directory of the test executable
        let expected = std::env::current_exe().unwrap();
        let expected: PathBuf = expected.parent().unwrap().to_path_buf();
        assert_eq!(
            PathBuf::from(&path).canonicalize().unwrap(),
            expected.canonicalize().unwrap()
        );

        let name = CString::new(class_name.clone()).unwrap();
        let class = AnyClass::get(&name).expect("the factory class is registered");
        let protocol = AnyProtocol::get(c"AUCocoaUIBase").expect("the protocol exists");
        assert!(class.conforms_to(protocol));
        // SAFETY: The factory is an NSObject subclass with `interfaceVersion`.
        let version: u32 = unsafe {
            let factory: Retained<AnyObject> = msg_send![class, new];
            msg_send![&*factory, interfaceVersion]
        };
        assert_eq!(version, 0);

        // Asking again gives the same class
        assert_eq!(view_info(&unit).1, class_name);
    }

    fn view_spawns_the_editor_in_a_sized_container() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);

        assert!(!view.is_null());
        assert!(class_name_of(view).starts_with("NiceAuv2Container_NplgTvew_"));
        assert_eq!(frame_size(view), (400.0, 300.0));
        recorded(|recorded| {
            assert_eq!(recorded.spawns, 1);
            assert_eq!(recorded.shows, 1);
            assert_eq!(recorded.last_parent, view as usize);
            assert_eq!(recorded.drops, 0);
        });

        release(view);
    }

    fn releasing_the_view_drops_the_editor() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());
        recorded(|recorded| assert_eq!(recorded.drops, 0));

        release(view);
        recorded(|recorded| assert_eq!(recorded.drops, 1));
    }

    fn a_second_view_replaces_the_first() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let first = make_view(&unit, &factory);
        let second = make_view(&unit, &factory);
        assert!(!first.is_null() && !second.is_null());

        recorded(|recorded| {
            assert_eq!(recorded.spawns, 2);
            // The first editor was dropped before the second was spawned
            assert_eq!(recorded.drops_at_last_spawn, 1);
        });

        // The first container no longer owns anything
        release(first);
        recorded(|recorded| assert_eq!(recorded.drops, 1));
        release(second);
        recorded(|recorded| assert_eq!(recorded.drops, 2));
    }

    fn disposing_the_unit_drops_an_open_editor() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        drop(unit);
        recorded(|recorded| assert_eq!(recorded.drops, 1));

        // The host may release the view after the unit is gone
        release(view);
        recorded(|recorded| assert_eq!(recorded.drops, 1));
    }

    fn leaving_the_window_hides_once() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        // The view is in no window, so this is the "left the window" call
        let _: () = unsafe { msg_send![view, viewDidMoveToWindow] };
        recorded(|recorded| {
            assert_eq!(recorded.hides, 1);
            assert_eq!(recorded.shows, 1);
        });

        let _: () = unsafe { msg_send![view, viewDidMoveToWindow] };
        recorded(|recorded| {
            assert_eq!(recorded.hides, 1);
            // Showing again needs a hide and a window, and this test cannot make a window
            assert_eq!(recorded.shows, 1);
        });

        release(view);
    }

    fn editor_resize_sets_the_frame_without_echo() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        let mut callbacks = recorded(|recorded| recorded.callbacks.take()).expect("host callbacks");
        callbacks
            .request_resize(
                Size::Logical(LogicalSize {
                    width: 520.0,
                    height: 310.0,
                }),
                1.0,
            )
            .expect("the resize request is accepted");

        assert_eq!(frame_size(view), (520.0, 310.0));
        recorded(|recorded| assert!(recorded.set_sizes.is_empty()));

        release(view);
        drop(callbacks);
    }

    fn host_resize_reaches_a_resizable_editor() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        set_frame_size(view, 500.0, 350.0);
        recorded(|recorded| assert_eq!(recorded.set_sizes, vec![(500, 350)]));

        recorded(|recorded| recorded.resizable = false);
        set_frame_size(view, 600.0, 400.0);
        recorded(|recorded| assert_eq!(recorded.set_sizes, vec![(500, 350)]));

        release(view);
    }

    // The AU event listener API, which the generated bindings do not cover
    #[repr(C)]
    #[derive(Clone, Copy)]
    union AuEventArgument {
        parameter: AudioUnitParameter,
        property: AudioUnitProperty,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AuEvent {
        event_type: AudioUnitEventType,
        argument: AuEventArgument,
    }

    type AuEventListenerProc =
        unsafe extern "C-unwind" fn(*mut c_void, *mut c_void, *const AuEvent, u64, f32);

    unsafe extern "C-unwind" {
        fn AUEventListenerCreate(
            proc_: AuEventListenerProc,
            user_data: *mut c_void,
            run_loop: *const c_void,
            run_loop_mode: *const c_void,
            notification_interval: f32,
            value_change_granularity: f32,
            out_listener: *mut *mut AUListenerBase,
        ) -> i32;

        fn AUEventListenerAddEventType(
            listener: *mut AUListenerBase,
            object: *mut c_void,
            event: *const AuEvent,
        ) -> i32;
    }

    unsafe extern "C-unwind" fn on_event(
        _user_data: *mut c_void,
        _object: *mut c_void,
        event: *const AuEvent,
        _host_time: u64,
        _value: f32,
    ) {
        // SAFETY: The listener passes a valid event, and these are parameter events.
        let (event_type, id) = unsafe {
            let event = &*event;
            (event.event_type.0, event.argument.parameter.mParameterID)
        };
        EVENTS.with(|events| events.borrow_mut().push((event_type, id)));
    }

    fn gui_gestures_reach_au_event_listeners() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        let gain = param_id("gain");
        let mut listener: *mut AUListenerBase = ptr::null_mut();
        // SAFETY: The listener is created on the main run loop and disposed below.
        let status = unsafe {
            AUEventListenerCreate(
                on_event,
                ptr::null_mut(),
                CFRunLoopGetMain() as *const c_void,
                kCFRunLoopDefaultMode as *const c_void,
                0.0,
                0.0,
                &mut listener,
            )
        };
        assert_eq!(status, 0, "AUEventListenerCreate failed");

        for event_type in [
            AudioUnitEventType::BeginParameterChangeGesture,
            AudioUnitEventType::ParameterValueChange,
            AudioUnitEventType::EndParameterChangeGesture,
        ] {
            let event = AuEvent {
                event_type,
                argument: AuEventArgument {
                    parameter: AudioUnitParameter {
                        mAudioUnit: unit.0,
                        mParameterID: gain,
                        mScope: kAudioUnitScope_Global,
                        mElement: 0,
                    },
                },
            };
            // SAFETY: The listener is alive and the event is valid.
            let status = unsafe { AUEventListenerAddEventType(listener, ptr::null_mut(), &event) };
            assert_eq!(status, 0, "AUEventListenerAddEventType failed");
        }

        let context = recorded(|recorded| recorded.context.clone()).expect("the GUI context");
        let params = recorded(|recorded| recorded.params.clone()).expect("the parameters");
        let setter = context.param_setter();
        setter.begin_set_parameter(&params.gain);
        setter.set_parameter(&params.gain, 1.5);
        setter.end_set_parameter(&params.gain);

        let end = AudioUnitEventType::EndParameterChangeGesture.0;
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline
            && !EVENTS.with(|events| events.borrow().iter().any(|&(kind, _)| kind == end))
        {
            spin(0.01);
        }

        let mut received: Vec<(u32, u32)> = EVENTS.with(|events| events.borrow().clone());
        received.dedup();
        assert_eq!(
            received,
            vec![
                (AudioUnitEventType::BeginParameterChangeGesture.0, gain),
                (AudioUnitEventType::ParameterValueChange.0, gain),
                (AudioUnitEventType::EndParameterChangeGesture.0, gain),
            ]
        );
        assert_eq!(unit.parameter("gain"), 0.75);
        recorded(|recorded| {
            assert!(recorded
                .value_changes
                .iter()
                .any(|(id, value)| id == "gain" && *value == 0.75))
        });

        // SAFETY: The listener is alive and not used again.
        unsafe { AUListenerDispose(listener) };
        drop(context);
        release(view);
    }

    fn host_parameter_change_reaches_the_editor() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        unit.set_parameter("gain", 0.25);
        recorded(|recorded| {
            assert_eq!(
                recorded.value_changes.last(),
                Some(&("gain".to_string(), 0.25))
            )
        });

        release(view);
    }

    fn state_restore_tells_the_editor() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        let (status, class_info) =
            unit.get_raw::<*const c_void>(kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0);
        assert_eq!(status, 0);
        // SAFETY: The status says it was written, and the caller owns a reference.
        let class_info = unsafe { class_info.assume_init() };
        let class_info =
            unsafe { CFRetained::from_raw(NonNull::new(class_info as *mut CFType).unwrap()) };

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
        recorded(|recorded| assert_eq!(recorded.state_changes, 1));

        release(view);
    }

    fn gui_set_state_applies_when_not_rendering() {
        let unit = Unit::new();
        let (_, factory) = view_info(&unit);
        let view = make_view(&unit, &factory);
        assert!(!view.is_null());

        unit.set_format(2, 44100.0);
        unit.set_max_frames(512);
        // Nothing renders after this: the unit is initialized but idle, like a stopped track
        assert_eq!(unsafe { AudioUnitInitialize(unit.0) }, 0);

        let context = recorded(|recorded| recorded.context.clone()).expect("the GUI context");
        let mut state = context.get_state();
        state.params.insert("gain".into(), ParamValue::F32(0.5));

        let started = Instant::now();
        context.set_state(state);
        // The wait for the render thread times out after 100 ms
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(unit.parameter("gain"), 0.25);
        recorded(|recorded| assert_eq!(recorded.state_changes, 1));

        assert_eq!(unsafe { AudioUnitUninitialize(unit.0) }, 0);
        let mut state = context.get_state();
        state.params.insert("gain".into(), ParamValue::F32(2.0));
        let started = Instant::now();
        context.set_state(state);
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(unit.parameter("gain"), 1.0);

        drop(context);
        release(view);
    }

    // --------------------------------------------------------------------------------------------
    // The main
    // --------------------------------------------------------------------------------------------

    pub(super) fn main() {
        // SAFETY: `sharedApplication` creates the application object AppKit needs.
        let _: *mut AnyObject =
            unsafe { msg_send![objc2::class!(NSApplication), sharedApplication] };

        let cases: &[(&str, fn())] = &[
            (
                "cocoa_ui_names_a_registered_view_factory",
                cocoa_ui_names_a_registered_view_factory,
            ),
            (
                "view_spawns_the_editor_in_a_sized_container",
                view_spawns_the_editor_in_a_sized_container,
            ),
            (
                "releasing_the_view_drops_the_editor",
                releasing_the_view_drops_the_editor,
            ),
            (
                "a_second_view_replaces_the_first",
                a_second_view_replaces_the_first,
            ),
            (
                "disposing_the_unit_drops_an_open_editor",
                disposing_the_unit_drops_an_open_editor,
            ),
            (
                "leaving_the_window_hides_once",
                leaving_the_window_hides_once,
            ),
            (
                "editor_resize_sets_the_frame_without_echo",
                editor_resize_sets_the_frame_without_echo,
            ),
            (
                "host_resize_reaches_a_resizable_editor",
                host_resize_reaches_a_resizable_editor,
            ),
            (
                "gui_gestures_reach_au_event_listeners",
                gui_gestures_reach_au_event_listeners,
            ),
            (
                "host_parameter_change_reaches_the_editor",
                host_parameter_change_reaches_the_editor,
            ),
            (
                "state_restore_tells_the_editor",
                state_restore_tells_the_editor,
            ),
            (
                "gui_set_state_applies_when_not_rendering",
                gui_set_state_applies_when_not_rendering,
            ),
        ];

        let mut passed = 0;
        for (name, case) in cases {
            reset();
            case();
            println!("test {name} ... ok");
            passed += 1;
        }
        reset();
        println!("auv2_view: {passed} passed");
    }
}
