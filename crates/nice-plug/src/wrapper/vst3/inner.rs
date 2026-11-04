use atomic_refcell::AtomicRefCell;
use crossbeam::atomic::AtomicCell;
use crossbeam::channel;
use nice_plug_core::audio_setup::{AudioIOLayout, BufferConfig, ProcessMode};
#[cfg(feature = "editor")]
use nice_plug_core::context::gui::GuiContext;
use nice_plug_core::context::process::Transport;
#[cfg(feature = "editor")]
use nice_plug_core::editor::dpi::Size;
#[cfg(feature = "editor")]
use nice_plug_core::editor::{Editor, SpawnedEditor};
// DELTON FORK DELTA: hpre-1
use nice_plug_core::host_notices::{HostNoticeSink, HostNotices};
// DELTON FORK DELTA: hpre-3
use nice_plug_core::host_presets::{HostPresets, ProgramList};
use nice_plug_core::midi::{MidiConfig, PluginNoteEvent};
use nice_plug_core::params::internals::ParamPtr;
use nice_plug_core::params::{ParamFlags, Params};
#[cfg(feature = "editor")]
use nice_plug_core::plugin::TrackInfo;
use nice_plug_core::plugin::{Plugin, PluginState, ProcessStatus, TaskExecutor};
use parking_lot::Mutex;
#[cfg(feature = "editor")]
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use try_lock::TryLock;
#[cfg(feature = "editor")]
use vst3::ComWrapper;
use vst3::Steinberg::Vst::{
    IComponentHandler, IComponentHandlerTrait, IEventList, IUnitHandler, IUnitHandlerTrait,
    RestartFlags_,
};
use vst3::Steinberg::{kInvalidArgument, kResultOk, tresult};
use vst3::{ComPtr, ComRef};

use super::context::{WrapperActivateContext, WrapperProcessContext};
use super::note_expressions::NoteExpressionController;
use super::param_units::ParamUnits;
use super::util::{
    PROGRAM_LIST_ID, VST3_MIDI_PARAMS_END, VST3_MIDI_PARAMS_START, VST3_MIDI_PRESET_NUM_PARAMS,
    VST3_MIDI_PRESET_PARAMS_END, VST3_MIDI_PRESET_PARAMS_START, VST3_PROGRAM_PARAM_ID,
    midi_value_from_normalized, program_from_normalized, program_index,
};
use crate::event_loop::{EventLoop, MainThreadExecutor, OsEventLoop};
use crate::util::permit_alloc;
use crate::wrapper::state;
use crate::wrapper::util::buffer_management::BufferManager;
use crate::wrapper::util::hash_param_id;
// DELTON FORK DELTA: hpre-1
use crate::wrapper::util::notice_flag::NoticeFlag;
// DELTON FORK DELTA
use crate::wrapper::util::raw_automation::RawAutomation;
use crate::wrapper::vst3::Vst3Plugin;
#[cfg(feature = "editor")]
use crate::wrapper::vst3::context::WrapperGuiContext;
#[cfg(feature = "editor")]
use crate::wrapper::vst3::view::WrapperView;

/// The actual wrapper bits. We need this as an `Arc<T>` so we can safely use our event loop API.
/// Since we can't combine that with VST3's interior reference counting this just has to be moved to
/// its own struct.
pub(crate) struct WrapperInner<P: Vst3Plugin> {
    /// The wrapped plugin instance.
    pub plugin: TryLock<P>,
    /// The plugin's background task executor closure.
    pub task_executor: Mutex<TaskExecutor<P>>,
    /// The plugin's parameters. These are fetched once during initialization. That way the
    /// `ParamPtr`s are guaranteed to live at least as long as this object and we can interact with
    /// the `Params` object without having to acquire a lock on `plugin`.
    pub params: Arc<dyn Params>,
    /// The plugin's editor, if it has one. This object does not do anything on its own, but we need
    /// to instantiate this in advance so we don't need to lock the entire [`Plugin`] object when
    /// creating an editor. Wrapped in an `AtomicRefCell` because it needs to be initialized late.
    #[allow(clippy::type_complexity)]
    #[cfg(feature = "editor")]
    pub editor: AtomicRefCell<Option<Arc<Mutex<P::Editor>>>>,
    #[allow(clippy::type_complexity)]
    // My personal record for craziest Rust type!!!!
    #[cfg(feature = "editor")]
    pub editor_window:
        Arc<AtomicRefCell<Option<fragile::Fragile<SpawnedEditor<<P::Editor as Editor>::Handle>>>>>,

    /// The host's [`IComponentHandler`] instance, if passed through
    /// [`IEditController::set_component_handler`].
    pub component_handler: AtomicRefCell<Option<ComPtr<IComponentHandler>>>,

    /// Our own [`IPlugView`] instance.
    #[cfg(feature = "editor")]
    pub plug_view: RwLock<Option<ComWrapper<WrapperView<P>>>>,

    /// Whether the editor is currently open. This is changed by the attached and removed functions on IPlugViewTrait
    #[cfg(feature = "editor")]
    pub is_editor_open: AtomicBool,

    /// A realtime-safe task queue so the plugin can schedule tasks that need to be run later on the
    /// GUI thread. This field should not be used directly for posting tasks. This should be done
    /// through [`Self::schedule_gui()`] instead. That method posts the task to the host's
    /// `IRunLoop` instead of it's available.
    ///
    /// This AtomicRefCell+Option is only needed because it has to be initialized late. There is no
    /// reason to mutably borrow the event loop, so reads will never be contested.
    ///
    /// TODO: Is there a better type for Send+Sync late initialization?
    pub event_loop: AtomicRefCell<Option<OsEventLoop<Task<P>, Self>>>,

    /// Raised by the plugin's [`HostNotices`] when parameter values changed with no editor open;
    /// taken by the `ParamValuesNotice` task, so a burst of notices posts one task.
    // DELTON FORK DELTA: hpre-1
    pub param_values_notice: NoticeFlag,

    /// The plugin's presets as the host's program list, asked for once right after
    /// [`attach_host_notices()`][Plugin::attach_host_notices()]. With `None` the wrapper has no
    /// program list and no program parameter.
    // DELTON FORK DELTA: hpre-3
    pub host_presets: OnceLock<Arc<dyn HostPresets>>,
    /// The program list as last read, on first use and whenever it changed. Main thread only; the
    /// audio thread never touches it.
    // DELTON FORK DELTA: hpre-3
    pub program_list: Mutex<Option<ProgramList>>,
    /// The latest program change the audio thread saw, as an INDEX into the program list (hpre-3b;
    /// hpre-3 stored the program parameter's value): the index, with [`PLUGIN_LOAD`] set for a MIDI
    /// program change (a plugin load) and clear for the program parameter (a host load), or
    /// [`NO_PROGRAM_CHANGE`]. The kind is a tag in the same word as the index, so one store writes
    /// both and the pair cannot tear; latest wins whichever source wrote it. Taken by the
    /// `ProgramChange` task.
    // DELTON FORK DELTA: hpre-3
    pub program_change_pending: AtomicU64,
    /// The program parameter's step count (programs minus one, 0 for none or one) as of the
    /// cached list, so the audio thread can turn the parameter's normalized value into an index.
    /// Written on the main thread wherever the cached list is set.
    // DELTON FORK DELTA: hpre-3b
    pub program_step_mirror: AtomicI32,
    /// Whether the 48 hidden MIDI bank select / program change parameters exist: the wrapper has
    /// [`HostPresets`], `P::MIDI_INPUT >= MidiConfig::Basic` and `midi_program_change()`. Read
    /// once, when `host_presets` is set.
    // DELTON FORK DELTA: hpre-3b
    pub midi_preset_params: AtomicBool,
    /// Per MIDI channel the bank select MSB (CC 0) and LSB (CC 32) the host last sent, 0 at
    /// creation. They live as long as the wrapper: no reset, not saved.
    // DELTON FORK DELTA: hpre-3b
    pub midi_bank_msb: [AtomicU8; 16],
    // DELTON FORK DELTA: hpre-3b
    pub midi_bank_lsb: [AtomicU8; 16],
    /// The changes of the 48 MIDI bank select / program change parameters in one `process` call,
    /// sorted before they are applied. Preallocated; the audio thread only borrows it.
    // DELTON FORK DELTA: hpre-3b
    pub midi_preset_changes: AtomicRefCell<Vec<MidiPresetChange>>,
    /// Raised with a pending program change, so a burst posts one `ProgramChange` task.
    // DELTON FORK DELTA: hpre-3
    pub program_change_notice: NoticeFlag,
    /// Raised when the program list changed, so a burst posts one `ProgramListChanged` task.
    // DELTON FORK DELTA: hpre-3
    pub program_list_notice: NoticeFlag,

    /// Whether the plugin is currently processing audio. In other words, the last state
    /// `IAudioProcessor::setActive()` has been called with.
    pub is_processing: AtomicBool,
    // DELTON FORK DELTA
    /// Whether `setActive(true)` succeeded and `setActive(false)` has not been called since.
    /// `process` outputs silence while this is false (STATUS 142).
    pub is_activated: AtomicBool,
    /// The current audio IO layout. Modified through `IAudioProcessor::setBusArrangements()` after
    /// matching the proposed bus arrangement to one of the supported ones. The plugin's first audio
    /// IO layout is chosen as the default. Because of the way VST3 works it's not possible to
    /// change the number of busses from that default, only the channel counts can change.
    pub current_audio_io_layout: AtomicCell<AudioIOLayout>,
    /// The current buffer configuration, containing the sample rate and the maximum block size.
    /// Will be set in `IAudioProcessor::setupProcessing()`.
    pub current_buffer_config: AtomicCell<Option<BufferConfig>>,
    /// The current audio processing mode. Set in `IAudioProcessor::setup_processing()`.
    pub current_process_mode: AtomicCell<ProcessMode>,
    /// The most recently reported track information. Hosts may send partial updates (e.g.
    /// Ableton), so this is used to merge successive [`IInfoListener::setChannelContextInfos()`]
    /// calls.
    #[cfg(feature = "editor")]
    pub current_track_info: AtomicRefCell<TrackInfo>,

    /// The last process status returned by the plugin. This is used for tail handling.
    pub last_process_status: AtomicCell<ProcessStatus>,
    /// The current latency in samples, as set by the plugin through the [`ActivateContext`] and the
    /// [`ProcessContext`].
    pub current_latency: AtomicU32,
    /// A data structure that helps manage and create buffers for all of the plugin's inputs and
    /// outputs based on channel pointers provided by the host.
    pub buffer_manager: AtomicRefCell<BufferManager>,
    /// The incoming events for the plugin, if `P::ACCEPTS_MIDI` is set. If
    /// `P::SAMPLE_ACCURATE_AUTOMATION`, this is also read in lockstep with the parameter change
    /// block splitting.
    ///
    /// NOTE: Because with VST3 MIDI CC messages are sent as parameter changes and VST3 does not
    ///       interleave parameter changes and note events, this queue has to be sorted when
    ///       creating the process context
    pub input_note_events: AtomicRefCell<VecDeque<PluginNoteEvent<P>>>,
    /// VST3 has several useful predefined note expressions, but for some reason they are the only
    /// note event type that don't have MIDI note ID and channel fields. So we need to keep track of
    /// the most recent VST3 note IDs we've seen, and then map those back to MIDI note IDs and
    /// channels as needed.
    pub note_expression_controller: AtomicRefCell<NoteExpressionController>,
    /// Unprocessed parameter changes and note events sent by the host during a process call.
    /// Parameter changes are sent as separate queues for each parameter, and note events are in
    /// another queue on top of that. And if `P::MIDI_INPUT >= MidiConfig::MidiCCs`, then we can
    /// also receive MIDI CC messages through special parameter changes. On top of that, we also
    /// support sample accurate automation through block splitting if
    /// `P::SAMPLE_ACCURATE_AUTOMATION` is set. To account for all of this, we'll read all of the
    /// parameter changes and events into a vector at the start of the process call, sort it, and
    /// then do the block splitting based on that. Note events need to have their timing adjusted to
    /// match the block start, since they're all read upfront.
    pub process_events: AtomicRefCell<Vec<ProcessEvent<P>>>,
    // DELTON FORK DELTA
    pub raw_automation: AtomicRefCell<RawAutomation>,
    // DELTON FORK DELTA
    pub raw_automation_window: AtomicRefCell<RawAutomation>,
    /// The plugin is able to restore state through a method on the `GuiContext`. To avoid changing
    /// parameters mid-processing and running into garbled data if the host also tries to load state
    /// at the same time the restoring happens at the end of each processing call. If this zero
    /// capacity channel contains state data at that point, then the audio thread will take the
    /// state out of the channel, restore the state, and then send it back through the same channel.
    /// In other words, the GUI thread acts as a sender and then as a receiver, while the audio
    /// thread acts as a receiver and then as a sender. That way deallocation can happen on the GUI
    /// thread. All of this happens without any blocking on the audio thread.
    pub updated_state_sender: channel::Sender<PluginState>,
    /// The receiver belonging to [`new_state_sender`][Self::new_state_sender].
    pub updated_state_receiver: channel::Receiver<PluginState>,

    /// The keys from `param_map` in a stable order.
    pub param_hashes: Vec<u32>,
    /// A mapping from parameter ID hashes (obtained from the string parameter IDs) to pointers to
    /// parameters belonging to the plugin. These addresses will remain stable as long as the
    /// `params` object does not get deallocated.
    pub param_by_hash: HashMap<u32, ParamPtr>,
    /// Mappings from parameter hashes to string parameter IDs. Used for notifying the plugin's
    /// editor about parameter changes.
    pub param_id_by_hash: HashMap<u32, String>,
    pub param_units: ParamUnits,
    /// Mappings from string parameter identifiers to parameter hashes. Useful for debug logging
    /// and when storing and restoring plugin state.
    pub param_id_to_hash: HashMap<String, u32>,
    /// The inverse mapping from [`param_by_hash`][Self::param_by_hash]. This is needed to be able
    /// to have an ergonomic parameter setting API that uses references to the parameters instead of
    /// having to add a setter function to the parameter (or even worse, have it be completely
    /// untyped).
    pub param_ptr_to_hash: HashMap<ParamPtr, u32>,
}

/// Tasks that can be sent from the plugin to be executed on the main thread in a non-blocking
/// realtime-safe way (either a random thread or `IRunLoop` on Linux, the OS' message loop on
/// Windows and macOS).
#[allow(clippy::enum_variant_names)]
pub enum Task<P: Plugin> {
    /// Execute one of the plugin's background tasks.
    PluginTask(P::BackgroundTask),
    /// Inform the plugin that one or more parameter values have changed.
    #[cfg(feature = "editor")]
    StateChanged,
    /// Inform the plugin that one parameter's value has changed. This uses the parameter hashes
    /// since the task will be created from the audio thread.
    #[cfg(feature = "editor")]
    ParameterValueChanged(u32, f32),
    /// Trigger a restart with the given restart flags. This is a bit set of the flags from
    /// [`vst3::Steinberg::Vst::RestartFlags`].
    TriggerRestart(i32),
    /// The plugin wrote parameter values with no editor open: tell the host to re-read them
    /// (`restartComponent(kParamValuesChanged)`). Posted by the plugin's [`HostNotices`].
    // DELTON FORK DELTA: hpre-1
    ParamValuesNotice,
    /// The audio thread saw the program parameter change: load the latest program the host asked
    /// for, as a host load.
    // DELTON FORK DELTA: hpre-3
    ProgramChange,
    /// The plugin's program list changed: re-read it, then tell the host
    /// (`restartComponent(kParamTitlesChanged | kParamValuesChanged)` and
    /// `IUnitHandler::notifyProgramListChange`).
    // DELTON FORK DELTA: hpre-3
    ProgramListChanged,
    // Request the editor to be resized according to its current size. Right now there is no way to
    // handle "denied resize" requests yet.
    #[cfg(feature = "editor")]
    RequestResize { size: Size, scale_factor: f64 },
    #[cfg(feature = "editor")]
    CallMainThread,
}

/// VST3 makes audio processing pretty complicated. In order to support both block splitting for
/// sample accurate automation and MIDI CC handling through parameters we need to put all parameter
/// changes and (translated) note events into a sorted array first.
#[derive(Debug, PartialEq)]
pub enum ProcessEvent<P: Plugin> {
    /// An incoming parameter change sent by the host. This will only be used when sample accurate
    /// automation has been enabled, and the parameters are only updated when we process this
    /// spooled event at the start of a block.
    ParameterChange {
        /// The event's sample offset within the buffer. Used for sorting.
        timing: u32,
        /// The parameter's hash, as used everywhere else.
        hash: u32,
        /// The normalized values, as provided by the host.
        normalized_value: f32,
    },
    /// An incoming parameter change sent by the host. This will only be used when sample accurate
    /// automation has been enabled, and the parameters are only updated when we process this
    /// spooled event at the start of a block.
    ///
    /// The timing stored within the note event needs to have the block start index subtraced from
    /// it. make sure to subtract the block start index with [`NoteEvent::subtract_timing()`] before
    /// putting this into the input event queue.
    NoteEvent(PluginNoteEvent<P>),
}

impl<P: Plugin> ProcessEvent<P> {
    #[inline]
    pub(crate) fn timing(&self) -> u32 {
        match self {
            ProcessEvent::ParameterChange { timing, .. } => *timing,
            ProcessEvent::NoteEvent(event) => event.timing(),
        }
    }
}

/// The "no program change pending" value of [`WrapperInner::program_change_pending`]: no index has
/// all 64 bits set.
// DELTON FORK DELTA: hpre-3
pub const NO_PROGRAM_CHANGE: u64 = u64::MAX;

/// The program parameter's `stepCount` for a list: the number of programs minus one, 0 for a list
/// of one or none.
// DELTON FORK DELTA: hpre-3b
fn step_count_of(list: &ProgramList) -> i32 {
    list.names.len().saturating_sub(1) as i32
}

/// What a `program_change_pending` word asks of the plugin: with [`PLUGIN_LOAD`] set,
/// `program_change(index)` (a plugin load: the list NOW, the plugin tells the host); else a host
/// load, `load_program(index, revision)`, with the revision `cached_revision(index)` read the
/// list at (`None`: an empty list or an index it does not have, which loads nothing). An error
/// from a host load is logged, not shown.
// DELTON FORK DELTA: hpre-3b
fn load_pending_with(
    host_presets: &dyn HostPresets,
    pending: u64,
    cached_revision: impl FnOnce(usize) -> Option<u64>,
) {
    let index = (pending & !PLUGIN_LOAD) as usize;
    if pending & PLUGIN_LOAD != 0 {
        host_presets.program_change(index);
        return;
    }
    if let Some(revision) = cached_revision(index)
        && let Err(err) = host_presets.load_program(index, revision)
    {
        crate::nice_log!("The host's program change was not loaded: {}", err);
    }
}

/// The tag in [`WrapperInner::program_change_pending`] of a program change that is a plugin load
/// (a MIDI program change), not a host load (the program parameter).
// DELTON FORK DELTA: hpre-3b
pub const PLUGIN_LOAD: u64 = 1 << 62;

/// One change of a MIDI bank select / program change parameter, as `process` collects them.
// DELTON FORK DELTA: hpre-3b
#[derive(Debug, Clone, Copy)]
pub struct MidiPresetChange {
    /// The change's sample offset in the buffer.
    pub timing: u32,
    /// 0 bank MSB, 1 bank LSB, 2 program: the order of equal offsets, so a bank change at the
    /// same offset as a program change applies first.
    pub k: u8,
    pub channel: u8,
    /// The order the host handed the points in, the last tie-break.
    pub seq: u32,
    /// The parameter's normalized value.
    pub value: f64,
}

impl MidiPresetChange {
    /// The order changes apply in: sample offset, then `k`, then channel, then arrival.
    pub fn sort_key(&self) -> (u32, u8, u8, u32) {
        (self.timing, self.k, self.channel, self.seq)
    }
}

/// The VST3 end of the plugin's [`HostNotices`]: raises the wrapper's flag, and the call that
/// raised it posts one `ParamValuesNotice` task.
// DELTON FORK DELTA: hpre-1
struct Vst3HostNotices<P: Vst3Plugin> {
    wrapper: std::sync::Weak<WrapperInner<P>>,
}

impl<P: Vst3Plugin> HostNoticeSink for Vst3HostNotices<P> {
    fn param_values_changed(&self) {
        let Some(wrapper) = self.wrapper.upgrade() else {
            return;
        };

        if wrapper.param_values_notice.raise() {
            // `post_gui`, not `schedule_gui`: on the main thread `schedule_gui` would run the task
            // inline, once per notice
            let posted = match wrapper.event_loop.borrow().as_ref() {
                Some(event_loop) => event_loop.post_gui(Task::ParamValuesNotice),
                None => false,
            };
            if !posted {
                // The task queue is full and nothing is coming to clear the flag: clear it so the
                // next notice tries again
                wrapper.param_values_notice.take();
            }
        }
    }

    // DELTON FORK DELTA: hpre-2
    fn preset_loaded(&self, _path: &std::path::Path) {
        // VST3 has no preset-browser hook for a loaded file. The program parameter's value is the
        // current preset, so the host re-reads every value (hpre-3).
        self.param_values_changed();
    }

    // DELTON FORK DELTA: hpre-3
    fn program_list_changed(&self) {
        if let Some(wrapper) = self.wrapper.upgrade() {
            wrapper.post_program_list_changed();
        }
    }
}

impl<P: Vst3Plugin> WrapperInner<P> {
    #[allow(unused_unsafe)]
    pub fn new() -> Arc<Self> {
        let mut plugin = P::default();
        let task_executor = Mutex::new(plugin.task_executor());

        // This is used to allow the plugin to restore preset data from its editor, see the comment
        // on `Self::updated_state_sender`
        let (updated_state_sender, updated_state_receiver) = channel::bounded(0);

        // This is a mapping from the parameter IDs specified by the plugin to pointers to those
        // parameters. These pointers are assumed to be safe to dereference as long as
        // `wrapper.plugin` is alive. The plugin API identifiers these parameters by hashes, which
        // we'll calculate from the string ID specified by the plugin. These parameters should also
        // remain in the same order as the one returned by the plugin.
        let params = plugin.params();
        let param_id_hashes_ptrs_groups: Vec<_> = params
            .param_map()
            .into_iter()
            .map(|(id, ptr, group)| {
                let hash = hash_param_id(&id);
                (id, hash, ptr, group)
            })
            .collect();
        if cfg!(debug_assertions) {
            let param_map = params.param_map();
            let param_ids: HashSet<_> = param_id_hashes_ptrs_groups
                .iter()
                .map(|(id, _, _, _)| id.clone())
                .collect();
            crate::nice_debug_assert_eq!(
                param_map.len(),
                param_ids.len(),
                "The plugin has duplicate parameter IDs, weird things may happen. Consider using \
                 6 character parameter IDs to avoid collisions."
            );

            let mut bypass_param_exists = false;
            for (id, hash, ptr, _) in &param_id_hashes_ptrs_groups {
                let flags = unsafe { ptr.flags() };
                let is_bypass = flags.contains(ParamFlags::BYPASS);

                if is_bypass && bypass_param_exists {
                    crate::nice_debug_assert_failure!(
                        "Duplicate bypass parameters found, the host will only use the first one"
                    );
                }

                bypass_param_exists |= is_bypass;

                // DELTON FORK DELTA: hpre-3
                if *hash == VST3_PROGRAM_PARAM_ID {
                    crate::nice_debug_assert_failure!(
                        "Parameter '{}' collides with the generated program parameter, consider \
                         giving it a different ID",
                        id
                    );
                }

                // DELTON FORK DELTA: hpre-3b
                if (VST3_MIDI_PRESET_PARAMS_START..VST3_MIDI_PRESET_PARAMS_END).contains(hash) {
                    crate::nice_debug_assert_failure!(
                        "Parameter '{}' collides with a generated MIDI bank select / program \
                         change parameter, consider giving it a different ID",
                        id
                    );
                }

                if P::MIDI_INPUT >= MidiConfig::MidiCCs
                    && (VST3_MIDI_PARAMS_START..VST3_MIDI_PARAMS_END).contains(hash)
                {
                    crate::nice_debug_assert_failure!(
                        "Parameter '{}' collides with an automatically generated MIDI CC \
                         parameter, consider giving it a different ID",
                        id
                    );
                }
            }
        }

        let param_hashes = param_id_hashes_ptrs_groups
            .iter()
            .map(|(_, hash, _, _)| *hash)
            .collect();
        let param_by_hash = param_id_hashes_ptrs_groups
            .iter()
            .map(|(_, hash, ptr, _)| (*hash, *ptr))
            .collect();
        let param_id_by_hash = param_id_hashes_ptrs_groups
            .iter()
            .map(|(id, hash, _, _)| (*hash, id.clone()))
            .collect();
        let param_units = ParamUnits::from_param_groups(
            param_id_hashes_ptrs_groups
                .iter()
                .map(|(_, hash, _, group_name)| (*hash, group_name.as_str())),
        )
        .expect("Inconsistent parameter groups");
        let param_id_to_hash = param_id_hashes_ptrs_groups
            .iter()
            .map(|(id, hash, _, _)| (id.clone(), *hash))
            .collect();
        let param_ptr_to_hash = param_id_hashes_ptrs_groups
            .into_iter()
            .map(|(_, hash, ptr, _)| (ptr, hash))
            .collect();

        let input_note_events = if P::MIDI_INPUT == MidiConfig::None {
            AtomicRefCell::new(VecDeque::new())
        } else {
            AtomicRefCell::new(VecDeque::with_capacity(P::INPUT_EVENT_CAPACITY))
        };

        let wrapper = Arc::new(Self {
            plugin: TryLock::new(plugin),
            task_executor,
            params,
            // Initialized later as it needs a reference to the wrapper for the async executor
            #[cfg(feature = "editor")]
            editor: AtomicRefCell::new(None),
            #[cfg(feature = "editor")]
            editor_window: Arc::new(AtomicRefCell::new(None)),

            component_handler: AtomicRefCell::new(None),

            #[cfg(feature = "editor")]
            plug_view: RwLock::new(None),

            #[cfg(feature = "editor")]
            is_editor_open: AtomicBool::new(false),

            event_loop: AtomicRefCell::new(None),
            // DELTON FORK DELTA: hpre-1
            param_values_notice: NoticeFlag::default(),
            // DELTON FORK DELTA: hpre-3
            host_presets: OnceLock::new(),
            program_list: Mutex::new(None),
            program_change_pending: AtomicU64::new(NO_PROGRAM_CHANGE),
            // DELTON FORK DELTA: hpre-3b
            program_step_mirror: AtomicI32::new(0),
            midi_preset_params: AtomicBool::new(false),
            midi_bank_msb: std::array::from_fn(|_| AtomicU8::new(0)),
            midi_bank_lsb: std::array::from_fn(|_| AtomicU8::new(0)),
            // Room for every parameter's changes in a normal block; a larger block allocates
            // (permitted, as for `process_events`)
            midi_preset_changes: AtomicRefCell::new(Vec::with_capacity(
                VST3_MIDI_PRESET_NUM_PARAMS as usize * 4,
            )),
            program_change_notice: NoticeFlag::default(),
            program_list_notice: NoticeFlag::default(),

            is_processing: AtomicBool::new(false),
            // DELTON FORK DELTA
            is_activated: AtomicBool::new(false),
            // Some hosts, like the current version of Bitwig and Ardour at the time of writing,
            // will try using the plugin's default not yet initialized bus arrangement. Because of
            // that, we'll always initialize this configuration even before the host requests a
            // channel layout.
            current_audio_io_layout: AtomicCell::new(
                P::AUDIO_IO_LAYOUTS.first().copied().unwrap_or_default(),
            ),
            current_buffer_config: AtomicCell::new(None),
            current_process_mode: AtomicCell::new(ProcessMode::Realtime),
            #[cfg(feature = "editor")]
            current_track_info: AtomicRefCell::new(TrackInfo::default()),
            last_process_status: AtomicCell::new(ProcessStatus::Normal),
            current_latency: AtomicU32::new(0),
            // This is initialized just before calling `Plugin::activate()` so that during the
            // process call buffers can be initialized without any allocations
            buffer_manager: AtomicRefCell::new(BufferManager::for_audio_io_layout(
                0,
                AudioIOLayout::default(),
            )),
            input_note_events,
            note_expression_controller: AtomicRefCell::new(NoteExpressionController::default()),
            process_events: AtomicRefCell::new(Vec::with_capacity(P::INPUT_EVENT_CAPACITY)),
            // DELTON FORK DELTA
            raw_automation: AtomicRefCell::new(RawAutomation::new(if P::RAW_PARAM_AUTOMATION {
                P::RAW_PARAM_AUTOMATION_CAPACITY
            } else {
                0
            })),
            // DELTON FORK DELTA
            raw_automation_window: AtomicRefCell::new(RawAutomation::new(
                if P::RAW_PARAM_AUTOMATION {
                    P::RAW_PARAM_AUTOMATION_CAPACITY
                } else {
                    0
                },
            )),
            updated_state_sender,
            updated_state_receiver,

            param_hashes,
            param_by_hash,
            param_id_by_hash,
            param_units,
            param_id_to_hash,
            param_ptr_to_hash,
        });

        // FIXME: Right now this is safe, but if we are going to have a singleton main thread queue
        //        serving multiple plugin instances, Arc can't be used because its reference count
        //        is separate from the internal COM-style reference count.
        *wrapper.event_loop.borrow_mut() =
            Some(OsEventLoop::new_and_spawn(Arc::downgrade(&wrapper)));

        // Hand the plugin its channel to the host before anything else can run. The plugin lock is
        // free: nothing but this function has the wrapper yet.
        // DELTON FORK DELTA: hpre-1
        wrapper
            .plugin
            .try_lock()
            .unwrap()
            .attach_host_notices(HostNotices::new(Arc::new(Vst3HostNotices {
                wrapper: Arc::downgrade(&wrapper),
            })));

        // Asked once, as the CLAP wrapper does: a host load goes through this handle and never
        // locks the plugin, which the audio thread may hold
        // DELTON FORK DELTA: hpre-3
        if let Some(host_presets) = wrapper.plugin.try_lock().unwrap().host_presets() {
            // MIDI bank select and program change get hidden parameters of their own, so a MIDI
            // program change and the host's program menu are told apart. Read once.
            // DELTON FORK DELTA: hpre-3b
            wrapper.midi_preset_params.store(
                P::MIDI_INPUT >= MidiConfig::Basic && host_presets.midi_program_change(),
                Ordering::Relaxed,
            );
            let _ = wrapper.host_presets.set(host_presets);
        }

        // The editor also needs to be initialized later so the Async executor can work.

        #[cfg(feature = "editor")]
        {
            *wrapper.editor.borrow_mut() = wrapper
                .plugin
                .try_lock()
                .unwrap()
                .editor(nice_plug_core::context::gui::AsyncExecutor::new(
                    Arc::new({
                        let wrapper = Arc::downgrade(&wrapper);
                        move |task| {
                            let wrapper = match wrapper.upgrade() {
                                Some(wrapper) => wrapper,
                                None => return,
                            };

                            let task_posted = wrapper.schedule_background(Task::PluginTask(task));
                            crate::nice_debug_assert!(
                                task_posted,
                                "The task queue is full, dropping task..."
                            );
                        }
                    }),
                    Arc::new({
                        let wrapper = Arc::downgrade(&wrapper);
                        move |task| {
                            let wrapper = match wrapper.upgrade() {
                                Some(wrapper) => wrapper,
                                None => return,
                            };

                            let task_posted = wrapper.schedule_gui(Task::PluginTask(task));
                            crate::nice_debug_assert!(
                                task_posted,
                                "The task queue is full, dropping task..."
                            );
                        }
                    }),
                ))
                .map(|editor| Arc::new(Mutex::new(editor)));
        }

        wrapper
    }

    #[cfg(feature = "editor")]
    pub fn make_gui_context(self: Arc<Self>) -> GuiContext {
        GuiContext::new(Arc::new(WrapperGuiContext {
            inner: Arc::downgrade(&self),
            #[cfg(debug_assertions)]
            param_gesture_checker: Default::default(),
        }))
    }

    /// # Note
    ///
    /// The lock on the plugin must be dropped before this object is dropped to avoid deadlocks
    /// caused by reentrant function calls.
    pub fn make_activate_context(&self) -> WrapperActivateContext<'_, P> {
        WrapperActivateContext {
            inner: self,
            pending_requests: Default::default(),
        }
    }

    pub fn make_process_context<'a>(
        &'a self,
        transport: Transport,
        host_out_events: Option<ComRef<'a, IEventList>>,
        total_buffer_len: usize,
        current_sample_idx: usize,
    ) -> WrapperProcessContext<'a, P> {
        WrapperProcessContext {
            inner: self,
            input_events_guard: self.input_note_events.borrow_mut(),
            // DELTON FORK DELTA
            raw_automation_guard: if P::SAMPLE_ACCURATE_AUTOMATION {
                self.raw_automation_window.borrow()
            } else {
                self.raw_automation.borrow()
            },
            transport,
            host_out_events,
            total_buffer_len: total_buffer_len as u32,
            current_sample_idx: current_sample_idx as u32,
        }
    }

    /// Whether the plugin gave a [`HostPresets`], that is, whether this wrapper has a program list
    /// and a program parameter.
    // DELTON FORK DELTA: hpre-3
    pub fn has_programs(&self) -> bool {
        self.host_presets.get().is_some()
    }

    /// Runs `f` on the cached program list, reading it from the plugin first when it was never
    /// read. `None` without [`HostPresets`]. Main thread only.
    // DELTON FORK DELTA: hpre-3
    pub fn with_program_list<R>(&self, f: impl FnOnce(&ProgramList) -> R) -> Option<R> {
        let host_presets = self.host_presets.get()?;
        if let Some(list) = &*self.program_list.lock() {
            return Some(f(list));
        }
        // Read outside the lock: the plugin may tell us about a change while it answers
        let fresh = host_presets.program_list();
        let mut cached = self.program_list.lock();
        let list = cached.get_or_insert(fresh);
        self.program_step_mirror
            .store(step_count_of(list), Ordering::Relaxed);
        Some(f(list))
    }

    /// Whether the 48 hidden MIDI bank select / program change parameters exist.
    // DELTON FORK DELTA: hpre-3b
    pub fn has_midi_preset_params(&self) -> bool {
        self.has_programs() && self.midi_preset_params.load(Ordering::Relaxed)
    }

    /// Prime the cached program list (and with it the step count the audio thread reads), if it
    /// was never read. Main thread only.
    // DELTON FORK DELTA: hpre-3b
    pub fn prime_program_list(&self) {
        let _ = self.with_program_list(|_| ());
    }

    /// The program parameter's `stepCount`: the number of programs minus one, 0 for a list of one
    /// or none.
    // DELTON FORK DELTA: hpre-3
    pub fn program_step_count(&self) -> i32 {
        self.with_program_list(step_count_of).unwrap_or(0)
    }

    /// Read the program list from the plugin again into the cache. Returns whether it differed
    /// from the cached one (a first read does not count). Does not tell the host. Main thread only.
    // DELTON FORK DELTA: hpre-3
    pub fn reread_program_list(&self) -> bool {
        let Some(host_presets) = self.host_presets.get() else {
            return false;
        };
        let fresh = host_presets.program_list();
        let mut cached = self.program_list.lock();
        let changed = cached.as_ref().is_some_and(|old| *old != fresh);
        self.program_step_mirror
            .store(step_count_of(&fresh), Ordering::Relaxed);
        *cached = Some(fresh);
        changed
    }

    /// Read the program list again (the host is asking for it, which is when a listing that
    /// changed behind the plugin's back shows). When it differs from the cached one the host is
    /// told. Main thread only.
    // DELTON FORK DELTA: hpre-3
    pub fn refresh_program_list(&self) {
        if self.reread_program_list() {
            self.post_program_list_changed();
        }
    }

    /// Post one `ProgramListChanged` task, however many notices come before it runs. Callable from
    /// any thread.
    // DELTON FORK DELTA: hpre-3
    pub fn post_program_list_changed(&self) {
        if self.program_list_notice.raise() {
            // `post_gui`, never inline: see `Vst3HostNotices::param_values_changed()`
            let posted = match self.event_loop.borrow().as_ref() {
                Some(event_loop) => event_loop.post_gui(Task::ProgramListChanged),
                None => false,
            };
            if !posted {
                self.program_list_notice.take();
            }
        }
    }

    /// Called on the audio thread with the program parameter's last normalized value in a block:
    /// turns it into an index with the step count the main thread last mirrored, and makes it the
    /// pending program change as a HOST load. Allocates nothing.
    // DELTON FORK DELTA: hpre-3
    pub fn post_program_change(&self, value: f64) {
        if !value.is_finite() {
            return;
        }
        let step_count = self.program_step_mirror.load(Ordering::Relaxed);
        self.post_program_index(program_from_normalized(value, step_count) as u64);
    }

    /// Makes `pending` (an index, with [`PLUGIN_LOAD`] set for a plugin load) the pending program
    /// change, latest wins from any source, and posts one `ProgramChange` task for a burst.
    /// Allocates nothing.
    // DELTON FORK DELTA: hpre-3b
    pub fn post_program_index(&self, pending: u64) {
        self.program_change_pending
            .store(pending, Ordering::Release);
        if self.program_change_notice.raise() {
            // `post_gui`, never inline: the load runs on the main thread, never in `process`
            let posted = match self.event_loop.borrow().as_ref() {
                Some(event_loop) => event_loop.post_gui(Task::ProgramChange),
                None => false,
            };
            if !posted {
                // The queue is full: clear the flag so the next change tries again
                self.program_change_notice.take();
            }
        }
    }

    /// Apply the collected changes of the MIDI bank select / program change parameters, in sample
    /// offset order, then `k` (a bank change at the same offset as a program change applies
    /// first), then channel, then arrival. A bank parameter sets that channel's MSB / LSB; a
    /// program parameter makes `program_index(msb, lsb, program)` the pending program change, as a
    /// PLUGIN load. Audio thread; allocates nothing.
    // DELTON FORK DELTA: hpre-3b
    pub fn apply_midi_preset_changes(&self, changes: &mut [MidiPresetChange]) {
        changes.sort_unstable_by_key(MidiPresetChange::sort_key);
        for change in changes.iter() {
            let channel = (change.channel & 0x0f) as usize;
            let value = midi_value_from_normalized(change.value);
            match change.k {
                0 => self.midi_bank_msb[channel].store(value, Ordering::Relaxed),
                1 => self.midi_bank_lsb[channel].store(value, Ordering::Relaxed),
                _ => {
                    let index = program_index(
                        self.midi_bank_msb[channel].load(Ordering::Relaxed),
                        self.midi_bank_lsb[channel].load(Ordering::Relaxed),
                        value,
                    );
                    self.post_program_index(index as u64 | PLUGIN_LOAD);
                }
            }
        }
    }

    /// Load the program a program parameter value selects, as a host load. Does nothing for an
    /// empty list or an index the cached list does not have. An error from the plugin is logged,
    /// not shown. Main thread only.
    // DELTON FORK DELTA: hpre-3
    pub fn load_program_from_value(&self, value: f64) {
        let step_count = self.program_step_count();
        self.load_pending_program(program_from_normalized(value, step_count) as u64);
    }

    /// Load the program change a `program_change_pending` word holds: a host load of that index of
    /// the cached list (nothing for an empty list or an index it does not have), or, with
    /// [`PLUGIN_LOAD`] set, a plugin load of it (the plugin ignores an index past its list). An
    /// error from the plugin is logged, not shown. Main thread only.
    // DELTON FORK DELTA: hpre-3b
    pub fn load_pending_program(&self, pending: u64) {
        let Some(host_presets) = self.host_presets.get() else {
            return;
        };
        load_pending_with(host_presets.as_ref(), pending, |index| {
            self.with_program_list(|list| (index < list.names.len()).then_some(list.revision))
                .flatten()
        });
    }

    /// Posts the task to the background task queue using [`EventLoop::schedule_background()`] so it
    /// can be run in the background without blocking either the GUI or the audio thread.
    ///
    /// If the task queue is full, then this will return false.
    #[must_use]
    pub fn schedule_background(&self, task: Task<P>) -> bool {
        let event_loop = self.event_loop.borrow();
        let event_loop = event_loop.as_ref().unwrap();
        event_loop.schedule_background(task)
    }

    /// Either posts the task to the task queue using [`EventLoop::schedule_gui()`] so it can be
    /// delegated to the main thread, executes the task directly if this is the main thread, or runs
    /// the task on the host's `IRunLoop` if the GUI is open and it exposes one.
    ///
    /// If the task queue is full, then this will return false.
    #[must_use]
    pub fn schedule_gui(&self, task: Task<P>) -> bool {
        let event_loop = self.event_loop.borrow();
        let event_loop = event_loop.as_ref().unwrap();
        if event_loop.is_main_thread() {
            self.execute(task, true);
            true
        } else {
            // If the editor is open, and the host exposes the `IRunLoop` interface, then we'll run
            // the task on the host's GUI thread using that interface. Otherwise we'll use the
            // regular event loop. If the editor gets dropped while there's still outstanding work
            // left in the run loop task queue, then those tasks will be posted to the regular event
            // loop so no work is lost.
            #[cfg(feature = "editor")]
            if self.is_editor_open.load(Ordering::Relaxed) {
                match self
                    .plug_view
                    .read()
                    .clone()
                    .unwrap()
                    .do_maybe_in_run_loop(task)
                {
                    Ok(()) => true,
                    Err(task) => event_loop.schedule_gui(task),
                }
            } else {
                event_loop.schedule_gui(task)
            }

            #[cfg(not(feature = "editor"))]
            event_loop.schedule_gui(task)
        }
    }

    /// Get a parameter's ID based on a `ParamPtr`. Used in the `GuiContext` implementation for the
    /// gesture checks.
    #[allow(unused)]
    pub fn param_id_from_ptr(&self, param: ParamPtr) -> Option<&str> {
        self.param_ptr_to_hash
            .get(&param)
            .and_then(|hash| self.param_id_by_hash.get(hash))
            .map(|s| s.as_str())
    }

    /// Convenience function for setting a value for a parameter as triggered by a VST3 parameter
    /// update. The same rate is for updating parameter smoothing.
    ///
    /// After calling this function, you should call
    /// [`notify_param_values_changed()`][Self::notify_param_values_changed()] to allow the editor
    /// to update itself. This needs to be done separately so you can process parameter changes in
    /// batches.
    pub fn set_normalized_value_by_hash(
        &self,
        hash: u32,
        normalized_value: f32,
        sample_rate: Option<f32>,
    ) -> tresult {
        if !normalized_value.is_finite() {
            return kInvalidArgument;
        }

        match self.param_by_hash.get(&hash) {
            Some(param_ptr) => {
                if unsafe { param_ptr._internal_set_normalized_value(normalized_value) } {
                    if let Some(sample_rate) = sample_rate {
                        unsafe { param_ptr._internal_update_smoother(sample_rate, false) };
                    }

                    #[cfg(feature = "editor")]
                    {
                        let task_posted =
                            self.schedule_gui(Task::ParameterValueChanged(hash, normalized_value));
                        crate::nice_debug_assert!(
                            task_posted,
                            "The task queue is full, dropping task..."
                        );
                    }
                }

                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    /// Get the plugin's state object, may be called by the plugin's GUI as part of its own preset
    /// management. The wrapper doesn't use these functions and serializes and deserializes directly
    /// the JSON in the relevant plugin API methods instead.
    #[cfg(feature = "editor")]
    pub fn get_state_object(&self) -> PluginState {
        unsafe {
            state::serialize_object::<P>(
                self.params.clone(),
                state::make_params_iter(&self.param_by_hash, &self.param_id_to_hash),
            )
        }
    }

    /// Update the plugin's internal state, called by the plugin itself from the GUI thread. To
    /// prevent corrupting data and changing parameters during processing the actual state is only
    /// updated at the end of the audio processing cycle.
    #[cfg(feature = "editor")]
    pub fn set_state_object_from_gui(&self, mut state: PluginState) {
        use crossbeam::channel::SendTimeoutError;

        // Use a loop and timeouts to handle the super rare edge case when this function gets called
        // between a process call and the host disabling the plugin
        loop {
            if self.is_processing.load(Ordering::SeqCst) {
                // If the plugin is currently processing audio, then we'll perform the restore
                // operation at the end of the audio call. This involves sending the state to the
                // audio thread, having the audio thread handle the state restore at the very end of
                // the process function, and then sending the state back to this thread so it can be
                // deallocated without blocking the audio thread.
                match self
                    .updated_state_sender
                    .send_timeout(state, std::time::Duration::from_secs(1))
                {
                    Ok(_) => {
                        // As mentioned above, the state object will be passed back to this thread
                        // so we can deallocate it without blocking.
                        let state = self.updated_state_receiver.recv();
                        drop(state);
                        break;
                    }
                    Err(SendTimeoutError::Timeout(value)) => {
                        state = value;
                        continue;
                    }
                    Err(SendTimeoutError::Disconnected(_)) => {
                        crate::nice_debug_assert_failure!("State update channel got disconnected");
                        return;
                    }
                }
            } else {
                // Otherwise we'll set the state right here and now, since this function should be
                // called from a GUI thread
                self.set_state_inner(&mut state);
                break;
            }
        }

        // After the state has been updated, notify the host about the new parameter values
        let task_posted = self
            .event_loop
            .borrow()
            .as_ref()
            .unwrap()
            .schedule_gui(Task::TriggerRestart(RestartFlags_::kParamValuesChanged));
        crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    pub fn set_latency_samples(&self, samples: u32) {
        // Only trigger a restart if it's actually needed
        let old_latency = self.current_latency.swap(samples, Ordering::SeqCst);
        if old_latency != samples {
            self.request_restart();
        }
    }

    pub fn request_restart(&self) {
        let task_posted = self.schedule_gui(Task::TriggerRestart(RestartFlags_::kLatencyChanged));
        crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    /// Immediately set the plugin state. Returns `false` if the deserialization failed. The plugin
    /// state is set from a couple places, so this function aims to deduplicate that. Includes
    /// `permit_alloc()`s around the deserialization and initialization for the use case where
    /// `set_state_object_from_gui()` was called while the plugin is process audio.
    ///
    /// Implicitly emits `Task::ParameterValuesChanged`.
    ///
    /// # Notes
    ///
    /// `self.plugin` must _not_ be locked while calling this function or it will deadlock.
    pub fn set_state_inner(&self, state: &mut PluginState) -> bool {
        // FIXME: This is obviously not realtime-safe, but loading presets without doing this
        //        could lead to inconsistencies. `state::deserialize_object()` normally never
        //        allocates, but if the plugin has persistent non-parameter data then its
        //        `deserialize_fields()` implementation may still allocate.
        let success = permit_alloc(|| unsafe {
            state::deserialize_object::<P>(
                state,
                self.params.clone(),
                state::make_params_getter(&self.param_by_hash, &self.param_id_to_hash),
                self.current_buffer_config.load().as_ref(),
            )
        });
        if !success {
            crate::nice_debug_assert_failure!(
                "Deserializing plugin state from a state object failed"
            );
            return false;
        }

        #[cfg(feature = "editor")]
        {
            // Reinitialize the plugin after loading state so it can respond to the new parameter values
            let task_posted = self.schedule_gui(Task::StateChanged);
            crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
        }

        success
    }
}

impl<P: Vst3Plugin> MainThreadExecutor<Task<P>> for WrapperInner<P> {
    fn execute(&self, task: Task<P>, is_gui_thread: bool) {
        // This function is always called from the main thread
        match task {
            Task::PluginTask(task) => (self.task_executor.lock())(task),
            #[cfg(feature = "editor")]
            Task::StateChanged => {
                use nice_plug_core::editor::EditorHandle;

                if let Some(editor_window) = self.editor_window.borrow().as_ref() {
                    editor_window.get().handle.state_changed();
                }
            }
            #[cfg(feature = "editor")]
            Task::ParameterValueChanged(param_hash, normalized_value) => {
                use nice_plug_core::editor::EditorHandle;

                if let Some(editor_window) = self.editor_window.borrow().as_ref() {
                    let param_id = &self.param_id_by_hash[&param_hash];
                    editor_window
                        .get()
                        .handle
                        .param_value_changed(param_id, normalized_value);
                }
            }
            // DELTON FORK DELTA: hpre-1
            Task::ParamValuesNotice => {
                // Take the flag *before* the host re-reads the values, so a write that lands after
                // this raises it again and posts a new task
                self.param_values_notice.take();
                // Without a handler the host has not connected yet and reads every value when it
                // does, so there is nothing to tell it
                if let Some(handler) = &*self.component_handler.borrow() {
                    unsafe {
                        crate::nice_debug_assert!(is_gui_thread);
                        let result = handler.restartComponent(RestartFlags_::kParamValuesChanged);
                        crate::nice_debug_assert_eq!(
                            result,
                            kResultOk,
                            "Failed the parameter values changed restart request"
                        );
                    }
                }
            }
            // DELTON FORK DELTA: hpre-3
            Task::ProgramChange => {
                // Clear the flag *before* taking the value, so a change that lands after the take
                // raises it again and posts a new task
                self.program_change_notice.take();
                let pending = self
                    .program_change_pending
                    .swap(NO_PROGRAM_CHANGE, Ordering::AcqRel);
                if pending != NO_PROGRAM_CHANGE {
                    self.load_pending_program(pending);
                }
            }
            // DELTON FORK DELTA: hpre-3
            Task::ProgramListChanged => {
                self.program_list_notice.take();
                self.reread_program_list();
                if let Some(handler) = &*self.component_handler.borrow() {
                    unsafe {
                        crate::nice_debug_assert!(is_gui_thread);
                        // The parameter's step count and the program names changed, and so may
                        // its value (the current preset's place in the list)
                        let result = handler.restartComponent(
                            RestartFlags_::kParamTitlesChanged | RestartFlags_::kParamValuesChanged,
                        );
                        crate::nice_debug_assert_eq!(
                            result,
                            kResultOk,
                            "Failed the program list changed restart request"
                        );
                        if let Some(unit_handler) = handler.cast::<IUnitHandler>() {
                            unit_handler.notifyProgramListChange(PROGRAM_LIST_ID, -1);
                        }
                    }
                }
            }
            Task::TriggerRestart(flags) => match &*self.component_handler.borrow() {
                Some(handler) => unsafe {
                    crate::nice_debug_assert!(is_gui_thread);
                    let result = handler.restartComponent(flags);
                    crate::nice_debug_assert_eq!(
                        result,
                        kResultOk,
                        "Failed the restart request call with flags '{:?}'",
                        flags
                    );
                },
                None => crate::nice_debug_assert_failure!("Component handler not yet set"),
            },
            #[cfg(feature = "editor")]
            Task::RequestResize { size, scale_factor } => {
                if self.is_editor_open.load(Ordering::SeqCst) {
                    unsafe {
                        crate::nice_debug_assert!(is_gui_thread);
                        let _success = WrapperView::request_resize(
                            &self.plug_view.read().clone().unwrap(),
                            size,
                            scale_factor,
                        );
                    }
                } else {
                    crate::nice_debug_assert_failure!("Can't resize a closed editor");
                }
            }
            #[cfg(feature = "editor")]
            Task::CallMainThread => {
                use nice_plug_core::editor::EditorHandle;

                if let Some(editor_window) = self.editor_window.borrow().as_ref() {
                    let editor_window = editor_window.get();
                    editor_window
                        .handle
                        .host_main_thread_callback(&editor_window.window)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Records which load the wrapper asked for.
    #[derive(Default)]
    struct Recorder {
        calls: StdMutex<Vec<String>>,
    }

    impl HostPresets for Recorder {
        fn load_file(&self, _path: &std::path::Path) -> Result<(), String> {
            Ok(())
        }
        fn program_list(&self) -> ProgramList {
            ProgramList::default()
        }
        fn load_program(&self, index: usize, revision: u64) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("host load {index} at {revision}"));
            Ok(())
        }
        fn current_program(&self) -> Option<usize> {
            None
        }
        fn midi_program_change(&self) -> bool {
            true
        }
        fn program_change(&self, index: usize) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("plugin load {index}"));
        }
    }

    #[test]
    fn program_pending_plugin_load_is_a_plugin_load_not_a_host_load() {
        let recorder = Recorder::default();
        // A MIDI program change: the list now, no revision, whatever the cached list says
        load_pending_with(&recorder, 133 | PLUGIN_LOAD, |_| None);
        load_pending_with(&recorder, 16_512 | PLUGIN_LOAD, |_| Some(9));
        assert_eq!(
            *recorder.calls.lock().unwrap(),
            ["plugin load 133", "plugin load 16512"]
        );
    }

    #[test]
    fn program_pending_host_load_names_the_cached_revision() {
        let recorder = Recorder::default();
        load_pending_with(&recorder, 139, |index| (index < 140).then_some(7));
        // An index the cached list does not have loads nothing
        load_pending_with(&recorder, 140, |index| (index < 140).then_some(7));
        assert_eq!(*recorder.calls.lock().unwrap(), ["host load 139 at 7"]);
    }

    #[test]
    fn program_pending_tag_never_collides_with_none_or_an_index() {
        assert_ne!(PLUGIN_LOAD, 0);
        assert_ne!(NO_PROGRAM_CHANGE, PLUGIN_LOAD);
        // The largest index a flat bank reaches
        let largest = program_index(127, 127, 127) as u64;
        assert_eq!(largest & PLUGIN_LOAD, 0);
        assert_ne!(largest | PLUGIN_LOAD, NO_PROGRAM_CHANGE);
    }
}
