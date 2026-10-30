// DELTON FORK DELTA: au-4a, AUv2 wrapper
//! The host's audio workgroup: the slot the render-context observer writes and `process` reads,
//! and the retain bookkeeping behind it. Pure: builds and tests on every OS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use nice_plug_core::context::process::AudioWorkgroup;

/// How a workgroup is kept alive. The macOS wrapper uses `os_retain` / `os_release` (F3); tests
/// count calls.
pub(crate) trait Retain {
    /// # Safety
    /// `workgroup` is a live `os_workgroup_t` (non-null).
    unsafe fn retain(workgroup: *mut c_void);
    /// # Safety
    /// `workgroup` was passed to `retain` and not released since.
    unsafe fn release(workgroup: *mut c_void);
}

/// How many distinct workgroups one instance keeps retained.
pub(crate) const WORKGROUP_CAPACITY: usize = 16;

/// The workgroup the host last gave this instance, plus every workgroup handed out before, all
/// retained until drop. A worker may still be joined to an earlier workgroup for up to one
/// period after the host switches, so an earlier one is never released while the instance
/// lives, except when more than `WORKGROUP_CAPACITY` distinct workgroups were seen (below).
pub(crate) struct WorkgroupSlot<R: Retain> {
    current: AtomicPtr<c_void>,
    seen: [AtomicPtr<c_void>; WORKGROUP_CAPACITY],
    /// Count of inserts into `seen`; the next insert goes to `inserts % WORKGROUP_CAPACITY`.
    inserts: AtomicUsize,
    _retain: PhantomData<R>,
}

impl<R: Retain> WorkgroupSlot<R> {
    pub(crate) fn new() -> Self {
        Self {
            current: AtomicPtr::new(ptr::null_mut()),
            seen: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            inserts: AtomicUsize::new(0),
            _retain: PhantomData,
        }
    }

    /// Called on the render thread from the observer block; wait-free, no allocation, no lock.
    pub(crate) fn set(&self, workgroup: *mut c_void) {
        let previous = self.current.load(Ordering::Acquire);
        if workgroup == previous {
            return;
        }

        if !workgroup.is_null()
            && !self
                .seen
                .iter()
                .any(|entry| entry.load(Ordering::Acquire) == workgroup)
        {
            // SAFETY: `workgroup` is non-null and the host gave it to the observer, so it is live.
            unsafe { R::retain(workgroup) };
            let index = self.inserts.fetch_add(1, Ordering::AcqRel) % WORKGROUP_CAPACITY;
            let evicted = self.seen[index].swap(workgroup, Ordering::AcqRel);
            // The workgroup that is `previous` may have a worker joined to it right now, so an
            // evicted `previous` is not released: that one reference is kept for the rest of the
            // instance's life.
            if !evicted.is_null() && evicted != previous {
                // SAFETY: `evicted` was retained when it was inserted and is released only here
                // (or in `drop`, which no longer sees it).
                unsafe { R::release(evicted) };
            }
        }

        self.current.store(workgroup, Ordering::Release);
    }

    pub(crate) fn current(&self) -> Option<AudioWorkgroup> {
        NonNull::new(self.current.load(Ordering::Acquire)).map(|workgroup| {
            // SAFETY: Every non-null `current` is in `seen`, retained until drop.
            unsafe { AudioWorkgroup::from_raw(workgroup) }
        })
    }
}

impl<R: Retain> Drop for WorkgroupSlot<R> {
    fn drop(&mut self) {
        for entry in &self.seen {
            let workgroup = entry.load(Ordering::Acquire);
            if !workgroup.is_null() {
                // SAFETY: Every non-null `seen` entry was retained and not released since.
                unsafe { R::release(workgroup) };
            }
        }
    }
}

/// `os_retain` / `os_release`.
#[cfg(target_os = "macos")]
pub(crate) struct OsRetain;

#[cfg(target_os = "macos")]
impl Retain for OsRetain {
    unsafe fn retain(workgroup: *mut c_void) {
        // SAFETY: Forwarded from the trait's contract.
        unsafe { super::sys::os_retain(workgroup) };
    }

    unsafe fn release(workgroup: *mut c_void) {
        // SAFETY: Forwarded from the trait's contract.
        unsafe { super::sys::os_release(workgroup) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! { static LOG: RefCell<Vec<(&'static str, usize)>> = const { RefCell::new(Vec::new()) }; }

    struct Counting;
    impl Retain for Counting {
        unsafe fn retain(w: *mut c_void) {
            LOG.with(|l| l.borrow_mut().push(("retain", w as usize)))
        }
        unsafe fn release(w: *mut c_void) {
            LOG.with(|l| l.borrow_mut().push(("release", w as usize)))
        }
    }

    /// A fake workgroup, never dereferenced.
    fn wg(n: usize) -> *mut c_void {
        (n * 16) as *mut c_void
    }

    fn log() -> Vec<(&'static str, usize)> {
        LOG.with(|l| l.borrow().clone())
    }

    fn clear() {
        LOG.with(|l| l.borrow_mut().clear());
    }

    fn current_ptr(slot: &WorkgroupSlot<Counting>) -> Option<*mut c_void> {
        slot.current().map(AudioWorkgroup::as_ptr)
    }

    fn released_sorted() -> Vec<usize> {
        let mut released: Vec<usize> = log()
            .iter()
            .filter(|e| e.0 == "release")
            .map(|e| e.1)
            .collect();
        released.sort_unstable();
        released
    }

    #[test]
    fn same_workgroup_twice_retains_once() {
        clear();
        let slot = WorkgroupSlot::<Counting>::new();
        slot.set(wg(1));
        slot.set(wg(1));
        assert_eq!(log(), vec![("retain", 16)]);
        assert_eq!(current_ptr(&slot), Some(wg(1)));
    }

    #[test]
    fn null_clears_current_but_keeps_the_reference() {
        clear();
        let slot = WorkgroupSlot::<Counting>::new();
        slot.set(wg(1));
        slot.set(ptr::null_mut());
        assert_eq!(log(), vec![("retain", 16)]);
        assert_eq!(slot.current(), None);
        drop(slot);
        let log = log();
        assert_eq!(log.last(), Some(&("release", 16)));
        assert_eq!(log.iter().filter(|e| e.0 == "release").count(), 1);
    }

    #[test]
    fn switching_back_does_not_retain_again() {
        clear();
        let slot = WorkgroupSlot::<Counting>::new();
        slot.set(wg(1));
        slot.set(wg(2));
        slot.set(wg(1));
        assert_eq!(log(), vec![("retain", 16), ("retain", 32)]);
        assert_eq!(current_ptr(&slot), Some(wg(1)));
    }

    #[test]
    fn the_seventeenth_workgroup_evicts_the_first() {
        clear();
        let slot = WorkgroupSlot::<Counting>::new();
        for n in 1..=17 {
            slot.set(wg(n));
        }
        let mut expected: Vec<_> = (1..=17).map(|n| ("retain", n * 16)).collect();
        expected.push(("release", 16));
        assert_eq!(log(), expected);
        assert_eq!(current_ptr(&slot), Some(wg(17)));
    }

    #[test]
    fn the_previous_workgroup_is_never_evicted() {
        clear();
        let slot = WorkgroupSlot::<Counting>::new();
        for n in 1..=16 {
            slot.set(wg(n));
        }
        // Found in `seen`: no retain, and it becomes the current workgroup
        slot.set(wg(1));
        assert_eq!(current_ptr(&slot), Some(wg(1)));
        // The insert evicts slot 0, which holds wg(1) == previous: not released
        slot.set(wg(17));
        let before_drop = log();
        assert_eq!(before_drop.iter().filter(|e| e.0 == "retain").count(), 17);
        assert_eq!(before_drop.iter().filter(|e| e.0 == "release").count(), 0);
        drop(slot);
        let expected: Vec<usize> = (2..=17).map(|n| n * 16).collect();
        assert_eq!(released_sorted(), expected);
    }

    #[test]
    fn drop_releases_each_retained_once() {
        clear();
        let slot = WorkgroupSlot::<Counting>::new();
        slot.set(wg(1));
        slot.set(wg(2));
        slot.set(wg(3));
        drop(slot);
        assert_eq!(released_sorted(), vec![16, 32, 48]);
    }
}
