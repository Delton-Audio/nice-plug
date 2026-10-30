// DELTON FORK DELTA: au-1, AUv2 wrapper
//! `Wrapper<P>`: one AUv2 instance. Its lifecycle, properties, parameters, render and listeners.
//!
//! Threading: the host may call property and parameter methods from any thread, and `Render` from
//! the audio thread. The render path never takes a mutex, never allocates and never calls
//! CoreFoundation; everything it shares with the other threads is atomic or an `AtomicCell`.

use atomic_refcell::AtomicRefCell;
use crossbeam::atomic::AtomicCell;
use crossbeam::channel;
use crossbeam::queue::ArrayQueue;
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use fragile::Fragile;
use nice_plug_core::audio_setup::{AudioIOLayout, AuxiliaryBuffers, BufferConfig, ProcessMode};
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use nice_plug_core::context::gui::{AsyncExecutor, GuiContext};
use nice_plug_core::context::process::{ParamAutomationPoint, Transport};
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use nice_plug_core::editor::dpi::{NativeSize, Size};
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use nice_plug_core::editor::{Editor, EditorHandle, SpawnedEditor};
use nice_plug_core::params::internals::ParamPtr;
use nice_plug_core::params::{ParamFlags, Params};
use nice_plug_core::plugin::{PluginState, ProcessStatus, TaskExecutor};
use parking_lot::Mutex;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_void};
use std::mem::{size_of, zeroed};
use std::ptr::{self, NonNull};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use try_lock::TryLock;

use super::buffers::AuBuffers;
use super::context::{WrapperActivateContext, WrapperProcessContext};
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use super::context::WrapperGuiContext;
use super::convert::{seconds_from_samples, tail_seconds};
use super::layout::{
    au_layouts, channels_allowed, element_channels, pick_layout, supported_channels, ChannelPair,
    ElementShape,
};
use super::param_table::{au_from_normalized, normalized_from_au, AuParamKind, AuParamTable};
use super::render_check::{check_render, RenderCheck, RenderError};
use super::state;
use super::sys::*;
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use super::view;
use super::AuPlugin;
use crate::event_loop::{EventLoop, MainThreadExecutor, OsEventLoop};
use crate::util::permit_alloc;
use crate::wrapper::util::buffer_management::BufferManager;
use crate::wrapper::util::process_wrapper;
use crate::wrapper::util::raw_automation::RawAutomation;

/// Apple's default `MaximumFramesPerSlice`.
const DEFAULT_MAX_FRAMES: u32 = 1156;
/// The sample rate before the host sets one.
const DEFAULT_SAMPLE_RATE: f64 = 44100.0;
/// How many property listeners one instance keeps.
const PROPERTY_LISTENER_CAPACITY: usize = 32;
/// How many render notifies one instance keeps.
const RENDER_NOTIFY_SLOTS: usize = 8;
/// The `kRenderQuality_Max` value: what `RenderQuality` reports.
const RENDER_QUALITY: u32 = kRenderQuality_Max;

/// Tasks that can be sent from the plugin or the host threads to be executed on the main thread in
/// a non-blocking realtime-safe way (the OS' message loop on macOS).
#[allow(clippy::enum_variant_names)]
pub enum Task<P: AuPlugin> {
    /// Execute one of the plugin's background tasks.
    PluginTask(P::BackgroundTask),
    /// One parameter's value was changed by the host or the editor. The editor is told.
    // DELTON FORK DELTA: au-2
    #[cfg_attr(not(feature = "editor"), allow(dead_code))]
    ParameterValueChanged(u32, f32),
    /// Parameter values changed all at once (a state load). Tells the host's parameter listeners
    /// and the editor.
    StateChanged,
    /// The plugin's latency changed: tells the `Latency` property listeners.
    LatencyChanged,
    /// The editor asked for a new size: resize its container (main thread).
    // DELTON FORK DELTA: au-2
    #[cfg(feature = "editor")]
    RequestResize { size: Size, scale_factor: f64 },
    /// The editor's `HostMainThreadCaller` fired.
    // DELTON FORK DELTA: au-2
    #[cfg(feature = "editor")]
    CallMainThread,
}

/// The open editor and the container view it lives in.
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
pub(super) struct EditorSlot<P: AuPlugin> {
    /// The container NSView (`*mut AnyObject` as usize). Not retained: the container's dealloc
    /// closes this slot before the container goes away.
    pub container: usize,
    /// Whether `viewDidMoveToWindow` hid the editor.
    pub hidden: Cell<bool>,
    pub spawned: SpawnedEditor<<P::Editor as Editor>::Handle>,
}

/// The format of one stream (one input or output element).
#[derive(Debug, Clone, Copy)]
struct StreamFormat {
    sample_rate: f64,
    channels: u32,
}

/// The formats of every element. Only the property functions and Initialize touch this.
struct Formats {
    inputs: Vec<StreamFormat>,
    outputs: Vec<StreamFormat>,
}

/// Where an input element gets its audio from.
#[derive(Clone, Copy)]
enum InputSource {
    None,
    /// A render callback the host set.
    Callback {
        proc: RenderProc,
        refcon: usize,
    },
    /// A connection to another unit's output element. `unit` is the source `AudioUnit` handle.
    Connection {
        unit: usize,
        output: u32,
    },
}

/// The host callbacks for transport information. Pointers are kept as `usize` so this is `Send`.
#[derive(Clone, Copy, Default)]
struct HostCallbacks {
    user_data: usize,
    beat_and_tempo: HostCallback_GetBeatAndTempo,
    musical_time_location: HostCallback_GetMusicalTimeLocation,
    transport_state2: HostCallback_GetTransportState2,
}

/// One property listener.
#[derive(Clone, Copy)]
struct PropertyListener {
    id: AudioUnitPropertyID,
    proc: PropertyListenerProc,
    user_data: usize,
}

/// A fixed list of render notifies. The render thread reads it without locking: a slot is a pair of
/// atomics, written refcon first then proc when added, and proc first then refcon when removed.
///
/// A notify removed while a render is in flight may be called once more after `RemoveRenderNotify`
/// returned. That is accepted: the host removes a notify before it frees what the refcon points at,
/// as with every AU.
struct RenderNotifyList {
    /// Serializes `add` and `remove`. The render thread never takes it.
    writers: Mutex<()>,
    procs: [AtomicPtr<c_void>; RENDER_NOTIFY_SLOTS],
    refcons: [AtomicPtr<c_void>; RENDER_NOTIFY_SLOTS],
    generation: AtomicU32,
}

impl RenderNotifyList {
    fn new() -> Self {
        Self {
            writers: Mutex::new(()),
            procs: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            refcons: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            generation: AtomicU32::new(0),
        }
    }

    /// Returns false if all slots are taken.
    fn add(&self, proc: RenderProc, refcon: *mut c_void) -> bool {
        let _guard = self.writers.lock();
        for (slot_proc, slot_refcon) in self.procs.iter().zip(self.refcons.iter()) {
            if slot_proc.load(Ordering::Acquire).is_null() {
                slot_refcon.store(refcon, Ordering::Release);
                slot_proc.store(proc as *mut c_void, Ordering::Release);
                self.generation.fetch_add(1, Ordering::Release);
                return true;
            }
        }

        false
    }

    fn remove(&self, proc: RenderProc, refcon: *mut c_void) {
        let _guard = self.writers.lock();
        for (slot_proc, slot_refcon) in self.procs.iter().zip(self.refcons.iter()) {
            if slot_proc.load(Ordering::Acquire) == proc as *mut c_void
                && slot_refcon.load(Ordering::Acquire) == refcon
            {
                slot_proc.store(ptr::null_mut(), Ordering::Release);
                slot_refcon.store(ptr::null_mut(), Ordering::Release);
                self.generation.fetch_add(1, Ordering::Release);
            }
        }
    }

    /// Call every notify. Lock-free.
    ///
    /// # Safety
    ///
    /// The arguments are passed through to the host's callbacks.
    unsafe fn call(
        &self,
        flags: &mut AudioUnitRenderActionFlags,
        timestamp: *const AudioTimeStamp,
        bus: u32,
        frames: u32,
        io_data: *mut AudioBufferList,
    ) {
        for (slot_proc, slot_refcon) in self.procs.iter().zip(self.refcons.iter()) {
            let proc = slot_proc.load(Ordering::Acquire);
            if proc.is_null() {
                continue;
            }
            let refcon = slot_refcon.load(Ordering::Acquire);

            // SAFETY: Only `add` stores into `procs`, and it stores a `RenderProc`.
            let proc = unsafe { std::mem::transmute::<*mut c_void, RenderProc>(proc) };
            // SAFETY: The notify is the host's; the arguments are the ones render received.
            unsafe { proc(refcon, flags, timestamp, bus, frames, io_data) };
        }
    }
}

/// Write a property value into the host's buffer.
///
/// # Safety
///
/// `out` must have room for a `T`.
unsafe fn put_value<T>(out: *mut c_void, value: T) {
    // SAFETY: Forwarded from the caller. Hosts do not promise alignment for property buffers.
    unsafe { out.cast::<T>().write_unaligned(value) };
}

/// Which property a property ID is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prop {
    ClassInfo,
    ClassInfoFromDocument,
    StreamFormat,
    SampleRate,
    ElementCount,
    SupportedNumChannels,
    MaximumFramesPerSlice,
    Latency,
    TailTime,
    ParameterList,
    ParameterInfo,
    ParameterValueStrings,
    ParameterStringFromValue,
    ParameterValueFromString,
    ParameterClumpName,
    SetRenderCallback,
    MakeConnection,
    HostCallbacks,
    InPlaceProcessing,
    ShouldAllocateBuffer,
    BypassEffect,
    LastRenderError,
    FactoryPresets,
    PresentPreset,
    ElementName,
    RenderQuality,
    OfflineRender,
    Private,
    // DELTON FORK DELTA: au-2
    #[cfg(feature = "editor")]
    CocoaUI,
}

/// The scopes a property exists in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeRule {
    /// Global scope only.
    Global,
    /// Input or output scope, with an element.
    InputOrOutput,
    /// Input scope only, with an element.
    Input,
    /// Global, input or output scope.
    Any,
}

impl Prop {
    // The property IDs are Apple's `kAudioUnitProperty_*` constants, used as patterns
    #[allow(non_upper_case_globals)]
    fn from_id(id: AudioUnitPropertyID) -> Option<Self> {
        Some(match id {
            kAudioUnitProperty_ClassInfo => Prop::ClassInfo,
            kAudioUnitProperty_ClassInfoFromDocument => Prop::ClassInfoFromDocument,
            kAudioUnitProperty_StreamFormat => Prop::StreamFormat,
            kAudioUnitProperty_SampleRate => Prop::SampleRate,
            kAudioUnitProperty_ElementCount => Prop::ElementCount,
            kAudioUnitProperty_SupportedNumChannels => Prop::SupportedNumChannels,
            kAudioUnitProperty_MaximumFramesPerSlice => Prop::MaximumFramesPerSlice,
            kAudioUnitProperty_Latency => Prop::Latency,
            kAudioUnitProperty_TailTime => Prop::TailTime,
            kAudioUnitProperty_ParameterList => Prop::ParameterList,
            kAudioUnitProperty_ParameterInfo => Prop::ParameterInfo,
            kAudioUnitProperty_ParameterValueStrings => Prop::ParameterValueStrings,
            kAudioUnitProperty_ParameterStringFromValue => Prop::ParameterStringFromValue,
            kAudioUnitProperty_ParameterValueFromString => Prop::ParameterValueFromString,
            kAudioUnitProperty_ParameterClumpName => Prop::ParameterClumpName,
            kAudioUnitProperty_SetRenderCallback => Prop::SetRenderCallback,
            kAudioUnitProperty_MakeConnection => Prop::MakeConnection,
            kAudioUnitProperty_HostCallbacks => Prop::HostCallbacks,
            kAudioUnitProperty_InPlaceProcessing => Prop::InPlaceProcessing,
            kAudioUnitProperty_ShouldAllocateBuffer => Prop::ShouldAllocateBuffer,
            kAudioUnitProperty_BypassEffect => Prop::BypassEffect,
            kAudioUnitProperty_LastRenderError => Prop::LastRenderError,
            kAudioUnitProperty_FactoryPresets => Prop::FactoryPresets,
            kAudioUnitProperty_PresentPreset => Prop::PresentPreset,
            kAudioUnitProperty_ElementName => Prop::ElementName,
            kAudioUnitProperty_RenderQuality => Prop::RenderQuality,
            kAudioUnitProperty_OfflineRender => Prop::OfflineRender,
            PRIVATE_WRAPPER_PROPERTY => Prop::Private,
            // DELTON FORK DELTA: au-2
            #[cfg(feature = "editor")]
            kAudioUnitProperty_CocoaUI => Prop::CocoaUI,
            _ => return None,
        })
    }

    fn scope_rule(self) -> ScopeRule {
        match self {
            Prop::StreamFormat
            | Prop::SampleRate
            | Prop::ElementName
            | Prop::ShouldAllocateBuffer => ScopeRule::InputOrOutput,
            Prop::SetRenderCallback | Prop::MakeConnection => ScopeRule::Input,
            Prop::ElementCount => ScopeRule::Any,
            _ => ScopeRule::Global,
        }
    }

    /// Whether `SetProperty` may ever succeed for this property.
    fn writable(self) -> bool {
        matches!(
            self,
            Prop::ClassInfo
                | Prop::ClassInfoFromDocument
                | Prop::StreamFormat
                | Prop::SampleRate
                | Prop::MaximumFramesPerSlice
                | Prop::SetRenderCallback
                | Prop::MakeConnection
                | Prop::HostCallbacks
                | Prop::InPlaceProcessing
                | Prop::ShouldAllocateBuffer
                | Prop::BypassEffect
                | Prop::PresentPreset
                | Prop::RenderQuality
                | Prop::OfflineRender
        )
    }
}

/// The actual wrapper bits. We need this as an `Arc<T>` so we can safely use our event loop API.
pub(crate) struct Wrapper<P: AuPlugin> {
    /// The AudioUnit handle `Open` received. Listeners need it.
    unit: UnitHandle,
    /// This wrapper, for the view factory (which only gets a pointer through the private property).
    // DELTON FORK DELTA: au-2
    #[cfg_attr(not(feature = "editor"), allow(dead_code))]
    pub(super) this: Weak<Self>,

    /// The wrapped plugin instance.
    pub(super) plugin: TryLock<P>,
    /// The plugin's background task executor closure.
    pub(super) task_executor: Mutex<TaskExecutor<P>>,
    /// The plugin's parameters. These are fetched once during initialization. That way the
    /// `ParamPtr`s are guaranteed to live at least as long as this object and we can interact with
    /// the `Params` object without having to acquire a lock on `plugin`.
    pub(super) params: Arc<dyn Params>,
    /// The AU view of the parameters.
    pub(super) table: AuParamTable,
    /// The ID of the first parameter flagged `BYPASS`, if any.
    bypass_id: Option<u32>,
    /// One `CFString` per table entry, made at Open, that `ParameterInfo` hands out as
    /// `cfNameString` (without `CFNameRelease`, so the host never releases them). These are
    /// addresses of retained `CFString`s, released when the wrapper drops. They are `usize` so the
    /// wrapper stays `Send + Sync`.
    cf_names: Vec<usize>,
    /// Parameter hash to parameter, for state and for the smoothers.
    pub(super) param_by_hash: HashMap<u32, ParamPtr>,
    /// String parameter ID to hash, for state.
    pub(super) param_id_to_hash: HashMap<String, u32>,
    /// ParamPtr to hash, for the GUI context.
    // DELTON FORK DELTA: au-2
    #[cfg_attr(not(feature = "editor"), allow(dead_code))]
    pub(super) param_ptr_to_hash: HashMap<ParamPtr, u32>,
    /// The plugin's editor, created at Open. Wrapped in an `AtomicRefCell` because it needs the
    /// `Arc` to exist first.
    // DELTON FORK DELTA: au-2
    #[cfg(feature = "editor")]
    pub(super) editor: AtomicRefCell<Option<Arc<Mutex<P::Editor>>>>,
    /// The open editor, if any, and the container view it lives in. Only touched on the main
    /// thread, and only through `try_borrow` and `try_borrow_mut`.
    // DELTON FORK DELTA: au-2
    #[cfg(feature = "editor")]
    pub(super) editor_slot: AtomicRefCell<Option<Fragile<EditorSlot<P>>>>,

    /// A realtime-safe task queue so the wrapper can schedule tasks that need to run on the main
    /// thread. Posted to through [`Self::schedule_gui()`]. Initialized late: it needs the `Arc`.
    event_loop: AtomicRefCell<Option<OsEventLoop<Task<P>, Self>>>,

    // Configuration: only changed while uninitialized, read by render.
    /// The element structure shared by every layout this unit offers.
    shape: ElementShape,
    /// The current format of each element.
    formats: Mutex<Formats>,
    /// The current sample rate, as `f64` bits, for the property getters that must not lock.
    sample_rate_bits: AtomicU64,
    /// The channel count of the main output, which a render's buffer list must match.
    render_channels: AtomicU32,
    max_frames: AtomicU32,
    initialized: AtomicBool,
    current_layout: AtomicCell<Option<AudioIOLayout>>,
    pub(super) current_buffer_config: AtomicCell<Option<BufferConfig>>,
    pub(super) current_process_mode: AtomicCell<ProcessMode>,
    bypassed: AtomicBool,
    /// `ShouldAllocateBuffer` for the input and output scope.
    should_allocate: [AtomicU32; 2],
    pub(super) present_preset_name: Mutex<String>,
    last_render_error: AtomicI32,
    last_logged_error: AtomicUsize,
    last_process_status: AtomicCell<ProcessStatus>,
    pub(super) current_latency: AtomicU32,

    /// The source of each input element.
    input_sources: Vec<AtomicCell<InputSource>>,
    host_callbacks: AtomicCell<HostCallbacks>,
    property_listeners: Mutex<Vec<PropertyListener>>,
    render_notifies: RenderNotifyList,

    /// Allocated at Initialize.
    buffers: AtomicRefCell<AuBuffers>,
    buffer_manager: AtomicRefCell<BufferManager>,

    /// Host parameter changes `(offset, hash, normalized)` waiting for the next render.
    param_events: ArrayQueue<(u32, u32, f32)>,
    automation_overflowed: AtomicBool,
    raw_automation: AtomicRefCell<RawAutomation>,

    /// The plugin is able to restore state through a method on the `GuiContext`. To avoid changing
    /// parameters mid-processing, the restoring happens at the end of each render: the audio thread
    /// takes the state out of this zero capacity channel, restores it, and sends it back so it is
    /// deallocated on the GUI thread. The editor sends.
    // DELTON FORK DELTA: au-2
    #[cfg_attr(not(feature = "editor"), allow(dead_code))]
    updated_state_sender: channel::Sender<PluginState>,
    updated_state_receiver: channel::Receiver<PluginState>,
}

impl<P: AuPlugin> Wrapper<P> {
    /// Build the wrapper for `Open`. Fails if the plugin has no audio layout AU can offer.
    pub(crate) fn new(unit: AudioComponentInstance) -> Result<Arc<Self>, OSStatus> {
        let layouts_offered = au_layouts(P::AUDIO_IO_LAYOUTS);
        let Some(&first_idx) = layouts_offered.first() else {
            crate::nice_error!("AU: the plugin has no audio IO layout");
            return Err(PARAM_ERR);
        };
        let first_layout = P::AUDIO_IO_LAYOUTS[first_idx];
        let shape = ElementShape::of(&first_layout);
        let (input_channels, output_channels) = element_channels(&first_layout);
        if shape.aux_outputs > 0 {
            crate::nice_warn!(
                "AU: auxiliary outputs are not supported yet; the plugin's auxiliary outputs get \
                 scratch buffers"
            );
        }

        let mut plugin = P::default();
        let task_executor = Mutex::new(plugin.task_executor());

        // This is used to allow the plugin to restore preset data from its editor, see the comment
        // on `Self::updated_state_sender`
        let (updated_state_sender, updated_state_receiver) = channel::bounded(0);

        let params = plugin.params();
        let param_map = params.param_map();
        if cfg!(debug_assertions) {
            let param_ids: HashSet<_> = param_map.iter().map(|(id, _, _)| id.clone()).collect();
            crate::nice_debug_assert_eq!(
                param_map.len(),
                param_ids.len(),
                "The plugin has duplicate parameter IDs, weird things may happen. Consider using \
                 6 character parameter IDs to avoid collisions."
            );

            let mut bypass_param_exists = false;
            for (_, ptr, _) in &param_map {
                let is_bypass = unsafe { ptr.flags() }.contains(ParamFlags::BYPASS);
                if is_bypass && bypass_param_exists {
                    crate::nice_debug_assert_failure!(
                        "Duplicate bypass parameters found, the host will only use the first one"
                    );
                }
                bypass_param_exists |= is_bypass;
            }
        }

        // SAFETY: The pointers come from `params`, which this wrapper keeps alive.
        let table = unsafe { AuParamTable::build(&param_map) };
        let cf_names = table
            .entries
            .iter()
            .map(|entry| CFRetained::into_raw(CFString::from_str(&entry.name)).as_ptr() as usize)
            .collect();
        let param_by_hash = table
            .entries
            .iter()
            .map(|entry| (entry.id, entry.ptr))
            .collect();
        let param_id_to_hash = table
            .entries
            .iter()
            .map(|entry| (entry.string_id.clone(), entry.id))
            .collect();
        // DELTON FORK DELTA: au-2
        let param_ptr_to_hash = table
            .entries
            .iter()
            .map(|entry| (entry.ptr, entry.id))
            .collect();
        let bypass_id = table.bypass_id();

        let make_formats = |channels: &[u32]| -> Vec<StreamFormat> {
            channels
                .iter()
                .map(|&channels| StreamFormat {
                    sample_rate: DEFAULT_SAMPLE_RATE,
                    channels,
                })
                .collect()
        };
        let formats = Formats {
            inputs: make_formats(&input_channels),
            outputs: make_formats(&output_channels),
        };

        let event_capacity = if P::INPUT_EVENT_CAPACITY == 0 {
            512
        } else {
            P::INPUT_EVENT_CAPACITY
        };

        // DELTON FORK DELTA: au-2 (`this` is for the view factory)
        let wrapper = Arc::new_cyclic(|this| Self {
            unit: UnitHandle(unit),
            this: this.clone(),
            plugin: TryLock::new(plugin),
            task_executor,
            params,
            table,
            bypass_id,
            cf_names,
            param_by_hash,
            param_id_to_hash,
            param_ptr_to_hash,
            #[cfg(feature = "editor")]
            editor: AtomicRefCell::new(None),
            #[cfg(feature = "editor")]
            editor_slot: AtomicRefCell::new(None),
            event_loop: AtomicRefCell::new(None),

            input_sources: (0..shape.input_elements())
                .map(|_| AtomicCell::new(InputSource::None))
                .collect(),
            shape,
            formats: Mutex::new(formats),
            sample_rate_bits: AtomicU64::new(DEFAULT_SAMPLE_RATE.to_bits()),
            render_channels: AtomicU32::new(output_channels.first().copied().unwrap_or(0)),
            max_frames: AtomicU32::new(DEFAULT_MAX_FRAMES),
            initialized: AtomicBool::new(false),
            current_layout: AtomicCell::new(None),
            current_buffer_config: AtomicCell::new(None),
            current_process_mode: AtomicCell::new(ProcessMode::Realtime),
            bypassed: AtomicBool::new(false),
            should_allocate: [AtomicU32::new(1), AtomicU32::new(1)],
            present_preset_name: Mutex::new(String::from("Untitled")),
            last_render_error: AtomicI32::new(NO_ERR),
            last_logged_error: AtomicUsize::new(0),
            last_process_status: AtomicCell::new(ProcessStatus::Normal),
            current_latency: AtomicU32::new(0),

            host_callbacks: AtomicCell::new(HostCallbacks::default()),
            property_listeners: Mutex::new(Vec::with_capacity(PROPERTY_LISTENER_CAPACITY)),
            render_notifies: RenderNotifyList::new(),

            // These are replaced at Initialize, when the channel counts are known
            buffers: AtomicRefCell::new(AuBuffers::new(&[], &[], 0)),
            buffer_manager: AtomicRefCell::new(BufferManager::for_audio_io_layout(
                0,
                AudioIOLayout::default(),
            )),

            param_events: ArrayQueue::new(event_capacity),
            automation_overflowed: AtomicBool::new(false),
            // DELTON FORK DELTA: raw parameter automation, as in the other wrappers
            raw_automation: AtomicRefCell::new(RawAutomation::new(if P::RAW_PARAM_AUTOMATION {
                P::RAW_PARAM_AUTOMATION_CAPACITY
            } else {
                0
            })),

            updated_state_sender,
            updated_state_receiver,
        });

        // The event loop needs the `Arc` to hold a weak reference to the wrapper
        *wrapper.event_loop.borrow_mut() =
            Some(OsEventLoop::new_and_spawn(Arc::downgrade(&wrapper)));

        // The editor also needs to be initialized late so the async executor can work
        // DELTON FORK DELTA: au-2
        #[cfg(feature = "editor")]
        {
            *wrapper.editor.borrow_mut() = wrapper
                .plugin
                .try_lock()
                .unwrap()
                .editor(AsyncExecutor::new(
                    Arc::new({
                        let wrapper = Arc::downgrade(&wrapper);
                        move |task| {
                            let Some(wrapper) = wrapper.upgrade() else {
                                return;
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
                            let Some(wrapper) = wrapper.upgrade() else {
                                return;
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

        Ok(wrapper)
    }

    // ----------------------------------------------------------------------------------------
    // Tasks
    // ----------------------------------------------------------------------------------------

    /// Posts the task to the background task queue using [`EventLoop::schedule_background()`] so it
    /// can be run in the background without blocking either the GUI or the audio thread.
    ///
    /// If the task queue is full, then this will return false.
    #[must_use]
    pub(super) fn schedule_background(&self, task: Task<P>) -> bool {
        let event_loop = self.event_loop.borrow();
        match event_loop.as_ref() {
            Some(event_loop) => event_loop.schedule_background(task),
            None => false,
        }
    }

    /// Either posts the task to the task queue using [`EventLoop::schedule_gui()`] so it can be
    /// delegated to the main thread, or executes the task directly if this is the main thread.
    ///
    /// If the task queue is full, then this will return false.
    #[must_use]
    pub(super) fn schedule_gui(&self, task: Task<P>) -> bool {
        let event_loop = self.event_loop.borrow();
        match event_loop.as_ref() {
            Some(event_loop) => event_loop.schedule_gui(task),
            None => false,
        }
    }

    /// The plugin's new latency. Tells the `Latency` property listeners if it changed.
    pub(super) fn set_latency_samples(&self, samples: u32) {
        let old_latency = self.current_latency.swap(samples, Ordering::SeqCst);
        if old_latency != samples {
            let task_posted = self.schedule_gui(Task::LatencyChanged);
            crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
        }
    }

    /// Immediately set the plugin state. Returns `false` if the deserialization failed. Includes
    /// `permit_alloc()`s around the deserialization for the case where the editor restores state
    /// while the plugin is processing audio.
    ///
    /// Implicitly emits `Task::StateChanged`.
    ///
    /// # Notes
    ///
    /// `self.plugin` must _not_ be locked while calling this function or it will deadlock.
    pub(super) fn set_state_inner(&self, state: &mut PluginState) -> bool {
        // FIXME: This is obviously not realtime-safe, but loading presets without doing this
        //        could lead to inconsistencies. `state::deserialize_object()` normally never
        //        allocates, but if the plugin has persistent non-parameter data then its
        //        `deserialize_fields()` implementation may still allocate.
        let success = permit_alloc(|| unsafe {
            crate::wrapper::state::deserialize_object::<P>(
                state,
                self.params.clone(),
                crate::wrapper::state::make_params_getter(
                    &self.param_by_hash,
                    &self.param_id_to_hash,
                ),
                self.current_buffer_config.load().as_ref(),
            )
        });
        if !success {
            crate::nice_debug_assert_failure!(
                "Deserializing plugin state from a state object failed"
            );
            return false;
        }

        // The host's parameter listeners and, with au-2, the editor need to know about all of the
        // new values
        let task_posted = self.schedule_gui(Task::StateChanged);
        crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");

        success
    }

    // ----------------------------------------------------------------------------------------
    // Lifecycle
    // ----------------------------------------------------------------------------------------

    fn sample_rate(&self) -> f64 {
        f64::from_bits(self.sample_rate_bits.load(Ordering::Relaxed))
    }

    /// `AudioUnitInitialize`.
    pub(crate) fn initialize(&self) -> OSStatus {
        if self.initialized.load(Ordering::Acquire) {
            return NO_ERR;
        }

        // Every element runs at one sample rate
        let (input_channels, output_channels, sample_rate) = {
            let formats = self.formats.lock();
            let mut rates = formats
                .inputs
                .iter()
                .chain(formats.outputs.iter())
                .map(|format| format.sample_rate);
            let first = rates.next().unwrap_or(DEFAULT_SAMPLE_RATE);
            if rates.any(|rate| rate != first) {
                return kAudioUnitErr_FormatNotSupported;
            }

            (
                formats
                    .inputs
                    .iter()
                    .map(|f| f.channels)
                    .collect::<Vec<_>>(),
                formats
                    .outputs
                    .iter()
                    .map(|f| f.channels)
                    .collect::<Vec<_>>(),
                first,
            )
        };

        let Some(layout_idx) = pick_layout(P::AUDIO_IO_LAYOUTS, &input_channels, &output_channels)
        else {
            return kAudioUnitErr_FormatNotSupported;
        };
        let layout = P::AUDIO_IO_LAYOUTS[layout_idx];

        let max_frames = self.max_frames.load(Ordering::Relaxed);
        let buffer_config = BufferConfig {
            sample_rate: sample_rate as f32,
            min_buffer_size: None,
            max_buffer_size: max_frames,
            process_mode: self.current_process_mode.load(),
        };

        // Everything the render path needs is allocated here, before the plugin activates, and is
        // installed only once the plugin has accepted the configuration
        let mut pending_buffers = Some((
            AuBuffers::new(&input_channels, &output_channels, max_frames as usize),
            BufferManager::for_audio_io_layout(max_frames as usize, layout),
        ));

        // Before initializing the plugin, make sure all smoothers are set to the default values
        for param in self.param_by_hash.values() {
            unsafe { param._internal_update_smoother(buffer_config.sample_rate, true) };
        }

        self.current_layout.store(Some(layout));
        self.current_buffer_config.store(Some(buffer_config));

        let mut activate_context = WrapperActivateContext {
            inner: self,
            latency_changed: Cell::new(false),
        };

        // In the case a host misbehaves and tries to initialize the unit without waiting for a
        // render to finish, manually wait for that render to finish.
        let now = Instant::now();
        let mut result = kAudioUnitErr_FailedInitialization;
        loop {
            if let Some(mut plugin) = self.plugin.try_lock() {
                if plugin.activate(&layout, &buffer_config, &mut activate_context) {
                    // Likewise, make sure that the buffers are not currently being used by a
                    // render before replacing them
                    let now_2 = Instant::now();
                    loop {
                        if let (Ok(mut buffers), Ok(mut buffer_manager)) = (
                            self.buffers.try_borrow_mut(),
                            self.buffer_manager.try_borrow_mut(),
                        ) {
                            if let Some((new_buffers, new_manager)) = pending_buffers.take() {
                                *buffers = new_buffers;
                                *buffer_manager = new_manager;
                            }
                            self.render_channels.store(
                                output_channels.first().copied().unwrap_or(0),
                                Ordering::Relaxed,
                            );
                            self.sample_rate_bits
                                .store(sample_rate.to_bits(), Ordering::Relaxed);

                            // `activate()` is always followed by `reset()`
                            process_wrapper(|| plugin.reset());
                            self.last_process_status.store(ProcessStatus::Normal);
                            self.initialized.store(true, Ordering::Release);
                            result = NO_ERR;
                            break;
                        } else if now_2.elapsed() > Duration::from_secs(1) {
                            crate::nice_error!(
                                "Failed to acquire lock on buffers while activating"
                            );
                            break;
                        } else {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                }

                break;
            } else if now.elapsed() > Duration::from_secs(1) {
                crate::nice_error!("Failed to acquire lock on plugin while activating");
                break;
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        // NOTE: The plugin lock was dropped with the loop above, so the latency announcement cannot
        //       deadlock on a listener that calls back in
        if result == NO_ERR && activate_context.latency_changed.get() {
            let task_posted = self.schedule_gui(Task::LatencyChanged);
            crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
        }

        result
    }

    /// `AudioUnitUninitialize`.
    pub(crate) fn uninitialize(&self) -> OSStatus {
        // A concurrent render sees this first and returns `Uninitialized`
        if !self.initialized.swap(false, Ordering::AcqRel) {
            return NO_ERR;
        }

        // In the case a host misbehaves and tries to uninitialize the unit without waiting for a
        // render to finish, manually wait for that render to finish.
        let now = Instant::now();
        loop {
            if let Some(mut plugin) = self.plugin.try_lock() {
                plugin.deactivate();
                break;
            } else if now.elapsed() > Duration::from_secs(1) {
                crate::nice_error!("Failed to acquire lock on plugin while deactivating");
                break;
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        self.last_process_status.store(ProcessStatus::Normal);

        NO_ERR
    }

    /// `AudioUnitReset`.
    pub(crate) fn reset(&self) -> OSStatus {
        if !self.initialized.load(Ordering::Acquire) {
            return NO_ERR;
        }

        process_wrapper(|| {
            let now = Instant::now();
            loop {
                if let Some(mut plugin) = self.plugin.try_lock() {
                    plugin.reset();
                    break;
                } else if now.elapsed() > Duration::from_millis(200) {
                    crate::nice_error!("Failed to acquire lock on plugin while resetting");
                    break;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });

        if let Some(buffer_config) = self.current_buffer_config.load() {
            for param in self.param_by_hash.values() {
                unsafe { param._internal_update_smoother(buffer_config.sample_rate, true) };
            }
        }
        self.last_process_status.store(ProcessStatus::Normal);

        NO_ERR
    }

    // ----------------------------------------------------------------------------------------
    // Parameters
    // ----------------------------------------------------------------------------------------

    /// `AudioUnitGetParameter`.
    pub(crate) unsafe fn get_parameter(
        &self,
        id: AudioUnitParameterID,
        scope: AudioUnitScope,
        out_value: *mut f32,
    ) -> OSStatus {
        if scope != kAudioUnitScope_Global {
            return kAudioUnitErr_InvalidScope;
        }
        let Some(entry) = self.table.get(id) else {
            return kAudioUnitErr_InvalidParameter;
        };
        if out_value.is_null() {
            return PARAM_ERR;
        }

        let normalized = unsafe { entry.ptr.unmodulated_normalized_value() };
        // SAFETY: Checked for null above; the host owns the pointer for this call.
        unsafe { out_value.write_unaligned(au_from_normalized(entry.kind, normalized)) };

        NO_ERR
    }

    /// `AudioUnitSetParameter`.
    pub(crate) fn set_parameter(
        &self,
        id: AudioUnitParameterID,
        scope: AudioUnitScope,
        value: f32,
        offset: u32,
    ) -> OSStatus {
        if scope != kAudioUnitScope_Global {
            return kAudioUnitErr_InvalidScope;
        }
        let Some(entry) = self.table.get(id) else {
            return kAudioUnitErr_InvalidParameter;
        };

        self.apply_host_parameter(
            entry.id,
            entry.ptr,
            normalized_from_au(entry.kind, value),
            offset,
        );

        NO_ERR
    }

    /// Set a parameter the way a host change must be applied: now, to the parameter and its
    /// smoother, with a note for the editor and for the next render's raw automation. This never
    /// notifies the host's parameter listeners: the host that set the value does that itself.
    fn apply_host_parameter(&self, hash: u32, ptr: ParamPtr, normalized: f32, offset: u32) {
        let initialized = self.initialized.load(Ordering::Acquire);

        if unsafe { ptr._internal_set_normalized_value(normalized) } {
            let sample_rate = self
                .current_buffer_config
                .load()
                .map(|buffer_config| buffer_config.sample_rate)
                .filter(|_| initialized);
            if let Some(sample_rate) = sample_rate {
                unsafe { ptr._internal_update_smoother(sample_rate, false) };
            }

            let task_posted = self.schedule_gui(Task::ParameterValueChanged(hash, normalized));
            crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
        }

        if initialized {
            // NOTE: `force_push()` would drop the oldest event
            if self.param_events.push((offset, hash, normalized)).is_err() {
                self.automation_overflowed.store(true, Ordering::Relaxed);
            }
        }
    }

    /// `AudioUnitScheduleParameters`. Ramped events are skipped: no parameter sets `CanRamp`.
    pub(crate) unsafe fn schedule_parameters(
        &self,
        events: *const AudioUnitParameterEvent,
        count: u32,
    ) -> OSStatus {
        if count == 0 {
            return NO_ERR;
        }
        if events.is_null() {
            return PARAM_ERR;
        }

        for i in 0..count as usize {
            // SAFETY: The host passes `count` events.
            let event = unsafe { events.add(i).read_unaligned() };
            if event.event_type == AUParameterEventType::ParameterEvent_Immediate.0 {
                // SAFETY: An immediate event uses the `immediate` half of the union.
                let immediate = unsafe { event.values.immediate };
                // A bad event does not stop the rest
                let _ = self.set_parameter(
                    event.parameter,
                    event.scope,
                    immediate.value,
                    immediate.buffer_offset,
                );
            }
        }

        NO_ERR
    }

    // ----------------------------------------------------------------------------------------
    // Property listeners
    // ----------------------------------------------------------------------------------------

    /// `AudioUnitAddPropertyListener`.
    pub(crate) fn add_property_listener(
        &self,
        id: AudioUnitPropertyID,
        proc: Option<PropertyListenerProc>,
        user_data: usize,
    ) -> OSStatus {
        let Some(proc) = proc else {
            return PARAM_ERR;
        };

        let mut listeners = self.property_listeners.lock();
        if listeners.len() >= PROPERTY_LISTENER_CAPACITY {
            return kAudioUnitErr_CannotDoInCurrentContext;
        }
        listeners.push(PropertyListener {
            id,
            proc,
            user_data,
        });

        NO_ERR
    }

    /// `AudioUnitRemovePropertyListener` (`user_data` is `None`) and
    /// `AudioUnitRemovePropertyListenerWithUserData`.
    pub(crate) fn remove_property_listener(
        &self,
        id: AudioUnitPropertyID,
        proc: Option<PropertyListenerProc>,
        user_data: Option<usize>,
    ) -> OSStatus {
        let Some(proc) = proc else {
            return PARAM_ERR;
        };

        self.property_listeners.lock().retain(|listener| {
            !(listener.id == id
                && listener.proc as usize == proc as usize
                && user_data.is_none_or(|user_data| listener.user_data == user_data))
        });

        NO_ERR
    }

    /// Call every listener of property `id`. The matching entries are copied out first and called
    /// after the mutex is released, because a listener may add or remove listeners.
    fn notify_property(&self, id: AudioUnitPropertyID, scope: AudioUnitScope, element: u32) {
        let matching: Vec<PropertyListener> = self
            .property_listeners
            .lock()
            .iter()
            .filter(|listener| listener.id == id)
            .copied()
            .collect();

        for listener in matching {
            // SAFETY: The listener is the host's; it gets the arguments the AU API documents.
            unsafe {
                (listener.proc)(
                    listener.user_data as *mut c_void,
                    self.unit.0,
                    id,
                    scope,
                    element,
                )
            };
        }
    }

    /// `AudioUnitAddRenderNotify`.
    pub(crate) fn add_render_notify(
        &self,
        proc: Option<RenderProc>,
        refcon: *mut c_void,
    ) -> OSStatus {
        let Some(proc) = proc else {
            return PARAM_ERR;
        };

        if self.render_notifies.add(proc, refcon) {
            NO_ERR
        } else {
            kAudioUnitErr_CannotDoInCurrentContext
        }
    }

    /// `AudioUnitRemoveRenderNotify`.
    pub(crate) fn remove_render_notify(
        &self,
        proc: Option<RenderProc>,
        refcon: *mut c_void,
    ) -> OSStatus {
        let Some(proc) = proc else {
            return PARAM_ERR;
        };

        self.render_notifies.remove(proc, refcon);

        NO_ERR
    }

    // ----------------------------------------------------------------------------------------
    // Properties
    // ----------------------------------------------------------------------------------------

    fn element_count(&self, scope: AudioUnitScope) -> u32 {
        if scope == kAudioUnitScope_Input {
            self.shape.input_elements()
        } else if scope == kAudioUnitScope_Output {
            self.shape.output_elements()
        } else {
            1
        }
    }

    /// Check the scope and element of a property, and say which property it is.
    fn validate_property(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) -> Result<Prop, OSStatus> {
        let prop = Prop::from_id(id).ok_or(kAudioUnitErr_InvalidProperty)?;

        let is_input_or_output = scope == kAudioUnitScope_Input || scope == kAudioUnitScope_Output;
        match prop.scope_rule() {
            ScopeRule::Global => {
                if scope != kAudioUnitScope_Global {
                    return Err(kAudioUnitErr_InvalidScope);
                }
            }
            ScopeRule::InputOrOutput => {
                if !is_input_or_output {
                    return Err(kAudioUnitErr_InvalidScope);
                }
                if element >= self.element_count(scope) {
                    return Err(kAudioUnitErr_InvalidElement);
                }
            }
            ScopeRule::Input => {
                if scope != kAudioUnitScope_Input {
                    return Err(kAudioUnitErr_InvalidScope);
                }
                if element >= self.element_count(scope) {
                    return Err(kAudioUnitErr_InvalidElement);
                }
            }
            ScopeRule::Any => {
                if scope != kAudioUnitScope_Global && !is_input_or_output {
                    return Err(kAudioUnitErr_InvalidScope);
                }
            }
        }

        Ok(prop)
    }

    /// The size in bytes of a property's value, and whether it can be set.
    fn property_size(
        &self,
        prop: Prop,
        element: AudioUnitElement,
    ) -> Result<(u32, bool), OSStatus> {
        let size = match prop {
            Prop::ClassInfo | Prop::ClassInfoFromDocument => size_of::<*const c_void>(),
            Prop::StreamFormat => size_of::<AudioStreamBasicDescription>(),
            Prop::SampleRate => size_of::<f64>(),
            Prop::ElementCount
            | Prop::MaximumFramesPerSlice
            | Prop::InPlaceProcessing
            | Prop::ShouldAllocateBuffer
            | Prop::BypassEffect
            | Prop::LastRenderError
            | Prop::RenderQuality
            | Prop::OfflineRender => size_of::<u32>(),
            Prop::SupportedNumChannels => {
                supported_channels(P::AUDIO_IO_LAYOUTS).len() * size_of::<AUChannelInfo>()
            }
            Prop::Latency | Prop::TailTime => size_of::<f64>(),
            Prop::ParameterList => self.table.listed_ids().len() * size_of::<u32>(),
            Prop::ParameterInfo => {
                if self.table.get(element).is_none() {
                    return Err(kAudioUnitErr_InvalidParameter);
                }
                size_of::<AudioUnitParameterInfo>()
            }
            Prop::ParameterValueStrings => match self.table.get(element) {
                None => return Err(kAudioUnitErr_InvalidParameter),
                Some(entry) => match entry.kind {
                    AuParamKind::Indexed { .. } => size_of::<*const c_void>(),
                    _ => return Err(kAudioUnitErr_InvalidProperty),
                },
            },
            Prop::ParameterStringFromValue => size_of::<ParameterStringFromValue>(),
            Prop::ParameterValueFromString => size_of::<ParameterValueFromString>(),
            Prop::ParameterClumpName => size_of::<AudioUnitParameterNameInfo>(),
            Prop::SetRenderCallback => size_of::<RenderCallbackStruct>(),
            Prop::MakeConnection => size_of::<AudioUnitConnection>(),
            Prop::HostCallbacks => size_of::<HostCallbackInfo>(),
            Prop::FactoryPresets | Prop::ElementName | Prop::Private => size_of::<*const c_void>(),
            Prop::PresentPreset => size_of::<AUPreset>(),
            // DELTON FORK DELTA: au-2
            #[cfg(feature = "editor")]
            Prop::CocoaUI => {
                if self.editor.borrow().is_none() {
                    return Err(kAudioUnitErr_InvalidProperty);
                }
                size_of::<AudioUnitCocoaViewInfo>()
            }
        };

        Ok((size as u32, prop.writable()))
    }

    /// `AudioUnitGetPropertyInfo`.
    pub(crate) unsafe fn get_property_info(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        out_size: *mut u32,
        out_writable: *mut u8,
    ) -> OSStatus {
        let prop = match self.validate_property(id, scope, element) {
            Ok(prop) => prop,
            Err(status) => return status,
        };
        let (size, writable) = match self.property_size(prop, element) {
            Ok(info) => info,
            Err(status) => return status,
        };

        // SAFETY: The host passes valid pointers or null for these.
        unsafe {
            if !out_size.is_null() {
                out_size.write_unaligned(size);
            }
            if !out_writable.is_null() {
                out_writable.write(writable as u8);
            }
        }

        NO_ERR
    }

    fn stream_format_description(
        &self,
        scope: AudioUnitScope,
        element: u32,
    ) -> AudioStreamBasicDescription {
        let formats = self.formats.lock();
        let list = if scope == kAudioUnitScope_Input {
            &formats.inputs
        } else {
            &formats.outputs
        };
        let format = list.get(element as usize).copied().unwrap_or(StreamFormat {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: 0,
        });

        AudioStreamBasicDescription {
            mSampleRate: format.sample_rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | kAudioFormatFlagIsNonInterleaved,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: format.channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }

    /// The name of an element, from the layout's port names.
    fn element_name(&self, scope: AudioUnitScope, element: u32) -> Option<String> {
        let layout = self
            .current_layout
            .load()
            .unwrap_or_else(|| P::AUDIO_IO_LAYOUTS[au_layouts(P::AUDIO_IO_LAYOUTS)[0]]);

        let element = element as usize;
        if scope == kAudioUnitScope_Input {
            if layout.main_input_channels.is_some() {
                if element == 0 {
                    Some(layout.main_input_name())
                } else {
                    layout.aux_input_name(element - 1)
                }
            } else {
                layout.aux_input_name(element)
            }
        } else if layout.main_output_channels.is_some() {
            if element == 0 {
                Some(layout.main_output_name())
            } else {
                layout.aux_output_name(element - 1)
            }
        } else {
            layout.aux_output_name(element)
        }
    }

    /// The current AU value of a parameter.
    fn parameter_au_value(&self, id: u32) -> Option<f32> {
        let entry = self.table.get(id)?;
        let normalized = unsafe { entry.ptr.unmodulated_normalized_value() };
        Some(au_from_normalized(entry.kind, normalized))
    }

    /// `AudioUnitGetProperty`.
    pub(crate) unsafe fn get_property(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        out_data: *mut c_void,
        io_size: *mut u32,
    ) -> OSStatus {
        let prop = match self.validate_property(id, scope, element) {
            Ok(prop) => prop,
            Err(status) => return status,
        };
        if matches!(
            prop,
            Prop::ClassInfoFromDocument | Prop::SetRenderCallback | Prop::MakeConnection
        ) {
            return kAudioUnitErr_InvalidProperty;
        }
        let (size, _) = match self.property_size(prop, element) {
            Ok(info) => info,
            Err(status) => return status,
        };

        if io_size.is_null() {
            return PARAM_ERR;
        }
        // SAFETY: Checked for null above.
        let capacity = unsafe { io_size.read_unaligned() };
        if capacity < size {
            return kAudioUnitErr_InvalidPropertyValue;
        }
        if out_data.is_null() && size > 0 {
            return PARAM_ERR;
        }

        // Writes a value of type `T` to the host's buffer. The size was checked above.
        macro_rules! put {
            ($value:expr) => {{
                let value = $value;
                debug_assert_eq!(size_of_val(&value) as u32, size);
                // SAFETY: `out_data` has room for `size` bytes, and the value is that size.
                unsafe { put_value(out_data, value) };
            }};
        }

        match prop {
            Prop::ClassInfo => match state::get_class_info(self) {
                Ok(dictionary) => {
                    put!(CFRetained::into_raw(dictionary).as_ptr() as *const c_void)
                }
                Err(status) => return status,
            },
            Prop::StreamFormat => put!(self.stream_format_description(scope, element)),
            Prop::SampleRate => put!(self.stream_format_description(scope, element).mSampleRate),
            Prop::ElementCount => put!(self.element_count(scope)),
            Prop::SupportedNumChannels => {
                let pairs = supported_channels(P::AUDIO_IO_LAYOUTS);
                for (i, ChannelPair { input, output }) in pairs.into_iter().enumerate() {
                    // SAFETY: `size` covers one `AUChannelInfo` per pair.
                    unsafe {
                        out_data
                            .cast::<AUChannelInfo>()
                            .add(i)
                            .write_unaligned(AUChannelInfo {
                                inChannels: input,
                                outChannels: output,
                            })
                    };
                }
            }
            Prop::MaximumFramesPerSlice => put!(self.max_frames.load(Ordering::Relaxed)),
            Prop::Latency => put!(seconds_from_samples(
                self.current_latency.load(Ordering::Relaxed),
                self.sample_rate()
            )),
            Prop::TailTime => put!(tail_seconds(
                &self.last_process_status.load(),
                self.sample_rate()
            )),
            Prop::ParameterList => {
                for (i, listed) in self.table.listed_ids().into_iter().enumerate() {
                    // SAFETY: `size` covers one `u32` per listed parameter.
                    unsafe { out_data.cast::<u32>().add(i).write_unaligned(listed) };
                }
            }
            Prop::ParameterInfo => {
                let Some(idx) = self.table.index_of(element) else {
                    return kAudioUnitErr_InvalidParameter;
                };
                let entry = &self.table.entries[idx];

                let mut name = [0 as c_char; 52];
                // Truncated to 51 bytes plus the terminator, on a character boundary
                let mut end = entry.name.len().min(51);
                while end > 0 && !entry.name.is_char_boundary(end) {
                    end -= 1;
                }
                for (dest, byte) in name.iter_mut().zip(&entry.name.as_bytes()[..end]) {
                    *dest = *byte as c_char;
                }

                put!(AudioUnitParameterInfo {
                    name,
                    unitName: ptr::null(),
                    clumpID: entry.clump.unwrap_or(0),
                    cfNameString: self.cf_names[idx] as *const CFString,
                    unit: match entry.kind {
                        AuParamKind::Continuous | AuParamKind::Stepped { .. } => {
                            AudioUnitParameterUnit::Generic
                        }
                        AuParamKind::Indexed { .. } => AudioUnitParameterUnit::Indexed,
                        AuParamKind::Boolean => AudioUnitParameterUnit::Boolean,
                    },
                    minValue: entry.min,
                    maxValue: entry.max,
                    defaultValue: entry.default,
                    flags: AudioUnitParameterOptions::from_bits_retain(entry.flags),
                })
            }
            Prop::ParameterValueStrings => {
                let Some(entry) = self.table.get(element) else {
                    return kAudioUnitErr_InvalidParameter;
                };
                let AuParamKind::Indexed { steps } = entry.kind else {
                    return kAudioUnitErr_InvalidProperty;
                };

                let strings: Vec<CFRetained<CFString>> = (0..=steps)
                    .map(|step| {
                        let normalized = if steps == 0 {
                            0.0
                        } else {
                            step as f32 / steps as f32
                        };
                        let text =
                            unsafe { entry.ptr.normalized_value_to_string(normalized, false) };
                        CFString::from_str(&text)
                    })
                    .collect();
                let array = CFArray::<CFString>::from_retained_objects(&strings);
                put!(CFRetained::into_raw(array).as_ptr() as *const c_void)
            }
            Prop::ParameterStringFromValue => {
                // SAFETY: The struct is in/out and `capacity >= size` was checked above.
                let mut request =
                    unsafe { out_data.cast::<ParameterStringFromValue>().read_unaligned() };
                let Some(entry) = self.table.get(request.in_param_id) else {
                    return kAudioUnitErr_InvalidParameter;
                };

                let value = if request.in_value.is_null() {
                    self.parameter_au_value(entry.id).unwrap_or(0.0)
                } else {
                    // SAFETY: The host passes a pointer to a value, or null.
                    unsafe { request.in_value.read_unaligned() }
                };
                let text = unsafe {
                    entry
                        .ptr
                        .normalized_value_to_string(normalized_from_au(entry.kind, value), true)
                };
                request.out_string = CFRetained::into_raw(CFString::from_str(&text)).as_ptr();
                put!(request)
            }
            Prop::ParameterValueFromString => {
                // SAFETY: The struct is in/out and `capacity >= size` was checked above.
                let mut request =
                    unsafe { out_data.cast::<ParameterValueFromString>().read_unaligned() };
                let Some(entry) = self.table.get(request.in_param_id) else {
                    return kAudioUnitErr_InvalidParameter;
                };
                if request.in_string.is_null() {
                    return kAudioUnitErr_InvalidPropertyValue;
                }

                // SAFETY: Checked for null above; the host owns the string for this call.
                let text = unsafe { &*request.in_string }.to_string();
                match unsafe { entry.ptr.string_to_normalized_value(&text) } {
                    Some(normalized) => {
                        request.out_value = au_from_normalized(entry.kind, normalized);
                    }
                    None => return kAudioUnitErr_InvalidPropertyValue,
                }
                put!(request)
            }
            Prop::ParameterClumpName => {
                // SAFETY: The struct is in/out and `capacity >= size` was checked above.
                let mut request = unsafe {
                    out_data
                        .cast::<AudioUnitParameterNameInfo>()
                        .read_unaligned()
                };
                let name = (request.inID as usize)
                    .checked_sub(1)
                    .and_then(|idx| self.table.clump_names.get(idx));
                let Some(name) = name else {
                    return kAudioUnitErr_InvalidPropertyValue;
                };
                request.outName = CFRetained::into_raw(CFString::from_str(name)).as_ptr();
                put!(request)
            }
            Prop::HostCallbacks => {
                let stored = self.host_callbacks.load();
                // SAFETY: An all-zero `HostCallbackInfo` is null pointers and `None`s.
                let mut info: HostCallbackInfo = unsafe { zeroed() };
                info.hostUserData = stored.user_data as *mut c_void;
                info.beatAndTempoProc = stored.beat_and_tempo;
                info.musicalTimeLocationProc = stored.musical_time_location;
                info.transportStateProc2 = stored.transport_state2;
                put!(info)
            }
            // DELTON FORK DELTA: au-3, a null output `mData` gets our buffer, not the input's
            Prop::InPlaceProcessing => put!(0u32),
            Prop::ShouldAllocateBuffer => {
                put!(
                    self.should_allocate[(scope == kAudioUnitScope_Output) as usize]
                        .load(Ordering::Relaxed)
                )
            }
            Prop::BypassEffect => {
                let bypassed = match self.bypass_id.and_then(|id| self.parameter_au_value(id)) {
                    Some(value) => value >= 0.5,
                    None => self.bypassed.load(Ordering::Relaxed),
                };
                put!(bypassed as u32)
            }
            Prop::LastRenderError => put!(self.last_render_error.swap(NO_ERR, Ordering::Relaxed)),
            Prop::FactoryPresets => {
                put!(CFRetained::into_raw(CFArray::<CFType>::empty()).as_ptr() as *const c_void)
            }
            Prop::PresentPreset => {
                let name = CFString::from_str(&self.present_preset_name.lock());
                put!(AUPreset {
                    presetNumber: -1,
                    presetName: CFRetained::into_raw(name).as_ptr(),
                })
            }
            Prop::ElementName => match self.element_name(scope, element) {
                Some(name) => {
                    put!(CFRetained::into_raw(CFString::from_str(&name)).as_ptr() as *const c_void)
                }
                None => return kAudioUnitErr_InvalidProperty,
            },
            Prop::RenderQuality => put!(RENDER_QUALITY),
            Prop::OfflineRender => {
                put!((self.current_process_mode.load() == ProcessMode::Offline) as u32)
            }
            Prop::Private => put!(self as *const Self as *const c_void),
            // DELTON FORK DELTA: au-2
            #[cfg(feature = "editor")]
            Prop::CocoaUI => match view::cocoa_view_info::<P>() {
                Some(info) => put!(info),
                None => return kAudioUnitErr_CannotDoInCurrentContext,
            },
            // Handled before the size check
            Prop::ClassInfoFromDocument | Prop::SetRenderCallback | Prop::MakeConnection => {
                return kAudioUnitErr_InvalidProperty;
            }
        }

        // SAFETY: Checked for null above.
        unsafe { io_size.write_unaligned(size) };

        NO_ERR
    }

    /// Store one format and replace the sample rate of every other element.
    fn store_stream_format(&self, scope: AudioUnitScope, element: u32, format: StreamFormat) {
        let mut formats = self.formats.lock();
        let list = if scope == kAudioUnitScope_Input {
            &mut formats.inputs
        } else {
            &mut formats.outputs
        };
        if let Some(slot) = list.get_mut(element as usize) {
            *slot = format;
        }
        self.set_every_sample_rate(&mut formats, format.sample_rate);
    }

    /// AU units run at one sample rate: a new rate on one element is the rate of all of them.
    fn set_every_sample_rate(&self, formats: &mut Formats, sample_rate: f64) {
        for format in formats.inputs.iter_mut().chain(formats.outputs.iter_mut()) {
            format.sample_rate = sample_rate;
        }
        self.sample_rate_bits
            .store(sample_rate.to_bits(), Ordering::Relaxed);
    }

    /// `AudioUnitSetProperty`.
    pub(crate) unsafe fn set_property(
        &self,
        id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        data: *const c_void,
        in_size: u32,
    ) -> OSStatus {
        let prop = match self.validate_property(id, scope, element) {
            Ok(prop) => prop,
            Err(status) => return status,
        };
        if !prop.writable() {
            return kAudioUnitErr_PropertyNotWritable;
        }
        let (size, _) = match self.property_size(prop, element) {
            Ok(info) => info,
            Err(status) => return status,
        };

        // HostCallbacks takes whatever size the host's struct version has
        if prop != Prop::HostCallbacks && in_size < size {
            return kAudioUnitErr_InvalidPropertyValue;
        }
        if data.is_null() {
            return kAudioUnitErr_InvalidPropertyValue;
        }

        // Reads a value of type `T` from the host's buffer. The size was checked above.
        macro_rules! take {
            ($ty:ty) => {
                // SAFETY: `data` is non-null and `in_size >= size_of::<$ty>()`.
                unsafe { data.cast::<$ty>().read_unaligned() }
            };
        }

        let initialized = self.initialized.load(Ordering::Acquire);
        match prop {
            Prop::ClassInfo | Prop::ClassInfoFromDocument => {
                let plist = take!(*const c_void);
                // SAFETY: The host passes a live property list.
                return unsafe { state::set_class_info(self, plist) };
            }
            Prop::StreamFormat => {
                if initialized {
                    return kAudioUnitErr_Initialized;
                }
                let description = take!(AudioStreamBasicDescription);

                let wanted_flags = kAudioFormatFlagIsFloat
                    | kAudioFormatFlagIsPacked
                    | kAudioFormatFlagIsNonInterleaved;
                let unwanted_flags = kAudioFormatFlagIsBigEndian | kAudioFormatFlagIsSignedInteger;
                let format_ok = description.mFormatID == kAudioFormatLinearPCM
                    && description.mFormatFlags & wanted_flags == wanted_flags
                    && description.mFormatFlags & unwanted_flags == 0
                    && description.mBitsPerChannel == 32
                    && description.mBytesPerFrame == 4
                    && description.mFramesPerPacket == 1
                    && description.mSampleRate.is_finite()
                    && description.mSampleRate > 0.0
                    && channels_allowed(
                        P::AUDIO_IO_LAYOUTS,
                        scope == kAudioUnitScope_Input,
                        element,
                        description.mChannelsPerFrame,
                    );
                if !format_ok {
                    return kAudioUnitErr_FormatNotSupported;
                }

                self.store_stream_format(
                    scope,
                    element,
                    StreamFormat {
                        sample_rate: description.mSampleRate,
                        channels: description.mChannelsPerFrame,
                    },
                );
            }
            Prop::SampleRate => {
                if initialized {
                    return kAudioUnitErr_Initialized;
                }
                let sample_rate = take!(f64);
                if !(sample_rate.is_finite() && sample_rate > 0.0) {
                    return kAudioUnitErr_FormatNotSupported;
                }

                let mut formats = self.formats.lock();
                self.set_every_sample_rate(&mut formats, sample_rate);
            }
            Prop::MaximumFramesPerSlice => {
                if initialized {
                    return kAudioUnitErr_Initialized;
                }
                let frames = take!(u32);
                if frames == 0 {
                    return kAudioUnitErr_InvalidPropertyValue;
                }
                // DELTON FORK DELTA: au-3, auval requires the change notification
                if self.max_frames.swap(frames, Ordering::Relaxed) != frames {
                    self.notify_property(
                        kAudioUnitProperty_MaximumFramesPerSlice,
                        kAudioUnitScope_Global,
                        0,
                    );
                }
            }
            Prop::SetRenderCallback => {
                let callback = take!(RenderCallbackStruct);
                let source = match callback.input_proc {
                    Some(proc) => InputSource::Callback {
                        proc,
                        refcon: callback.input_proc_ref_con as usize,
                    },
                    None => InputSource::None,
                };
                self.input_sources[element as usize].store(source);
            }
            Prop::MakeConnection => {
                let connection = take!(AudioUnitConnection);
                let source = if connection.sourceAudioUnit.is_null() {
                    InputSource::None
                } else {
                    InputSource::Connection {
                        unit: connection.sourceAudioUnit as usize,
                        output: connection.sourceOutputNumber,
                    }
                };
                self.input_sources[element as usize].store(source);
            }
            Prop::HostCallbacks => {
                // The host's struct may be from an older version of the header and shorter than ours
                // SAFETY: An all-zero `HostCallbackInfo` is null pointers and `None`s.
                let mut info: HostCallbackInfo = unsafe { zeroed() };
                let copy_len = (in_size as usize).min(size_of::<HostCallbackInfo>());
                // SAFETY: `data` has `in_size` bytes and `info` has room for `copy_len`.
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.cast::<u8>(),
                        (&raw mut info).cast::<u8>(),
                        copy_len,
                    )
                };
                self.host_callbacks.store(HostCallbacks {
                    user_data: info.hostUserData as usize,
                    beat_and_tempo: info.beatAndTempoProc,
                    musical_time_location: info.musicalTimeLocationProc,
                    transport_state2: info.transportStateProc2,
                });
            }
            Prop::InPlaceProcessing | Prop::RenderQuality => {
                // Accepted and ignored
                let _ = take!(u32);
            }
            Prop::ShouldAllocateBuffer => {
                let value = take!(u32);
                self.should_allocate[(scope == kAudioUnitScope_Output) as usize]
                    .store(value, Ordering::Relaxed);
            }
            Prop::BypassEffect => {
                let bypassed = take!(u32) != 0;
                self.bypassed.store(bypassed, Ordering::Relaxed);
                if let Some(entry) = self.bypass_id.and_then(|id| self.table.get(id)) {
                    self.apply_host_parameter(
                        entry.id,
                        entry.ptr,
                        if bypassed { 1.0 } else { 0.0 },
                        0,
                    );
                }
            }
            Prop::PresentPreset => {
                let preset = take!(AUPreset);
                let name = if preset.presetName.is_null() {
                    String::from("Untitled")
                } else {
                    // SAFETY: Checked for null above; the host owns the string for this call.
                    unsafe { &*preset.presetName }.to_string()
                };
                *self.present_preset_name.lock() = name;
            }
            Prop::OfflineRender => {
                let mode = if take!(u32) != 0 {
                    ProcessMode::Offline
                } else {
                    ProcessMode::Realtime
                };
                let old_mode = self.current_process_mode.swap(mode);
                if initialized && P::RESTART_ON_PROCESS_MODE_CHANGE && old_mode != mode {
                    let status = self.uninitialize();
                    if status != NO_ERR {
                        return status;
                    }
                    return self.initialize();
                }
            }
            // Not writable: refused above
            Prop::ElementCount
            | Prop::SupportedNumChannels
            | Prop::Latency
            | Prop::TailTime
            | Prop::ParameterList
            | Prop::ParameterInfo
            | Prop::ParameterValueStrings
            | Prop::ParameterStringFromValue
            | Prop::ParameterValueFromString
            | Prop::ParameterClumpName
            | Prop::LastRenderError
            | Prop::FactoryPresets
            | Prop::ElementName
            | Prop::Private => return kAudioUnitErr_PropertyNotWritable,
            // DELTON FORK DELTA: au-2
            #[cfg(feature = "editor")]
            Prop::CocoaUI => return kAudioUnitErr_PropertyNotWritable,
        }

        NO_ERR
    }

    // ----------------------------------------------------------------------------------------
    // Render
    // ----------------------------------------------------------------------------------------

    /// `AudioUnitRender`.
    ///
    /// # Safety
    ///
    /// The pointers are the host's; they are checked for null here.
    pub(crate) unsafe fn render(
        &self,
        io_action_flags: *mut AudioUnitRenderActionFlags,
        timestamp: *const AudioTimeStamp,
        output_bus: u32,
        frames: u32,
        io_data: *mut AudioBufferList,
    ) -> OSStatus {
        if timestamp.is_null() || io_data.is_null() {
            return PARAM_ERR;
        }

        // SAFETY: Checked for null above.
        let buffer_count = unsafe { (*io_data).mNumberBuffers };
        match check_render(
            self.initialized.load(Ordering::Acquire),
            output_bus,
            frames,
            self.max_frames.load(Ordering::Relaxed),
            buffer_count,
            self.render_channels.load(Ordering::Relaxed),
        ) {
            Ok(RenderCheck::Proceed) => (),
            Ok(RenderCheck::NothingToDo) => return NO_ERR,
            Err(error) => {
                let status = match error {
                    RenderError::Uninitialized => kAudioUnitErr_Uninitialized,
                    RenderError::InvalidElement => kAudioUnitErr_InvalidElement,
                    RenderError::TooManyFrames => kAudioUnitErr_TooManyFramesToProcess,
                    RenderError::FormatNotSupported => kAudioUnitErr_FormatNotSupported,
                };
                self.last_render_error.store(status, Ordering::Relaxed);
                return status;
            }
        }

        let base_flags = if io_action_flags.is_null() {
            AudioUnitRenderActionFlags(0)
        } else {
            // SAFETY: Checked for null above.
            unsafe { *io_action_flags }
        };

        let mut flags = AudioUnitRenderActionFlags(
            base_flags.0 | AudioUnitRenderActionFlags::UnitRenderAction_PreRender.0,
        );
        // SAFETY: The notifies are the host's; they get the render's own arguments.
        unsafe {
            self.render_notifies
                .call(&mut flags, timestamp, output_bus, frames, io_data)
        };

        // SAFETY: The pointers were checked for null above.
        let status = unsafe { self.render_block(timestamp, frames, io_data) };

        let mut flags = AudioUnitRenderActionFlags(
            base_flags.0
                | AudioUnitRenderActionFlags::UnitRenderAction_PostRender.0
                | if status == NO_ERR {
                    0
                } else {
                    AudioUnitRenderActionFlags::UnitRenderAction_PostRenderError.0
                },
        );
        // SAFETY: See above.
        unsafe {
            self.render_notifies
                .call(&mut flags, timestamp, output_bus, frames, io_data)
        };

        status
    }

    /// Pull the inputs, process one block, fill the output. Steps 3 to 10 of the render.
    ///
    /// # Safety
    ///
    /// `timestamp` and `io_data` must be valid, and `io_data` must hold one buffer per main output
    /// channel.
    unsafe fn render_block(
        &self,
        timestamp: *const AudioTimeStamp,
        frames: u32,
        io_data: *mut AudioBufferList,
    ) -> OSStatus {
        let Ok(mut buffers) = self.buffers.try_borrow_mut() else {
            return kAudioUnitErr_CannotDoInCurrentContext;
        };
        let Ok(mut buffer_manager) = self.buffer_manager.try_borrow_mut() else {
            return kAudioUnitErr_CannotDoInCurrentContext;
        };
        let (Some(buffer_config), Some(_layout)) = (
            self.current_buffer_config.load(),
            self.current_layout.load(),
        ) else {
            return kAudioUnitErr_Uninitialized;
        };
        let Some(timestamp_ptr) = NonNull::new(timestamp as *mut AudioTimeStamp) else {
            return PARAM_ERR;
        };

        // Pull every input element
        for element in 0..buffers.input_elements() {
            let list = buffers.prepare_input(element, frames);
            let source = self
                .input_sources
                .get(element)
                .map(|source| source.load())
                .unwrap_or(InputSource::None);

            let status = match source {
                InputSource::None => kAudioUnitErr_NoConnection,
                InputSource::Callback { proc, refcon } => {
                    let mut pull_flags = AudioUnitRenderActionFlags(0);
                    // SAFETY: The callback is the host's and gets a buffer list we prepared.
                    unsafe {
                        proc(
                            refcon as *mut c_void,
                            &mut pull_flags,
                            timestamp,
                            element as u32,
                            frames,
                            list,
                        )
                    }
                }
                InputSource::Connection { unit, output } => {
                    let mut pull_flags = AudioUnitRenderActionFlags(0);
                    match NonNull::new(list) {
                        // SAFETY: The unit is the host's and gets a buffer list we prepared.
                        Some(list) => unsafe {
                            AudioUnitRender(
                                unit as AudioUnit,
                                &mut pull_flags,
                                timestamp_ptr,
                                output,
                                frames,
                                list,
                            )
                        },
                        None => PARAM_ERR,
                    }
                }
            };

            let is_main = self.shape.has_main_input && element == 0;
            if status != NO_ERR && is_main {
                self.last_render_error.store(status, Ordering::Relaxed);
                return status;
            }
            // An aux element without a source, or whose call failed, reads as silence
            buffers.finish_input(element, frames, status == NO_ERR);
        }

        // The output: the host's buffers, or ours when it passed none
        // SAFETY: The caller guarantees `io_data` holds one buffer per main output channel.
        if !unsafe { buffers.prepare_outputs(io_data, frames) } {
            return PARAM_ERR;
        }

        // Host parameter changes since the last render
        if P::RAW_PARAM_AUTOMATION {
            if let Ok(mut raw_automation) = self.raw_automation.try_borrow_mut() {
                raw_automation.clear();
                let last_frame = frames.saturating_sub(1);
                while let Some((offset, hash, normalized_value)) = self.param_events.pop() {
                    if let Some(&param) = self.param_by_hash.get(&hash) {
                        raw_automation.push(ParamAutomationPoint {
                            timing: offset.min(last_frame),
                            param,
                            normalized_value,
                        });
                    }
                }
                if self.automation_overflowed.swap(false, Ordering::Relaxed) {
                    raw_automation.mark_overflowed();
                }
                raw_automation.sort_by_timing();
            }
        } else {
            // Values were applied when they were set, nothing else reads these
            while self.param_events.pop().is_some() {}
            self.automation_overflowed.store(false, Ordering::Relaxed);
        }

        // Without a bypass parameter, a bypassed unit is a copy
        if self.bypassed.load(Ordering::Relaxed) && self.bypass_id.is_none() {
            buffers.copy_main_input_to_output(frames);
            return NO_ERR;
        }

        let has_main_input = self.shape.has_main_input;
        let has_main_output = self.shape.has_main_output;
        let aux_input_start = usize::from(has_main_input);
        let aux_output_start = usize::from(has_main_output);
        // SAFETY: The channel pointer arrays in `buffers` point at buffers that hold `frames`
        // samples, and they are not touched again until the plugin has returned.
        let created = unsafe {
            buffer_manager.create_buffers(0, frames as usize, |source| {
                if has_main_output {
                    *source.main_output_channel_pointers = buffers.output_channel_pointers(0);
                }
                if has_main_input {
                    *source.main_input_channel_pointers = buffers.input_channel_pointers(0);
                }
                for (i, slot) in source.aux_input_channel_pointers.iter_mut().enumerate() {
                    *slot = buffers.input_channel_pointers(aux_input_start + i);
                }
                for (i, slot) in source.aux_output_channel_pointers.iter_mut().enumerate() {
                    *slot = buffers.output_channel_pointers(aux_output_start + i);
                }
            })
        };

        let transport = self.build_transport(
            self.host_callbacks.load(),
            unsafe { &*timestamp },
            buffer_config.sample_rate,
        );

        let Some(mut plugin) = self.plugin.try_lock() else {
            // A reset or a state load holds the plugin on another thread: this block is silence
            crate::nice_error!("The plugin is busy on another thread, rendering silence");
            buffers.zero_main_output(frames);
            return NO_ERR;
        };

        let status = {
            let mut aux = AuxiliaryBuffers {
                inputs: created.aux_inputs,
                outputs: created.aux_outputs,
            };
            let mut context = WrapperProcessContext {
                inner: self,
                raw_automation_guard: self.raw_automation.borrow(),
                transport,
            };

            process_wrapper(|| plugin.process(created.main_buffer, &mut aux, &mut context))
        };
        drop(plugin);

        self.last_process_status.store(status);
        if let ProcessStatus::Error(message) = status {
            // Log each message once, without allocating on every block
            if self
                .last_logged_error
                .swap(message.as_ptr() as usize, Ordering::Relaxed)
                != message.as_ptr() as usize
            {
                crate::nice_error!("Process error: {}", message);
            }
            self.last_render_error
                .store(kAudioUnitErr_CannotDoInCurrentContext, Ordering::Relaxed);
            return kAudioUnitErr_CannotDoInCurrentContext;
        }

        // After processing audio, we'll check if the editor has sent us updated plugin state.
        // We'll restore that here on the audio thread to prevent changing the values during the
        // process call. The state goes back to the GUI thread to be deallocated there.
        // FIXME: Zero capacity channels allocate on receiving, find a better alternative that
        //        doesn't do that
        let updated_state = permit_alloc(|| self.updated_state_receiver.try_recv());
        if let Ok(mut state) = updated_state {
            self.set_state_inner(&mut state);
            if let Err(err) = self.updated_state_sender.send(state) {
                crate::nice_debug_assert_failure!(
                    "Failed to send state object back to GUI thread: {}",
                    err
                );
            }
        }

        NO_ERR
    }

    /// The transport the plugin sees, from the host's callbacks. A callback that is absent or that
    /// fails leaves its fields at their `None` defaults.
    fn build_transport(
        &self,
        callbacks: HostCallbacks,
        timestamp: &AudioTimeStamp,
        sample_rate: f32,
    ) -> Transport {
        let mut transport = Transport::new(sample_rate);
        let user_data = callbacks.user_data as *mut c_void;

        if let Some(proc) = callbacks.beat_and_tempo {
            let (mut beat, mut tempo) = (0.0f64, 0.0f64);
            // SAFETY: The callback is the host's and gets valid output pointers.
            if unsafe { proc(user_data, &mut beat, &mut tempo) } == NO_ERR {
                transport.tempo = Some(tempo);
                transport.pos_beats = Some(beat);
            }
        }

        if let Some(proc) = callbacks.musical_time_location {
            let (mut delta_to_next_beat, mut numerator, mut denominator, mut bar_start) =
                (0u32, 0.0f32, 0u32, 0.0f64);
            // SAFETY: The callback is the host's and gets valid output pointers.
            if unsafe {
                proc(
                    user_data,
                    &mut delta_to_next_beat,
                    &mut numerator,
                    &mut denominator,
                    &mut bar_start,
                )
            } == NO_ERR
            {
                transport.time_sig_numerator = Some(numerator as i32);
                transport.time_sig_denominator = Some(denominator as i32);
                transport.bar_start_pos_beats = Some(bar_start);
            }
        }

        if let Some(proc) = callbacks.transport_state2 {
            let (mut playing, mut recording, mut changed) = (0u8, 0u8, 0u8);
            let (mut sample_position, mut cycling) = (0.0f64, 0u8);
            let (mut cycle_start, mut cycle_end) = (0.0f64, 0.0f64);
            // SAFETY: The callback is the host's and gets valid output pointers.
            if unsafe {
                proc(
                    user_data,
                    &mut playing,
                    &mut recording,
                    &mut changed,
                    &mut sample_position,
                    &mut cycling,
                    &mut cycle_start,
                    &mut cycle_end,
                )
            } == NO_ERR
            {
                transport.playing = playing != 0;
                transport.recording = recording != 0;
                transport.pos_samples = Some(sample_position as i64);
                if cycling != 0 {
                    transport.loop_range_beats = Some((cycle_start, cycle_end));
                }
            }
        }

        if transport.pos_samples.is_none()
            && timestamp.mFlags.0 & AudioTimeStampFlags::SampleTimeValid.0 != 0
        {
            transport.pos_samples = Some(timestamp.mSampleTime as i64);
        }

        transport
    }
}

// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
impl<P: AuPlugin> Wrapper<P> {
    pub(super) fn make_gui_context(self: Arc<Self>) -> GuiContext {
        GuiContext::new(Arc::new(WrapperGuiContext {
            inner: Arc::downgrade(&self),
            #[cfg(debug_assertions)]
            param_gesture_checker: Default::default(),
        }))
    }

    /// Whether the calling thread is the main thread.
    pub(super) fn is_main_thread(&self) -> bool {
        self.event_loop
            .borrow()
            .as_ref()
            .is_some_and(|event_loop| event_loop.is_main_thread())
    }

    /// Get the plugin's state object, may be called by the plugin's GUI as part of its own preset
    /// management.
    pub(super) fn get_state_object(&self) -> PluginState {
        // SAFETY: The pointers in the maps come from `self.params`, which this wrapper keeps alive.
        unsafe {
            crate::wrapper::state::serialize_object::<P>(
                self.params.clone(),
                crate::wrapper::state::make_params_iter(
                    &self.param_by_hash,
                    &self.param_id_to_hash,
                ),
            )
        }
    }

    /// Update the plugin's state, called by the plugin itself from the GUI thread. While the unit
    /// is rendering, the state goes to the render thread, which applies it at the end of the
    /// block. A unit can be initialized without anyone rendering it (a host may stop rendering
    /// idle tracks), so this waits for the render thread for 100 ms at most and then applies the
    /// state on this thread.
    pub(super) fn set_state_object_from_gui(&self, mut state: PluginState) {
        use crossbeam::channel::SendTimeoutError;

        if self.initialized.load(Ordering::Acquire) {
            match self
                .updated_state_sender
                .send_timeout(state, Duration::from_millis(100))
            {
                Ok(()) => {
                    // The state object comes back to this thread so it is deallocated here
                    let returned = self.updated_state_receiver.recv();
                    drop(returned);
                }
                Err(SendTimeoutError::Timeout(mut state)) => {
                    self.set_state_inner(&mut state);
                }
                Err(SendTimeoutError::Disconnected(_)) => {
                    crate::nice_debug_assert_failure!("State update channel got disconnected");
                }
            }
        } else {
            self.set_state_inner(&mut state);
        }
    }

    /// A GUI edit: set the parameter now, the same way a host `SetParameter` would, and tell the
    /// host's listeners.
    pub(super) fn apply_editor_parameter(&self, hash: u32, normalized: f32) {
        let Some(&ptr) = self.param_by_hash.get(&hash) else {
            crate::nice_debug_assert_failure!("Unknown parameter hash: {}", hash);
            return;
        };

        self.apply_host_parameter(hash, ptr, normalized, 0);
        self.notify_parameter_event(AudioUnitEventType::ParameterValueChange, hash);
    }

    /// Tell the host's listeners about a gesture or a value change of one parameter.
    pub(super) fn notify_parameter_event(&self, event_type: AudioUnitEventType, hash: u32) {
        let event = AudioUnitEvent {
            event_type,
            argument: AudioUnitEventArgument {
                parameter: AudioUnitParameter {
                    mAudioUnit: self.unit.0,
                    mParameterID: hash,
                    mScope: kAudioUnitScope_Global,
                    mElement: 0,
                },
            },
        };

        // SAFETY: A null listener and object tell every listener, including the host's. The event
        // outlives the call.
        let status = unsafe { AUEventListenerNotify(ptr::null_mut(), ptr::null_mut(), &event) };
        crate::nice_debug_assert!(
            status == NO_ERR,
            "AUEventListenerNotify failed with status {}",
            status
        );
    }

    /// Run `f` on the open editor, if there is one. Does nothing if there is none, if the slot is
    /// busy (an editor call is further up the stack), or if the slot belongs to another thread.
    fn with_open_editor(&self, f: impl FnOnce(&EditorSlot<P>)) {
        let Ok(slot) = self.editor_slot.try_borrow() else {
            return;
        };
        let Some(Ok(slot)) = slot.as_ref().map(|slot| slot.try_get()) else {
            return;
        };

        f(slot);
    }

    /// Close the open editor, whichever container it is in. The factory's `close` calls this.
    pub(crate) fn close_editor(&self) {
        self.take_editor(None);
    }

    /// Close the open editor only if it lives in `container`. The container's dealloc calls this.
    pub(super) fn close_editor_for(&self, container: usize) {
        self.take_editor(Some(container));
    }

    /// Take the open editor out of its slot and drop it, if it lives in `only_container` (or if
    /// that is `None`).
    fn take_editor(&self, only_container: Option<usize>) {
        if !self.is_main_thread() {
            match only_container {
                // A container released off the main thread cannot be matched to the slot
                Some(_) => crate::nice_error!(
                    "AU: the editor's view was released off the main thread; the editor stays open"
                ),
                None => {
                    let slot = match self.editor_slot.try_borrow_mut() {
                        Ok(mut slot) => slot.take(),
                        Err(_) => {
                            crate::nice_error!("AU: the editor is busy, cannot close it");
                            return;
                        }
                    };
                    if let Some(slot) = slot {
                        crate::nice_error!(
                            "AU: the editor must be closed on the main thread; leaking it"
                        );
                        // Dropping a `Fragile` on another thread panics
                        std::mem::forget(slot);
                    }
                }
            }
            return;
        }

        // The guard is gone before the slot is dropped, so a re-entrant call from the editor's
        // drop finds the slot empty instead of panicking
        let slot = match self.editor_slot.try_borrow_mut() {
            Ok(mut slot) => {
                let matches = match (slot.as_ref(), only_container) {
                    (Some(_), None) => true,
                    (Some(slot), Some(container)) => {
                        slot.try_get().is_ok_and(|slot| slot.container == container)
                    }
                    (None, _) => false,
                };
                if matches {
                    slot.take()
                } else {
                    None
                }
            }
            Err(_) => {
                crate::nice_error!("AU: the editor is busy, cannot close it");
                return;
            }
        };
        drop(slot);
    }

    /// The container moved into (`true`) or out of (`false`) a window: show or hide the editor.
    pub(super) fn container_moved_to_window(&self, container: usize, in_window: bool) {
        self.with_open_editor(|slot| {
            if slot.container != container {
                return;
            }

            if !in_window && !slot.hidden.get() {
                if let Err(err) = slot.spawned.handle.hide(&slot.spawned.window) {
                    crate::nice_warn!("AU: failed to hide the editor: {}", err);
                }
                slot.hidden.set(true);
            } else if in_window && slot.hidden.get() {
                if let Err(err) = slot.spawned.handle.show(&slot.spawned.window) {
                    crate::nice_warn!("AU: failed to show the editor: {}", err);
                }
                slot.hidden.set(false);
            }
        });
    }

    /// Someone other than the editor resized `container`: forward to the editor if it is
    /// resizable.
    pub(super) fn container_resized_by_host(&self, container: usize, size: NativeSize<u32>) {
        // `try_lock`: the editor is locked while it spawns, and it may resize its parent from there
        let can_resize = match self.editor.borrow().as_ref() {
            Some(editor) => editor
                .try_lock()
                .is_some_and(|editor| editor.resize_hint().can_resize),
            None => false,
        };
        if !can_resize {
            return;
        }

        self.with_open_editor(|slot| {
            if slot.container != container {
                return;
            }

            if let Err(err) = slot.spawned.handle.set_size(size, &slot.spawned.window) {
                crate::nice_warn!("AU: the editor refused the host's resize: {}", err);
            }
        });
    }

    /// The editor asked for `size`: resize the container without forwarding it back.
    fn resize_container_from_editor(&self, size: NativeSize<u32>) {
        self.with_open_editor(|slot| view::set_container_size_from_editor(slot.container, size));
    }
}

impl<P: AuPlugin> MainThreadExecutor<Task<P>> for Wrapper<P> {
    fn execute(&self, task: Task<P>, _is_gui_thread: bool) {
        // This function is always called from the main thread
        match task {
            Task::PluginTask(task) => (self.task_executor.lock())(task),
            // DELTON FORK DELTA: au-2
            Task::ParameterValueChanged(hash, normalized) => {
                #[cfg(feature = "editor")]
                if let Some(entry) = self.table.get(hash) {
                    self.with_open_editor(|slot| {
                        slot.spawned
                            .handle
                            .param_value_changed(&entry.string_id, normalized)
                    });
                }
                #[cfg(not(feature = "editor"))]
                let _ = (hash, normalized);
            }
            Task::StateChanged => {
                for entry in &self.table.entries {
                    let mut parameter = AudioUnitParameter {
                        mAudioUnit: self.unit.0,
                        mParameterID: entry.id,
                        mScope: kAudioUnitScope_Global,
                        mElement: 0,
                    };
                    // SAFETY: A null listener and object tell every listener, including the host's.
                    unsafe {
                        AUParameterListenerNotify(
                            ptr::null_mut(),
                            ptr::null_mut(),
                            NonNull::from(&mut parameter),
                        )
                    };
                }
                // DELTON FORK DELTA: au-2
                #[cfg(feature = "editor")]
                self.with_open_editor(|slot| slot.spawned.handle.state_changed());
            }
            Task::LatencyChanged => {
                self.notify_property(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0)
            }
            // DELTON FORK DELTA: au-2
            #[cfg(feature = "editor")]
            Task::RequestResize { size, scale_factor } => {
                self.resize_container_from_editor(NativeSize::<u32>::from_size(size, scale_factor))
            }
            #[cfg(feature = "editor")]
            Task::CallMainThread => self.with_open_editor(|slot| {
                slot.spawned
                    .handle
                    .host_main_thread_callback(&slot.spawned.window)
            }),
        }
    }
}

impl<P: AuPlugin> Drop for Wrapper<P> {
    fn drop(&mut self) {
        // DELTON FORK DELTA: au-2
        #[cfg(feature = "editor")]
        if let Some(slot) = self.editor_slot.get_mut().take() {
            if self.is_main_thread() {
                drop(slot);
            } else {
                crate::nice_error!(
                    "AU: the wrapper was dropped off the main thread with an editor open; leaking \
                     the editor"
                );
                std::mem::forget(slot);
            }
        }

        for &address in &self.cf_names {
            if let Some(string) = NonNull::new(address as *mut CFString) {
                // SAFETY: These are the references `new` leaked with `CFRetained::into_raw`.
                drop(unsafe { CFRetained::from_raw(string) });
            }
        }
    }
}
