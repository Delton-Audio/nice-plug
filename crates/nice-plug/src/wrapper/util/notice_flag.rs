//! The flag that merges the plugin's host notices into one task for the host's main thread.
// DELTON FORK DELTA: hpre-1

use std::sync::atomic::{AtomicBool, Ordering};

/// A flag with two sides: the plugin side raises it, the main thread takes it. Only the call that
/// raises it from clear posts a task, so any number of notices sent before the main thread runs
/// cost one task, and the task queue never fills up with them.
///
/// The main thread must take the flag *before* it reads the values the notice is about: a write
/// that lands after the take raises the flag again and posts a new task, so no write is lost.
#[derive(Debug, Default)]
pub(crate) struct NoticeFlag(AtomicBool);

#[allow(dead_code)] // each wrapper format uses a different subset, and the standalone one none
impl NoticeFlag {
    /// Raise the flag. Returns `true` only for the call that raised it from clear.
    pub(crate) fn raise(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    /// Clear the flag. Returns `true` if it was raised.
    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_flag_second_raise_does_not_rise() {
        let flag = NoticeFlag::default();
        assert!(flag.raise());
        assert!(!flag.raise());
    }

    #[test]
    fn notice_flag_take_clears_so_the_next_raise_rises_again() {
        let flag = NoticeFlag::default();
        assert!(flag.raise());
        assert!(flag.take());
        assert!(flag.raise());
    }

    #[test]
    fn notice_flag_take_on_a_clear_flag_is_false() {
        let flag = NoticeFlag::default();
        assert!(!flag.take());
        assert!(flag.raise());
        assert!(flag.take());
        assert!(!flag.take());
    }
}
