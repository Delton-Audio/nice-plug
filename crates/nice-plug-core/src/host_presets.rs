//! The plugin's presets as the host's own preset UI uses them.
// DELTON FORK DELTA: hpre-2

/// The program list as the host's program menu shows it.
// DELTON FORK DELTA: hpre-3
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProgramList {
    /// Moves whenever the list changes. A load names the revision it read the list at.
    pub revision: u64,
    /// In menu order: "<Category> / <Name>", just "<Name>" at a bank root.
    pub names: Vec<String>,
}

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

    /// The program list as the host's program menu shows it (the VST3 program list).
    // DELTON FORK DELTA: hpre-3
    fn program_list(&self) -> ProgramList;

    /// Loads program `index` of the list at `revision`, as a host load. A stale revision or an
    /// index past the list loads nothing and is an `Err` (text for the host / the log).
    // DELTON FORK DELTA: hpre-3
    fn load_program(&self, index: usize, revision: u64) -> Result<(), String>;

    /// The current preset's index in the list now; `None` with no current preset or one the list
    /// does not have.
    // DELTON FORK DELTA: hpre-3
    fn current_program(&self) -> Option<usize>;

    /// Whether MIDI program change selects a program (`[presets] midi_program_change`).
    // DELTON FORK DELTA: hpre-3
    fn midi_program_change(&self) -> bool;
}
