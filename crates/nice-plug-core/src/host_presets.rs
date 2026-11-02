//! The plugin's presets as the host's own preset UI uses them.
// DELTON FORK DELTA: hpre-2

/// The plugin's presets as the host's own preset UI uses them (the CLAP `preset-load` extension).
/// The wrapper asks the plugin for one of these once, through
/// [`Plugin::host_presets()`][crate::plugin::Plugin::host_presets()], and keeps it, so a host load
/// never locks the plugin (which the audio thread may hold).
///
/// Called on the host's main thread only.
pub trait HostPresets: Send + Sync {
    /// Load a preset file the host picked, as a host load. `Err` is the text handed back to the
    /// host.
    fn load_file(&self, path: &std::path::Path) -> Result<(), String>;
}
