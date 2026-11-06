// DELTON FORK DELTA: hpre-4, AU factory presets on the HostPresets program list
//! The AU preset decisions: what `FactoryPresets` lists, what setting and reading `PresentPreset`
//! mean, and the program list cache they share. Everything that decides something is here and
//! takes the plugin's [`HostPresets`] as an argument, so it builds and tests on every OS; the Apple
//! calls (the CFArray of `AUPreset`s) are a small macOS-only part at the bottom.
//!
//! The list is cached because the host's `presetNumber` is an index into the list it READ (from
//! `FactoryPresets`), and [`HostPresets::load_program()`] refuses a load whose revision is not the
//! plugin's current one. A set therefore decides on the cached list and never re-reads it: a
//! re-read at that point would hand the host's old index a new revision and load the wrong preset.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use nice_plug_core::host_presets::{HostPresets, ProgramList};
use parking_lot::Mutex;

/// What a `PresentPreset` set does.
// DELTON FORK DELTA: hpre-4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetSet {
    /// presetNumber >= 0 inside the cached list: load program `index` of the list at `revision`.
    LoadProgram { index: usize, revision: u64 },
    /// presetNumber >= 0 with no list, or past it: refuse (`kAudioUnitErr_InvalidPropertyValue`).
    Refuse,
    /// presetNumber < 0: the host's own user preset. Keep the name only; load nothing.
    HostUserPreset,
}

/// What setting `PresentPreset` to `number` means, given the list the host read (the cached one).
// DELTON FORK DELTA: hpre-4
pub fn preset_set(number: i32, list: Option<&ProgramList>) -> PresetSet {
    let Ok(index) = usize::try_from(number) else {
        return PresetSet::HostUserPreset;
    };
    match list {
        Some(list) if index < list.names.len() => PresetSet::LoadProgram {
            index,
            revision: list.revision,
        },
        Some(_) | None => PresetSet::Refuse,
    }
}

/// What the wrapper remembers about the present preset.
// DELTON FORK DELTA: hpre-4
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentPreset {
    /// The number last set or loaded: an index, or -1.
    pub number: i32,
    pub name: String,
    /// For a host user preset (-1): `current_program()` at the time of the set. While the live
    /// current program still equals it, the host's preset is what reads back.
    pub program_at_set: Option<usize>,
}

impl Default for PresentPreset {
    fn default() -> Self {
        Self {
            number: -1,
            name: String::from("Untitled"),
            program_at_set: None,
        }
    }
}

/// What a `PresentPreset` get returns: (presetNumber, name).
/// - a host user preset (`number < 0`) whose `program_at_set` equals `live`: (-1, its name);
/// - else `live = Some(i)` with `i` in the list: (i, the list's name i);
/// - else (-1, the stored name).
// DELTON FORK DELTA: hpre-4
pub fn present_preset(
    stored: &PresentPreset,
    live: Option<usize>,
    list: Option<&ProgramList>,
) -> (i32, String) {
    if stored.number < 0 && stored.program_at_set == live {
        return (-1, stored.name.clone());
    }
    if let (Some(index), Some(list)) = (live, list)
        && let Some(name) = list.names.get(index)
        && let Ok(number) = i32::try_from(index)
    {
        return (number, name.clone());
    }
    (-1, stored.name.clone())
}

/// The `FactoryPresets` entries: (index as i32, name) for every list name whose index fits in i32.
// DELTON FORK DELTA: hpre-4
pub fn factory_entries(list: &ProgramList) -> impl Iterator<Item = (i32, &str)> {
    list.names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| Some((i32::try_from(index).ok()?, name.as_str())))
}

// ------------------------------------------------------------------------------------------------
// The program list cache
// ------------------------------------------------------------------------------------------------

/// Runs `f` on the cached list, never asking the plugin (`None` if it was never read).
// DELTON FORK DELTA: hpre-4
pub fn with_cached<R>(
    cache: &Mutex<Option<ProgramList>>,
    f: impl FnOnce(Option<&ProgramList>) -> R,
) -> R {
    f(cache.lock().as_ref())
}

/// Runs `f` on the cached list, reading it from the plugin first when it was never read. Main
/// thread only (the plugin's [`HostPresets`] is).
// DELTON FORK DELTA: hpre-4
pub fn with_list<R>(
    cache: &Mutex<Option<ProgramList>>,
    host: &dyn HostPresets,
    f: impl FnOnce(Option<&ProgramList>) -> R,
) -> R {
    {
        let cached = cache.lock();
        if cached.is_some() {
            return f(cached.as_ref());
        }
    }
    // Read outside the lock: the plugin may tell us about a change while it answers
    let fresh = host.program_list();
    let mut cached = cache.lock();
    let list = cached.get_or_insert(fresh);
    f(Some(list))
}

/// Reads the list from the plugin again into the cache. Returns whether it differed from the cached
/// one (a first read does not count). Does not tell the host. Main thread only.
// DELTON FORK DELTA: hpre-4
pub fn reread(cache: &Mutex<Option<ProgramList>>, host: &dyn HostPresets) -> bool {
    let fresh = host.program_list();
    let mut cached = cache.lock();
    let changed = cached.as_ref().is_some_and(|old| *old != fresh);
    *cached = Some(fresh);
    changed
}

// ------------------------------------------------------------------------------------------------
// Setting and reading PresentPreset
// ------------------------------------------------------------------------------------------------

/// What [`apply_preset_set()`] did.
// DELTON FORK DELTA: hpre-4
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetResult {
    /// Done: the name was kept, or the program loaded and is now the present preset.
    Done,
    /// A number the cached list does not have: `kAudioUnitErr_InvalidPropertyValue`, nothing
    /// changed.
    Refused,
    /// The plugin refused the load (the text it gave); nothing changed. The wrapper logs it and
    /// has the list refreshed.
    LoadFailed(String),
    /// Not the main thread: the load must run there, with these arguments, through [`run_load()`].
    LoadLater { index: usize, revision: u64 },
}

/// A `PresentPreset` set. Decides on the CACHED list (never a re-read, see the module doc).
/// `on_main` is whether this is the main thread: the load runs here only then.
// DELTON FORK DELTA: hpre-4
pub fn apply_preset_set(
    host: Option<&dyn HostPresets>,
    cache: &Mutex<Option<ProgramList>>,
    present: &Mutex<PresentPreset>,
    number: i32,
    name: String,
    on_main: bool,
) -> SetResult {
    match with_cached(cache, |list| preset_set(number, list)) {
        PresetSet::Refuse => SetResult::Refused,
        PresetSet::HostUserPreset => {
            let program_at_set = match host {
                Some(host) if on_main => host.current_program(),
                Some(_) | None => None,
            };
            *present.lock() = PresentPreset {
                number: -1,
                name,
                program_at_set,
            };
            SetResult::Done
        }
        PresetSet::LoadProgram { index, revision } => {
            let Some(host) = host else {
                return SetResult::Refused;
            };
            if !on_main {
                return SetResult::LoadLater { index, revision };
            }
            match run_load(host, cache, present, index, revision) {
                Ok(()) => SetResult::Done,
                Err(err) => SetResult::LoadFailed(err),
            }
        }
    }
}

/// Loads program `index` of the list at `revision` as a host load; on success it becomes the present
/// preset, named by the cached list. Main thread only. An `Err` changes nothing.
// DELTON FORK DELTA: hpre-4
pub fn run_load(
    host: &dyn HostPresets,
    cache: &Mutex<Option<ProgramList>>,
    present: &Mutex<PresentPreset>,
    index: usize,
    revision: u64,
) -> Result<(), String> {
    host.load_program(index, revision)?;
    let name = with_cached(cache, |list| {
        list.and_then(|list| list.names.get(index).cloned())
    })
    .unwrap_or_default();
    *present.lock() = PresentPreset {
        number: i32::try_from(index).unwrap_or(-1),
        name,
        program_at_set: None,
    };
    Ok(())
}

/// A `PresentPreset` get: (presetNumber, name). On the main thread the live current program is the
/// plugin's `current_program()`; elsewhere the plugin is not called and the live program is taken
/// to be the stored number when it is `>= 0`.
// DELTON FORK DELTA: hpre-4
pub fn current_preset(
    host: Option<&dyn HostPresets>,
    cache: &Mutex<Option<ProgramList>>,
    present: &Mutex<PresentPreset>,
    on_main: bool,
) -> (i32, String) {
    let stored = present.lock().clone();
    match host {
        None => with_cached(cache, |list| present_preset(&stored, None, list)),
        Some(host) if on_main => {
            let live = host.current_program();
            with_list(cache, host, |list| present_preset(&stored, live, list))
        }
        Some(_) => {
            let live = usize::try_from(stored.number).ok();
            with_cached(cache, |list| present_preset(&stored, live, list))
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The CFArray of AUPresets (macOS)
// ------------------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) use apple::factory_presets_array;
// DELTON FORK DELTA: hpre-4. Counted for the in-process host test (tests/auv2_presets.rs).
#[cfg(target_os = "macos")]
pub use apple::live_preset_blocks;

#[cfg(target_os = "macos")]
mod apple {
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicUsize, Ordering, fence};

    use nice_plug_core::host_presets::ProgramList;
    use objc2_core_foundation::{CFAllocator, CFArrayCallBacks, CFIndex};

    use super::factory_entries;
    use crate::wrapper::auv2::sys::{AUPreset, CFArray, CFRetained, CFString, CFType};

    /// How many [`PresetBlock`]s exist now.
    static LIVE_BLOCKS: AtomicUsize = AtomicUsize::new(0);

    /// How many `FactoryPresets` entries are alive: made and not yet freed. The in-process host
    /// test reads it to see that releasing the array frees its entries and that an array the host
    /// still holds keeps them.
    // DELTON FORK DELTA: hpre-4
    #[doc(hidden)]
    pub fn live_preset_blocks() -> usize {
        LIVE_BLOCKS.load(Ordering::SeqCst)
    }

    /// One `FactoryPresets` entry: the `AUPreset` the host reads (FIRST, so a pointer to the block is
    /// a pointer to the `AUPreset`) and a reference count the array's callbacks move. It owns the
    /// `presetName` string.
    // DELTON FORK DELTA: hpre-4
    #[repr(C)]
    struct PresetBlock {
        preset: AUPreset,
        refs: AtomicUsize,
    }

    impl PresetBlock {
        /// A new block with one reference (the caller's). The name is a +1 CFString the block owns.
        fn new(number: i32, name: &str) -> *mut PresetBlock {
            let name = CFRetained::into_raw(CFString::from_str(name));
            LIVE_BLOCKS.fetch_add(1, Ordering::SeqCst);
            Box::into_raw(Box::new(PresetBlock {
                preset: AUPreset {
                    presetNumber: number,
                    presetName: name.as_ptr(),
                },
                refs: AtomicUsize::new(1),
            }))
        }
    }

    /// The array's `retain` callback: one more reference to the block.
    unsafe extern "C-unwind" fn retain_block(
        _allocator: *const CFAllocator,
        value: *const c_void,
    ) -> *const c_void {
        // SAFETY: Every value in these arrays is a live `PresetBlock` from `PresetBlock::new`.
        let block = unsafe { &*(value as *const PresetBlock) };
        block.refs.fetch_add(1, Ordering::Relaxed);
        value
    }

    /// The array's `release` callback: drops one reference; at zero it releases the name and frees
    /// the block.
    unsafe extern "C-unwind" fn release_block(
        _allocator: *const CFAllocator,
        value: *const c_void,
    ) {
        let block = value as *mut PresetBlock;
        // SAFETY: Every value in these arrays is a live `PresetBlock`, and this call owns one of
        // its references.
        if unsafe { &(*block).refs }.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        // SAFETY: That was the last reference, so nothing else reads the block.
        let block = unsafe { Box::from_raw(block) };
        if let Some(name) = NonNull::new(block.preset.presetName as *mut CFString) {
            // SAFETY: The +1 reference `PresetBlock::new` took with `CFRetained::into_raw`.
            drop(unsafe { CFRetained::from_raw(name) });
        }
        drop(block);
        LIVE_BLOCKS.fetch_sub(1, Ordering::SeqCst);
    }

    /// The value of `kAudioUnitProperty_FactoryPresets`: a NEW `CFArray` (+1) whose values are
    /// `*const AUPreset`, one per list entry, the caller releasing the array.
    ///
    /// `AudioUnitProperties.h` documents the property as a `CFArrayRef` of `AUPreset` structs that
    /// the caller releases (read from memory: the header is not on this machine), and `AUBase` keeps
    /// the structs alive for the unit's life. This array instead retains and releases its entries
    /// through the callbacks below, so they live exactly as long as a host keeps the array, even
    /// after the list changes (hosts keep the array, and the structs it holds, across calls).
    ///
    /// The result is the raw pointer `GetProperty` stores; `None` is an empty array.
    // DELTON FORK DELTA: hpre-4
    pub fn factory_presets_array(list: Option<&ProgramList>) -> *const c_void {
        let blocks: Vec<*mut PresetBlock> = list
            .into_iter()
            .flat_map(factory_entries)
            .map(|(number, name)| PresetBlock::new(number, name))
            .collect();
        let mut values: Vec<*const c_void> =
            blocks.iter().map(|&block| block as *const c_void).collect();
        let callbacks = CFArrayCallBacks {
            version: 0,
            retain: Some(retain_block),
            release: Some(release_block),
            copyDescription: None,
            equal: None,
        };

        // SAFETY: `values` holds `values.len()` live blocks, `callbacks` is a valid version 0
        // structure whose functions match the blocks, and CFArray copies the structure. The array
        // retains each block once, through `retain_block`.
        let array = unsafe {
            CFArray::new(
                None,
                values.as_mut_ptr(),
                values.len() as CFIndex,
                &callbacks,
            )
        };

        // Give up the references `PresetBlock::new` made for us: the array holds its own (and if
        // the array could not be made, these were the last ones)
        for &block in &blocks {
            // SAFETY: Each block is live and we own the reference being dropped.
            unsafe { release_block(ptr::null(), block as *const c_void) };
        }

        match array {
            Some(array) => CFRetained::into_raw(array).as_ptr() as *const c_void,
            None => CFRetained::into_raw(CFArray::<CFType>::empty()).as_ptr() as *const c_void,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn list(revision: u64, names: &[&str]) -> ProgramList {
        ProgramList {
            revision,
            names: names.iter().map(|name| name.to_string()).collect(),
        }
    }

    // ---- preset_set -------------------------------------------------------------------------

    #[test]
    fn hpre4_preset_set_loads_the_first_and_last_index_at_the_lists_revision() {
        let l = list(7, &["A", "B", "C"]);
        assert_eq!(
            preset_set(0, Some(&l)),
            PresetSet::LoadProgram {
                index: 0,
                revision: 7
            }
        );
        assert_eq!(
            preset_set(2, Some(&l)),
            PresetSet::LoadProgram {
                index: 2,
                revision: 7
            }
        );
    }

    #[test]
    fn hpre4_preset_set_refuses_one_past_the_list_and_far_past_it() {
        let l = list(7, &["A", "B", "C"]);
        assert_eq!(preset_set(3, Some(&l)), PresetSet::Refuse);
        assert_eq!(preset_set(i32::MAX, Some(&l)), PresetSet::Refuse);
        // An empty list has no index 0
        assert_eq!(preset_set(0, Some(&list(1, &[]))), PresetSet::Refuse);
    }

    #[test]
    fn hpre4_preset_set_refuses_a_number_with_no_list() {
        assert_eq!(preset_set(0, None), PresetSet::Refuse);
        assert_eq!(preset_set(5, None), PresetSet::Refuse);
    }

    #[test]
    fn hpre4_preset_set_negative_is_the_hosts_own_preset_with_or_without_a_list() {
        let l = list(7, &["A", "B"]);
        for number in [-1, -2, i32::MIN] {
            assert_eq!(preset_set(number, Some(&l)), PresetSet::HostUserPreset);
            assert_eq!(preset_set(number, None), PresetSet::HostUserPreset);
        }
    }

    // ---- present_preset ---------------------------------------------------------------------

    fn user(name: &str, program_at_set: Option<usize>) -> PresentPreset {
        PresentPreset {
            number: -1,
            name: name.into(),
            program_at_set,
        }
    }

    #[test]
    fn hpre4_present_preset_host_user_preset_reads_back_while_the_program_is_unchanged() {
        let l = list(1, &["A", "B", "C"]);
        // The live program is the one at the time of the set: the host's preset is current
        assert_eq!(
            present_preset(&user("Mine", Some(1)), Some(1), Some(&l)),
            (-1, "Mine".to_string())
        );
        // No program then, none now (`None == None`): the host's name
        assert_eq!(
            present_preset(&user("Mine", None), None, Some(&l)),
            (-1, "Mine".to_string())
        );
        assert_eq!(
            present_preset(&user("Mine", None), None, None),
            (-1, "Mine".to_string())
        );
    }

    #[test]
    fn hpre4_present_preset_the_live_program_wins_once_it_moved() {
        let l = list(1, &["A", "B", "C"]);
        assert_eq!(
            present_preset(&user("Mine", Some(1)), Some(2), Some(&l)),
            (2, "C".to_string())
        );
        // From no program to one
        assert_eq!(
            present_preset(&user("Mine", None), Some(0), Some(&l)),
            (0, "A".to_string())
        );
    }

    #[test]
    fn hpre4_present_preset_a_loaded_program_reads_back_the_lists_name() {
        let l = list(1, &["A", "B", "C"]);
        let loaded = PresentPreset {
            number: 1,
            name: "stale".into(),
            program_at_set: None,
        };
        assert_eq!(
            present_preset(&loaded, Some(1), Some(&l)),
            (1, "B".to_string())
        );
    }

    #[test]
    fn hpre4_present_preset_a_live_index_the_list_does_not_have_reads_the_stored_name() {
        let l = list(1, &["A", "B"]);
        assert_eq!(
            present_preset(&user("Mine", Some(0)), Some(5), Some(&l)),
            (-1, "Mine".to_string())
        );
        // No list at all
        assert_eq!(
            present_preset(&user("Mine", Some(0)), Some(1), None),
            (-1, "Mine".to_string())
        );
        // No live program while the host's preset was made over one
        assert_eq!(
            present_preset(&user("Mine", Some(0)), None, Some(&l)),
            (-1, "Mine".to_string())
        );
    }

    // ---- factory_entries --------------------------------------------------------------------

    #[test]
    fn hpre4_factory_entries_are_the_numbers_and_names_in_order() {
        let l = list(3, &["Bank / A", "B", "C"]);
        let entries: Vec<(i32, &str)> = factory_entries(&l).collect();
        assert_eq!(entries, vec![(0, "Bank / A"), (1, "B"), (2, "C")]);
    }

    #[test]
    fn hpre4_factory_entries_of_an_empty_list_are_empty() {
        assert_eq!(factory_entries(&list(3, &[])).count(), 0);
    }

    // ---- the cache and the set / get paths --------------------------------------------------

    /// A plugin whose list can change under the wrapper.
    struct Host {
        list: Mutex<ProgramList>,
        current: Mutex<Option<usize>>,
        loads: Mutex<Vec<(usize, u64)>>,
        fail: Mutex<bool>,
        list_reads: AtomicUsize,
    }

    impl Host {
        fn new(names: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                list: Mutex::new(list(1, names)),
                current: Mutex::new(None),
                loads: Mutex::new(Vec::new()),
                fail: Mutex::new(false),
                list_reads: AtomicUsize::new(0),
            })
        }

        fn replace_list(&self, revision: u64, names: &[&str]) {
            *self.list.lock() = list(revision, names);
        }
    }

    impl HostPresets for Host {
        fn load_file(&self, _path: &std::path::Path) -> Result<(), String> {
            Err("no files".into())
        }
        fn program_list(&self) -> ProgramList {
            self.list_reads.fetch_add(1, Ordering::SeqCst);
            self.list.lock().clone()
        }
        fn load_program(&self, index: usize, revision: u64) -> Result<(), String> {
            self.loads.lock().push((index, revision));
            if *self.fail.lock() {
                return Err("refused".into());
            }
            let list = self.list.lock();
            if revision != list.revision || index >= list.names.len() {
                return Err("stale".into());
            }
            *self.current.lock() = Some(index);
            Ok(())
        }
        fn current_program(&self) -> Option<usize> {
            *self.current.lock()
        }
        fn midi_program_change(&self) -> bool {
            true
        }
        fn program_change(&self, _index: usize) {}
    }

    struct State {
        cache: Mutex<Option<ProgramList>>,
        present: Mutex<PresentPreset>,
    }

    fn state() -> State {
        State {
            cache: Mutex::new(None),
            present: Mutex::new(PresentPreset::default()),
        }
    }

    #[test]
    fn hpre4_with_list_reads_once_and_with_cached_never_reads() {
        let host = Host::new(&["A", "B"]);
        let s = state();
        // Never read: the cached view is empty and the plugin was not asked
        assert_eq!(with_cached(&s.cache, |l| l.cloned()), None);
        assert_eq!(host.list_reads.load(Ordering::SeqCst), 0);

        assert_eq!(with_list(&s.cache, &*host, |l| l.unwrap().names.len()), 2);
        assert_eq!(with_list(&s.cache, &*host, |l| l.unwrap().names.len()), 2);
        assert_eq!(host.list_reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn hpre4_reread_reports_a_change_but_not_a_first_read() {
        let host = Host::new(&["A", "B"]);
        let s = state();
        assert!(!reread(&s.cache, &*host), "a first read is not a change");
        assert!(!reread(&s.cache, &*host), "the same list is not a change");
        host.replace_list(2, &["A", "B", "C"]);
        assert!(reread(&s.cache, &*host));
        assert_eq!(with_cached(&s.cache, |l| l.unwrap().revision), 2);
    }

    #[test]
    fn hpre4_set_loads_with_the_revision_of_the_list_the_host_read_not_the_plugins_now() {
        // The host read revision 1, then the plugin's list moved to revision 2. The host's index
        // 1 is an index into revision 1: the load must say so, and the plugin refuses it
        let host = Host::new(&["A", "B"]);
        let s = state();
        reread(&s.cache, &*host);
        host.replace_list(2, &["X", "A", "B"]);

        let result = apply_preset_set(
            Some(&*host),
            &s.cache,
            &s.present,
            1,
            "ignored".into(),
            true,
        );
        assert_eq!(*host.loads.lock(), vec![(1, 1)]);
        assert_eq!(result, SetResult::LoadFailed("stale".into()));
        assert_eq!(*s.present.lock(), PresentPreset::default());
        // and it did not re-read the list to find out
        assert_eq!(host.list_reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn hpre4_set_a_program_loads_stores_the_lists_name_and_reads_back() {
        let host = Host::new(&["A", "B", "C"]);
        let s = state();
        reread(&s.cache, &*host);

        let result = apply_preset_set(Some(&*host), &s.cache, &s.present, 2, "x".into(), true);
        assert_eq!(result, SetResult::Done);
        assert_eq!(*host.loads.lock(), vec![(2, 1)]);
        assert_eq!(
            *s.present.lock(),
            PresentPreset {
                number: 2,
                name: "C".into(),
                program_at_set: None
            }
        );
        assert_eq!(
            current_preset(Some(&*host), &s.cache, &s.present, true),
            (2, "C".to_string())
        );
    }

    #[test]
    fn hpre4_set_with_no_cached_list_or_past_it_refuses_and_changes_nothing() {
        let host = Host::new(&["A", "B"]);
        let s = state();
        // Never read
        assert_eq!(
            apply_preset_set(Some(&*host), &s.cache, &s.present, 0, "x".into(), true),
            SetResult::Refused
        );
        reread(&s.cache, &*host);
        assert_eq!(
            apply_preset_set(Some(&*host), &s.cache, &s.present, 2, "x".into(), true),
            SetResult::Refused
        );
        // No HostPresets
        assert_eq!(
            apply_preset_set(None, &s.cache, &s.present, 0, "x".into(), true),
            SetResult::Refused
        );
        assert!(host.loads.lock().is_empty());
        assert_eq!(*s.present.lock(), PresentPreset::default());
    }

    #[test]
    fn hpre4_set_a_failing_load_changes_nothing() {
        let host = Host::new(&["A", "B"]);
        let s = state();
        reread(&s.cache, &*host);
        *host.fail.lock() = true;
        let result = apply_preset_set(Some(&*host), &s.cache, &s.present, 1, "x".into(), true);
        assert_eq!(result, SetResult::LoadFailed("refused".into()));
        assert_eq!(*s.present.lock(), PresentPreset::default());
    }

    #[test]
    fn hpre4_set_off_the_main_thread_defers_the_load_and_never_calls_the_plugin() {
        let host = Host::new(&["A", "B"]);
        let s = state();
        reread(&s.cache, &*host);
        let result = apply_preset_set(Some(&*host), &s.cache, &s.present, 1, "x".into(), false);
        assert_eq!(
            result,
            SetResult::LoadLater {
                index: 1,
                revision: 1
            }
        );
        assert!(host.loads.lock().is_empty());
        // and the host's own preset records no program off the main thread
        *host.current.lock() = Some(0);
        assert_eq!(
            apply_preset_set(Some(&*host), &s.cache, &s.present, -1, "Mine".into(), false),
            SetResult::Done
        );
        assert_eq!(s.present.lock().program_at_set, None);
    }

    #[test]
    fn hpre4_set_a_host_user_preset_keeps_the_name_and_the_program_it_was_made_over() {
        let host = Host::new(&["A", "B"]);
        let s = state();
        *host.current.lock() = Some(1);
        let result = apply_preset_set(Some(&*host), &s.cache, &s.present, -1, "Mine".into(), true);
        assert_eq!(result, SetResult::Done);
        assert!(host.loads.lock().is_empty());
        assert_eq!(
            *s.present.lock(),
            PresentPreset {
                number: -1,
                name: "Mine".into(),
                program_at_set: Some(1)
            }
        );
        assert_eq!(
            current_preset(Some(&*host), &s.cache, &s.present, true),
            (-1, "Mine".to_string())
        );
        // The plugin moves to another program: the live one wins
        *host.current.lock() = Some(0);
        assert_eq!(
            current_preset(Some(&*host), &s.cache, &s.present, true),
            (0, "A".to_string())
        );
    }

    #[test]
    fn hpre4_get_off_the_main_thread_never_calls_the_plugin() {
        let host = Host::new(&["A", "B"]);
        let s = state();
        reread(&s.cache, &*host);
        apply_preset_set(Some(&*host), &s.cache, &s.present, 1, "x".into(), true);
        let reads = host.list_reads.load(Ordering::SeqCst);
        // The plugin's own program moved, but this thread may not ask: the stored number is read
        *host.current.lock() = Some(0);
        assert_eq!(
            current_preset(Some(&*host), &s.cache, &s.present, false),
            (1, "B".to_string())
        );
        assert_eq!(host.list_reads.load(Ordering::SeqCst), reads);
    }

    #[test]
    fn hpre4_get_without_host_presets_is_the_stored_name() {
        let s = state();
        assert_eq!(
            apply_preset_set(None, &s.cache, &s.present, -1, "Mine".into(), true),
            SetResult::Done
        );
        assert_eq!(
            current_preset(None, &s.cache, &s.present, true),
            (-1, "Mine".to_string())
        );
    }
}
