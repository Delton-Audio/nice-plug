// DELTON FORK DELTA: hpre-4, AU factory presets on the HostPresets program list
//! Drives the AUv2 wrapper's `FactoryPresets` and `PresentPreset` through Apple's own host API
//! (`AudioComponentRegister`, `AudioUnitGetProperty`, `AudioUnitSetProperty`,
//! `AudioUnitAddPropertyListener`), as a DAW does, with a test plugin whose `HostPresets` is a list
//! behind a mutex that records every call.
//!
//! This test has its own `main` (`harness = false`), like `auv2_view`: the wrapper's property
//! notices are delivered by the main run loop, and the main-thread paths (a load run inline, a
//! re-read of the list) are taken only on the real main thread, which libtest does not run tests
//! on. Runs on macOS only; the cases run one after another and a failed assertion fails the run.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(target_os = "macos")]
#[allow(non_upper_case_globals)]
mod macos {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::path::Path;
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, Once};
    use std::time::{Duration, Instant};

    use core_foundation::runloop::{CFRunLoopRunInMode, kCFRunLoopDefaultMode};
    use nice_plug::prelude::*;
    use nice_plug::wrapper::auv2::live_preset_blocks;
    use objc2_audio_toolbox::{
        AUPreset, AudioComponent, AudioComponentDescription, AudioComponentFactoryFunction,
        AudioComponentFindNext, AudioComponentInstance, AudioComponentInstanceDispose,
        AudioComponentInstanceNew, AudioComponentPlugInInterface, AudioComponentRegister,
        AudioUnit, AudioUnitAddPropertyListener, AudioUnitGetProperty, AudioUnitSetProperty,
        AudioUnitUninitialize, kAudioUnitErr_InvalidPropertyValue, kAudioUnitProperty_ClassInfo,
        kAudioUnitProperty_FactoryPresets, kAudioUnitProperty_PresentPreset,
        kAudioUnitScope_Global,
    };
    use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};

    const AU_EFFECT: u32 = 0x6175_6678; // 'aufx'
    const SUBTYPE_PRESETS: u32 = 0x5470_7265; // 'Tpre'
    const SUBTYPE_PLAIN: u32 = 0x5470_6C6E; // 'Tpln'
    const MANUFACTURER: u32 = 0x4E70_6C67; // 'Nplg'

    // --------------------------------------------------------------------------------------------
    // The test plugins
    // --------------------------------------------------------------------------------------------

    /// One call the wrapper made to the plugin's `HostPresets`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        LoadProgram(usize, u64),
        ProgramChange(usize),
    }

    /// The plugin's presets: a list the test can change, the current program, and every call.
    struct TestPresets {
        list: Mutex<ProgramList>,
        current: Mutex<Option<usize>>,
        calls: Mutex<Vec<Call>>,
        fail_loads: AtomicBool,
    }

    impl TestPresets {
        fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
            mutex.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Back to a list at `revision` with `names`, no current program, no calls, loads working.
        fn reset(&self, revision: u64, names: &[&str]) {
            self.set_list(revision, names);
            *Self::lock(&self.current) = None;
            Self::lock(&self.calls).clear();
            self.fail_loads.store(false, Ordering::SeqCst);
        }

        fn set_list(&self, revision: u64, names: &[&str]) {
            *Self::lock(&self.list) = ProgramList {
                revision,
                names: names.iter().map(|name| name.to_string()).collect(),
            };
        }

        fn calls(&self) -> Vec<Call> {
            Self::lock(&self.calls).clone()
        }
    }

    impl HostPresets for TestPresets {
        fn load_file(&self, _path: &Path) -> Result<(), String> {
            Err("the test plugin loads no files".into())
        }

        fn program_list(&self) -> ProgramList {
            Self::lock(&self.list).clone()
        }

        fn load_program(&self, index: usize, revision: u64) -> Result<(), String> {
            Self::lock(&self.calls).push(Call::LoadProgram(index, revision));
            if self.fail_loads.load(Ordering::SeqCst) {
                return Err("the test plugin refuses".into());
            }
            let list = Self::lock(&self.list);
            if revision != list.revision || index >= list.names.len() {
                return Err("stale".into());
            }
            *Self::lock(&self.current) = Some(index);
            Ok(())
        }

        fn current_program(&self) -> Option<usize> {
            *Self::lock(&self.current)
        }

        fn midi_program_change(&self) -> bool {
            false
        }

        /// The plugin loads a program itself: the current program follows.
        fn program_change(&self, index: usize) {
            Self::lock(&self.calls).push(Call::ProgramChange(index));
            *Self::lock(&self.current) = Some(index);
        }
    }

    fn presets() -> Arc<TestPresets> {
        static PRESETS: Mutex<Option<Arc<TestPresets>>> = Mutex::new(None);
        let mut slot = PRESETS.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert_with(|| {
            Arc::new(TestPresets {
                list: Mutex::new(ProgramList::default()),
                current: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                fail_loads: AtomicBool::new(false),
            })
        })
        .clone()
    }

    /// The latest instance's `HostNotices`, for the cases that send notices from the plugin.
    static NOTICES: Mutex<Option<HostNotices>> = Mutex::new(None);

    fn notices() -> HostNotices {
        NOTICES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("an instance attached its notices")
    }

    #[derive(Params, Default)]
    struct TestParams {}

    /// `PRESETS` is whether the plugin hands the wrapper a `HostPresets`.
    struct TestPlugin<const PRESETS: bool> {
        params: Arc<TestParams>,
    }

    impl<const PRESETS: bool> Default for TestPlugin<PRESETS> {
        fn default() -> Self {
            Self {
                params: Arc::new(TestParams::default()),
            }
        }
    }

    impl<const PRESETS: bool> Plugin for TestPlugin<PRESETS> {
        const NAME: &'static str = "TestPresets";
        const VENDOR: &'static str = "Nice";
        const URL: &'static str = "https://example.com";
        const EMAIL: &'static str = "info@example.com";
        const VERSION: &'static str = "1.0.0";

        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        }];

        type Editor = ();
        type SysExMessage = ();
        type BackgroundTask = ();

        fn params(&self) -> Arc<dyn Params> {
            self.params.clone()
        }

        fn attach_host_notices(&mut self, notices: HostNotices) {
            *NOTICES.lock().unwrap_or_else(|e| e.into_inner()) = Some(notices);
        }

        fn host_presets(&self) -> Option<Arc<dyn HostPresets>> {
            PRESETS.then(|| presets() as Arc<dyn HostPresets>)
        }

        fn process(
            &mut self,
            _buffer: &mut Buffer,
            _aux: &mut AuxiliaryBuffers,
            _context: &mut impl ProcessContext<Self>,
        ) -> ProcessStatus {
            ProcessStatus::Normal
        }
    }

    impl<const PRESETS: bool> AuPlugin for TestPlugin<PRESETS> {
        const AU_MANUFACTURER: [u8; 4] = *b"Nplg";
        const AU_SUBTYPE: [u8; 4] = if PRESETS { *b"Tpre" } else { *b"Tpln" };
    }

    // `nice_export_auv2!` defines a module `auv2`, so each plugin gets a module of its own
    mod with_presets {
        use super::*;
        nice_plug::nice_export_auv2!(@unexported TestPlugin<true>);

        pub(super) fn factory() -> unsafe extern "C-unwind" fn(*const c_void) -> *mut c_void {
            auv2::factory
        }
    }

    mod plain {
        use super::*;
        nice_plug::nice_export_auv2!(@unexported TestPlugin<false>);

        pub(super) fn factory() -> unsafe extern "C-unwind" fn(*const c_void) -> *mut c_void {
            auv2::factory
        }
    }

    // --------------------------------------------------------------------------------------------
    // The host side
    // --------------------------------------------------------------------------------------------

    #[derive(Clone, Copy)]
    enum Kind {
        /// The plugin has `HostPresets`.
        WithPresets,
        /// The plugin has none.
        Plain,
    }

    fn description(kind: Kind) -> AudioComponentDescription {
        AudioComponentDescription {
            componentType: AU_EFFECT,
            componentSubType: match kind {
                Kind::WithPresets => SUBTYPE_PRESETS,
                Kind::Plain => SUBTYPE_PLAIN,
            },
            componentManufacturer: MANUFACTURER,
            componentFlags: 0,
            componentFlagsMask: 0,
        }
    }

    /// Register a factory with the AudioComponent system, once per kind for the process.
    fn ensure_registered(kind: Kind) {
        static WITH_PRESETS: Once = Once::new();
        static PLAIN: Once = Once::new();
        let (once, name, factory) = match kind {
            Kind::WithPresets => (&WITH_PRESETS, "Nice: TestPresets", with_presets::factory()),
            Kind::Plain => (&PLAIN, "Nice: TestPlain", plain::factory()),
        };
        once.call_once(|| unsafe {
            let mut description = description(kind);
            let name = CFString::from_str(name);
            // The macro's factory takes the description as a plain pointer, so the user crate needs
            // no objc2 import; it is the same function to the loader
            let factory: AudioComponentFactoryFunction = Some(std::mem::transmute::<
                unsafe extern "C-unwind" fn(*const c_void) -> *mut c_void,
                unsafe extern "C-unwind" fn(
                    NonNull<AudioComponentDescription>,
                ) -> *mut AudioComponentPlugInInterface,
            >(factory));
            let component = AudioComponentRegister(
                NonNull::from(&mut description),
                &name,
                0x0001_0000,
                factory,
            );
            assert!(!component.is_null(), "AudioComponentRegister failed");
        });
    }

    /// An instance of a test plugin, disposed on drop.
    struct Unit(AudioComponentInstance);

    impl Unit {
        fn new(kind: Kind) -> Self {
            ensure_registered(kind);
            unsafe {
                let mut description = description(kind);
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

        fn set<T>(&self, id: u32, value: &T) -> i32 {
            unsafe {
                AudioUnitSetProperty(
                    self.0,
                    id,
                    kAudioUnitScope_Global,
                    0,
                    value as *const T as *const c_void,
                    size_of::<T>() as u32,
                )
            }
        }

        /// Get a property whose all-zero bit pattern is a valid value.
        fn get<T: Copy>(&self, id: u32) -> (i32, T) {
            unsafe {
                let mut value: T = zeroed();
                let mut size = size_of::<T>() as u32;
                let status = AudioUnitGetProperty(
                    self.0,
                    id,
                    kAudioUnitScope_Global,
                    0,
                    NonNull::from(&mut value).cast(),
                    NonNull::from(&mut size),
                );
                (status, value)
            }
        }

        /// `FactoryPresets`: a `CFArray` the caller owns (+1) of `*const AUPreset`.
        fn factory_presets(&self) -> CFRetained<CFArray> {
            let (status, raw) = self.get::<*const c_void>(kAudioUnitProperty_FactoryPresets);
            assert_eq!(status, 0, "getting FactoryPresets failed");
            let raw = NonNull::new(raw as *mut CFArray).expect("FactoryPresets gave no array");
            // SAFETY: A +1 CFArray the wrapper handed over.
            unsafe { CFRetained::from_raw(raw) }
        }

        /// `PresentPreset` set to `(number, name)`: the status.
        fn set_present(&self, number: i32, name: &str) -> i32 {
            let name = CFString::from_str(name);
            let preset = AUPreset {
                presetNumber: number,
                presetName: CFRetained::as_ptr(&name).as_ptr(),
            };
            self.set(kAudioUnitProperty_PresentPreset, &preset)
        }

        /// `PresentPreset` get: `(number, name)`.
        fn present(&self) -> (i32, String) {
            let (status, preset) = self.get::<AUPreset>(kAudioUnitProperty_PresentPreset);
            assert_eq!(status, 0, "getting PresentPreset failed");
            let name = NonNull::new(preset.presetName as *mut CFString)
                .expect("PresentPreset gave no name");
            // SAFETY: The name is a +1 CFString the wrapper handed over.
            let name = unsafe { CFRetained::from_raw(name) };
            (preset.presetNumber, name.to_string())
        }

        fn add_listener(&self, id: u32, counter: &'static AtomicU32) {
            let status = unsafe {
                AudioUnitAddPropertyListener(
                    self.0,
                    id,
                    Some(property_changed),
                    counter as *const AtomicU32 as *mut c_void,
                )
            };
            assert_eq!(status, 0, "AudioUnitAddPropertyListener failed");
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

    /// A unit handle the cases hand to a thread that is not the main one. A host may call the
    /// property functions from any thread.
    struct SendUnit(AudioUnit);
    // SAFETY: An `AudioUnit` is an opaque handle that Apple's API accepts from any thread.
    unsafe impl Send for SendUnit {}

    /// Counts the notices of the property the counter was registered for; never panics (it is
    /// called from inside AudioToolbox).
    unsafe extern "C-unwind" fn property_changed(
        refcon: NonNull<c_void>,
        _unit: AudioUnit,
        _id: u32,
        _scope: u32,
        _element: u32,
    ) {
        unsafe { &*(refcon.as_ptr() as *const AtomicU32) }.fetch_add(1, Ordering::SeqCst);
    }

    /// Run the main run loop for `seconds`, so the wrapper's posted tasks run.
    fn spin(seconds: f64) {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < deadline {
            // SAFETY: The default mode is a valid run loop mode.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.01, 0) };
        }
    }

    /// `FactoryPresets` entries as `(presetNumber, name)`, read through the `AUPreset` pointers.
    fn entries(array: &CFArray) -> Vec<(i32, String)> {
        (0..array.count())
            .map(|index| {
                // SAFETY: Every value is a `*const AUPreset` whose name is a live CFString.
                let preset = unsafe { &*(array.value_at_index(index) as *const AUPreset) };
                let name = unsafe { &*preset.presetName }.to_string();
                (preset.presetNumber, name)
            })
            .collect()
    }

    fn expected(names: &[&str]) -> Vec<(i32, String)> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| (index as i32, name.to_string()))
            .collect()
    }

    const LIST: [&str; 3] = ["Bank / A", "B", "C"];

    /// A fresh unit over `LIST` at `revision`, with the list read the way a host reads it first.
    fn unit_over_list(revision: u64) -> Unit {
        presets().reset(revision, &LIST);
        let unit = Unit::new(Kind::WithPresets);
        drop(unit.factory_presets());
        unit
    }

    // --------------------------------------------------------------------------------------------
    // The cases
    // --------------------------------------------------------------------------------------------

    /// P1: the array has the list's numbers and names, one reference, and releasing it frees the
    /// entries. The entries are counted by `live_preset_blocks()`.
    fn p1_factory_presets_list() {
        presets().reset(5, &LIST);
        let unit = Unit::new(Kind::WithPresets);
        let before = live_preset_blocks();

        let array = unit.factory_presets();
        assert_eq!(array.count(), 3);
        assert_eq!(entries(&array), expected(&LIST));
        assert_eq!(array.retain_count(), 1, "the caller owns the one reference");
        assert_eq!(live_preset_blocks(), before + 3, "one block per entry");

        drop(array);
        assert_eq!(
            live_preset_blocks(),
            before,
            "releasing the array frees the entries"
        );

        // An empty list is an empty array
        presets().set_list(6, &[]);
        let array = unit.factory_presets();
        assert_eq!(array.count(), 0);
        assert_eq!(live_preset_blocks(), before);
        spin(0.05);
    }

    /// P2: an array the host still holds keeps its names after the list changed and a new array
    /// was read.
    fn p2_array_outlives_the_list() {
        let unit = unit_over_list(1);
        let before = live_preset_blocks();
        let old = unit.factory_presets();
        assert_eq!(live_preset_blocks(), before + 3);

        presets().set_list(2, &["X", "Y"]);
        let new = unit.factory_presets();
        assert_eq!(entries(&new), expected(&["X", "Y"]));
        // The old array is read AFTER the change
        assert_eq!(entries(&old), expected(&LIST));
        assert_eq!(live_preset_blocks(), before + 5);

        drop(old);
        assert_eq!(
            live_preset_blocks(),
            before + 2,
            "only the old entries went"
        );
        assert_eq!(entries(&new), expected(&["X", "Y"]));
        drop(new);
        assert_eq!(live_preset_blocks(), before);
        spin(0.05);
    }

    /// P3: setting `{k, _}` loads program k of the list the host read, and reads back as k.
    fn p3_set_loads_the_program() {
        let unit = unit_over_list(4);
        assert_eq!(unit.set_present(1, "ignored"), 0);
        assert_eq!(presets().calls(), vec![Call::LoadProgram(1, 4)]);
        assert_eq!(unit.present(), (1, "B".to_string()));

        // From a thread that is not the main one the load is posted, and runs on the main thread
        presets().reset(7, &LIST);
        let unit = Unit::new(Kind::WithPresets);
        drop(unit.factory_presets());
        let sent = SendUnit(unit.0);
        std::thread::spawn(move || {
            let sent = sent;
            let name = CFString::from_str("ignored");
            let preset = AUPreset {
                presetNumber: 2,
                presetName: CFRetained::as_ptr(&name).as_ptr(),
            };
            let status = unsafe {
                AudioUnitSetProperty(
                    sent.0,
                    kAudioUnitProperty_PresentPreset,
                    kAudioUnitScope_Global,
                    0,
                    &preset as *const AUPreset as *const c_void,
                    size_of::<AUPreset>() as u32,
                )
            };
            assert_eq!(status, 0);
        })
        .join()
        .unwrap();
        spin(0.1);
        assert_eq!(presets().calls(), vec![Call::LoadProgram(2, 7)]);
        assert_eq!(unit.present(), (2, "C".to_string()));
    }

    /// P4: `{-1, "Mine"}` loads nothing and reads back, until the plugin's program moves.
    fn p4_host_user_preset() {
        let unit = unit_over_list(1);
        assert_eq!(unit.set_present(-1, "Mine"), 0);
        assert_eq!(presets().calls(), vec![]);
        assert_eq!(unit.present(), (-1, "Mine".to_string()));

        // The plugin loads program 2 itself: the live program wins
        presets().program_change(2);
        assert_eq!(unit.present(), (2, "C".to_string()));
        spin(0.05);
    }

    /// P5: a number past the list, and a load the plugin refuses, are
    /// `InvalidPropertyValue` and change nothing.
    fn p5_refusals() {
        let unit = unit_over_list(1);
        assert_eq!(unit.present(), (-1, "Untitled".to_string()));

        assert_eq!(unit.set_present(3, "x"), kAudioUnitErr_InvalidPropertyValue);
        assert_eq!(unit.present(), (-1, "Untitled".to_string()));
        assert_eq!(
            presets().calls(),
            vec![],
            "a number past the list is not even tried"
        );

        presets().fail_loads.store(true, Ordering::SeqCst);
        assert_eq!(unit.set_present(1, "x"), kAudioUnitErr_InvalidPropertyValue);
        assert_eq!(unit.present(), (-1, "Untitled".to_string()));
        assert_eq!(presets().calls(), vec![Call::LoadProgram(1, 1)]);

        // The plugin's list moved since the host read it: the stale load is refused, and the
        // host's old index still means what it meant
        presets().fail_loads.store(false, Ordering::SeqCst);
        presets().set_list(2, &["X", "A", "B"]);
        assert_eq!(unit.set_present(0, "x"), kAudioUnitErr_InvalidPropertyValue);
        assert_eq!(
            presets().calls().last(),
            Some(&Call::LoadProgram(0, 1)),
            "the load names the revision the host read"
        );
        assert_eq!(unit.present(), (-1, "Untitled".to_string()));
        spin(0.1);
    }

    static FACTORY_NOTICES: AtomicU32 = AtomicU32::new(0);
    static PRESENT_NOTICES: AtomicU32 = AtomicU32::new(0);

    /// P6: two `program_list_changed` notices from a thread that is not the main one reach a
    /// `FactoryPresets` listener once, and a new get shows the new list.
    fn p6_factory_presets_listener() {
        let unit = unit_over_list(1);
        FACTORY_NOTICES.store(0, Ordering::SeqCst);
        unit.add_listener(kAudioUnitProperty_FactoryPresets, &FACTORY_NOTICES);

        presets().set_list(2, &["X", "Y"]);
        let notices = notices();
        std::thread::spawn(move || {
            notices.program_list_changed();
            notices.program_list_changed();
        })
        .join()
        .unwrap();
        // Nothing runs until the main run loop does
        assert_eq!(FACTORY_NOTICES.load(Ordering::SeqCst), 0);
        spin(0.2);
        assert_eq!(FACTORY_NOTICES.load(Ordering::SeqCst), 1);

        let array = unit.factory_presets();
        assert_eq!(entries(&array), expected(&["X", "Y"]));
        spin(0.1);
        assert_eq!(
            FACTORY_NOTICES.load(Ordering::SeqCst),
            1,
            "the host's own re-read of an unchanged list is not a change"
        );
    }

    /// P7: the plugin's own `preset_loaded` notifies a `PresentPreset` listener once; a host load
    /// does not.
    fn p7_present_preset_listener() {
        let unit = unit_over_list(1);
        PRESENT_NOTICES.store(0, Ordering::SeqCst);
        unit.add_listener(kAudioUnitProperty_PresentPreset, &PRESENT_NOTICES);

        assert_eq!(unit.set_present(1, "x"), 0);
        spin(0.2);
        assert_eq!(PRESENT_NOTICES.load(Ordering::SeqCst), 0, "the host asked");

        let notices = notices();
        std::thread::spawn(move || {
            notices.preset_loaded(Path::new("a-preset-file"));
            notices.preset_loaded(Path::new("a-preset-file"));
        })
        .join()
        .unwrap();
        spin(0.2);
        assert_eq!(PRESENT_NOTICES.load(Ordering::SeqCst), 1);

        assert_eq!(unit.set_present(2, "x"), 0);
        spin(0.2);
        assert_eq!(PRESENT_NOTICES.load(Ordering::SeqCst), 1);
    }

    /// P8: no `HostPresets`: no factory presets, and `PresentPreset` keeps the name only.
    fn p8_no_host_presets() {
        let unit = Unit::new(Kind::Plain);
        let array = unit.factory_presets();
        assert_eq!(array.count(), 0);

        assert_eq!(unit.present(), (-1, "Untitled".to_string()));
        assert_eq!(unit.set_present(-1, "Mine"), 0);
        assert_eq!(unit.present(), (-1, "Mine".to_string()));
        assert_eq!(unit.set_present(0, "x"), kAudioUnitErr_InvalidPropertyValue);
        assert_eq!(unit.present(), (-1, "Mine".to_string()));
    }

    /// P9: the present preset's name survives a ClassInfo round trip.
    fn p9_class_info_keeps_the_name() {
        let unit = unit_over_list(1);
        assert_eq!(unit.set_present(-1, "Mine"), 0);

        let (status, class_info) = unit.get::<*const c_void>(kAudioUnitProperty_ClassInfo);
        assert_eq!(status, 0);
        // The caller owns a reference
        let class_info =
            unsafe { CFRetained::from_raw(NonNull::new(class_info as *mut CFType).unwrap()) };

        assert_eq!(unit.set_present(-1, "Other"), 0);
        assert_eq!(unit.present(), (-1, "Other".to_string()));

        let plist: *const c_void = CFRetained::as_ptr(&class_info).as_ptr() as *const c_void;
        assert_eq!(unit.set(kAudioUnitProperty_ClassInfo, &plist), 0);
        assert_eq!(unit.present(), (-1, "Mine".to_string()));
        spin(0.05);
    }

    pub fn main() {
        let cases: [(&str, fn()); 9] = [
            ("p1_factory_presets_list", p1_factory_presets_list),
            ("p2_array_outlives_the_list", p2_array_outlives_the_list),
            ("p3_set_loads_the_program", p3_set_loads_the_program),
            ("p4_host_user_preset", p4_host_user_preset),
            ("p5_refusals", p5_refusals),
            ("p6_factory_presets_listener", p6_factory_presets_listener),
            ("p7_present_preset_listener", p7_present_preset_listener),
            ("p8_no_host_presets", p8_no_host_presets),
            ("p9_class_info_keeps_the_name", p9_class_info_keeps_the_name),
        ];
        for (name, case) in cases {
            print!("{name} ... ");
            case();
            println!("ok");
        }
        println!("\ntest result: ok. {} passed", cases.len());
    }
}
