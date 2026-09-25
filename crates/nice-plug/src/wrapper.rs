//! Wrappers for different plugin types. Each wrapper has an entry point macro that you can pass the
//! name of a type that implements `Plugin` to. The macro will handle the rest.

pub mod clap;
pub(crate) mod state;
pub(crate) mod util;

// DELTON FORK DELTA: module-deinit hooks (offered upstream).
static MODULE_DEINIT_HOOKS: std::sync::Mutex<Vec<fn()>> = std::sync::Mutex::new(Vec::new());

/// Register a function to run when the host deinitializes this plugin module: CLAP's
/// `clap_entry.deinit`, and the VST3 module exit (`ExitDll` on Windows, `bundleExit` on macOS,
/// `ModuleExit` on Linux). This is the last code the library runs before the host may unload the
/// module, so it is where a library layered on nice-plug joins threads it still owns.
///
/// Hooks run in registration order, on every deinit call, so they must be idempotent. Registering
/// the same function twice runs it twice.
pub fn on_module_deinit(hook: fn()) {
    let mut hooks = MODULE_DEINIT_HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    hooks.push(hook);
}

/// Run every registered hook. Called by the export macros; public (but `#[doc(hidden)]`) only so
/// the macro expansion in a plugin crate can reach it.
#[doc(hidden)]
pub fn run_module_deinit_hooks() {
    let hooks = MODULE_DEINIT_HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for hook in hooks {
        hook();
    }
}

#[cfg(feature = "standalone")]
pub mod standalone;
#[cfg(feature = "vst3")]
pub mod vst3;

// This is used by the wrappers.
pub use util::setup_logger;

// DELTON FORK DELTA
#[cfg(test)]
mod module_deinit_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER_A: AtomicUsize = AtomicUsize::new(0);
    static COUNTER_OTHER: AtomicUsize = AtomicUsize::new(0);
    static ORDER: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn increment_a() {
        COUNTER_A.fetch_add(1, Ordering::SeqCst);
    }

    fn record_one() {
        ORDER.lock().unwrap().push(b'1');
    }

    fn record_two() {
        ORDER.lock().unwrap().push(b'2');
    }

    fn register_other() {
        on_module_deinit(increment_other);
    }

    fn increment_other() {
        COUNTER_OTHER.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn a_registered_hook_runs_on_each_deinit() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        let before = COUNTER_A.load(Ordering::SeqCst);
        on_module_deinit(increment_a);
        run_module_deinit_hooks();
        run_module_deinit_hooks();
        assert_eq!(COUNTER_A.load(Ordering::SeqCst) - before, 2);
    }

    #[test]
    fn hooks_run_in_registration_order() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        on_module_deinit(record_one);
        on_module_deinit(record_two);
        run_module_deinit_hooks();
        let order = ORDER.lock().unwrap();
        assert!(order.windows(2).any(|pair| pair == b"12"));
    }

    #[test]
    fn a_hook_may_register_another_hook_without_deadlock() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        let before = COUNTER_OTHER.load(Ordering::SeqCst);
        on_module_deinit(register_other);
        run_module_deinit_hooks();
        run_module_deinit_hooks();
        assert!(COUNTER_OTHER.load(Ordering::SeqCst) > before);
    }
}
