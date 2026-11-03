//! What a plugin may tell its host when no editor is open.
// DELTON FORK DELTA: hpre-1

use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// What a plugin may tell its host with no editor open. Cheap to clone, callable from any thread,
/// never blocks; a notice sent before the host's main thread runs is merged with the next one.
///
/// The wrapper creates one per plugin instance and hands it over once, through
/// [`Plugin::attach_host_notices()`][crate::plugin::Plugin::attach_host_notices()].
#[derive(Clone)]
pub struct HostNotices {
    sink: Arc<dyn HostNoticeSink>,
}

/// The wrapper's end of a [`HostNotices`]: one implementation per plugin format. A call must be
/// cheap, callable from any thread and must never block.
pub trait HostNoticeSink: Send + Sync {
    /// Parameter values changed without a gesture: the host re-reads every value.
    fn param_values_changed(&self);

    /// The plugin loaded this preset file itself (not the host asking): the host learns which
    /// preset is loaded. A format with no such notion makes this an explicit no-op.
    // DELTON FORK DELTA: hpre-2
    fn preset_loaded(&self, path: &Path);

    /// The program list changed (a preset was added, renamed or removed): a format with a program
    /// list tells the host to re-read it. A format with none makes this an explicit no-op.
    // DELTON FORK DELTA: hpre-3
    fn program_list_changed(&self);
}

impl HostNotices {
    /// Wrap a wrapper's sink.
    pub fn new(sink: Arc<dyn HostNoticeSink>) -> Self {
        Self { sink }
    }

    /// Parameter values changed without a gesture (written while no editor was open): the host
    /// re-reads every value. Many calls before the host's main thread runs reach the host as one
    /// notice. Does nothing once the plugin instance is gone.
    pub fn param_values_changed(&self) {
        self.sink.param_values_changed();
    }

    /// The plugin loaded this preset file itself (its own preset UI, not a host load): the host
    /// learns which preset is loaded. Never call it for a load the host asked for; the wrapper
    /// answers that one. Does nothing once the plugin instance is gone, and in formats with no
    /// such notion.
    // DELTON FORK DELTA: hpre-2
    pub fn preset_loaded(&self, path: &Path) {
        self.sink.preset_loaded(path);
    }

    /// The plugin's program list changed (see
    /// [`HostPresets::program_list()`][crate::host_presets::HostPresets::program_list()]): the
    /// host re-reads it. Many calls before the host's main thread runs reach the host as one
    /// notice. Does nothing once the plugin instance is gone, and in formats with no program list.
    // DELTON FORK DELTA: hpre-3
    pub fn program_list_changed(&self) {
        self.sink.program_list_changed();
    }
}

impl fmt::Debug for HostNotices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostNotices").finish_non_exhaustive()
    }
}
