// DELTON FORK DELTA: au-1, AUv2 wrapper
//! The contexts handed to the plugin: activate, process and (with the `editor` feature) the GUI
//! context the editor gets in `spawn`.
use atomic_refcell::{AtomicRef, AtomicRefMut};
use nice_plug_core::audio_setup::ProcessMode;
use nice_plug_core::context::activate::ActivateContext;
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use nice_plug_core::context::gui::GuiContextInner;
use nice_plug_core::context::process::{
    AudioWorkgroup, ParamAutomationPoint, ProcessContext, SendEventError, Transport,
};
use nice_plug_core::context::PluginApi;
use nice_plug_core::midi::PluginNoteEvent;
// DELTON FORK DELTA: au-4b
use nice_plug_core::midi::MidiResult;
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use nice_plug_core::params::internals::ParamPtr;
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use nice_plug_core::plugin::PluginState;
use std::borrow::Borrow;
use std::cell::Cell;
use std::sync::atomic::Ordering;
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use std::sync::Weak;

// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
use super::sys::AudioUnitEventType;
// DELTON FORK DELTA: au-4b
use super::midi;
use super::wrapper::{Task, Wrapper};
use super::AuPlugin;
use crate::wrapper::util::raw_automation::RawAutomation;

/// An [`ActivateContext`] implementation for the wrapper.
///
/// A latency change made during `activate()` is announced by the wrapper after Initialize has
/// finished (see [`Wrapper::initialize`]), not from inside the plugin's lock.
pub(crate) struct WrapperActivateContext<'a, P: AuPlugin> {
    pub(super) inner: &'a Wrapper<P>,
    /// Whether `set_latency_samples()` changed the latency.
    pub(super) latency_changed: Cell<bool>,
}

/// A [`ProcessContext`] implementation for the wrapper. Holds the raw automation guard so reading
/// the points does not touch the borrow flag again.
pub(crate) struct WrapperProcessContext<'a, P: AuPlugin> {
    pub(super) inner: &'a Wrapper<P>,
    pub(super) raw_automation_guard: AtomicRef<'a, RawAutomation>,
    pub(super) transport: Transport,
    // DELTON FORK DELTA: au-4b
    /// This block's input events, sorted. `next_input` is the next one `next_event` returns.
    pub(super) input_events: AtomicRef<'a, Vec<PluginNoteEvent<P>>>,
    // DELTON FORK DELTA: au-4b
    pub(super) next_input: usize,
    // DELTON FORK DELTA: au-4b
    /// What `try_send_event` queued this block: (timing, message start in `output_bytes`, length).
    pub(super) output_events: AtomicRefMut<'a, Vec<(u32, u32, u32)>>,
    // DELTON FORK DELTA: au-4b
    /// The bytes of this block's outgoing messages.
    pub(super) output_bytes: AtomicRefMut<'a, Vec<u8>>,
}

impl<P: AuPlugin> ActivateContext<P> for WrapperActivateContext<'_, P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Auv2
    }

    fn execute(&self, task: P::BackgroundTask) {
        (self.inner.task_executor.lock())(task);
    }

    fn set_latency_samples(&self, samples: u32) {
        if self.inner.current_latency.swap(samples, Ordering::SeqCst) != samples {
            self.latency_changed.set(true);
        }
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {
        // This is only supported by CLAP
    }
}

impl<P: AuPlugin> ProcessContext<P> for WrapperProcessContext<'_, P> {
    fn param_automation(&self) -> &[ParamAutomationPoint] {
        self.raw_automation_guard.points()
    }

    fn param_automation_overflowed(&self) -> bool {
        self.raw_automation_guard.overflowed()
    }

    fn plugin_api(&self) -> PluginApi {
        PluginApi::Auv2
    }

    fn execute_background(&self, task: P::BackgroundTask) {
        let task_posted = self.inner.schedule_background(Task::PluginTask(task));
        crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    fn execute_gui(&self, task: P::BackgroundTask) {
        let task_posted = self.inner.schedule_gui(Task::PluginTask(task));
        crate::nice_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    #[inline]
    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn process_mode(&self) -> ProcessMode {
        self.inner.current_process_mode.load()
    }

    // DELTON FORK DELTA: au-4b
    fn next_event(&mut self) -> Option<PluginNoteEvent<P>> {
        let event = self.input_events.get(self.next_input).cloned();
        if event.is_some() {
            self.next_input += 1;
        }

        event
    }

    fn try_send_event(
        &mut self,
        event: PluginNoteEvent<P>,
    ) -> Result<(), (PluginNoteEvent<P>, SendEventError)> {
        // DELTON FORK DELTA: au-4b
        let Some(message) = midi::midi_out_message(P::MIDI_OUTPUT, &event) else {
            return Err((
                event,
                SendEventError::InvalidEvent {
                    midi_output_config: P::MIDI_OUTPUT,
                },
            ));
        };

        let bytes: &[u8] = match &message {
            MidiResult::Basic(bytes) => &bytes[..],
            MidiResult::SysEx(buffer, length) => {
                match Borrow::<[u8]>::borrow(buffer).get(..*length) {
                    Some(bytes) => bytes,
                    // A SysEx message that claims to be longer than its own buffer
                    None => {
                        return Err((
                            event,
                            SendEventError::InvalidEvent {
                                midi_output_config: P::MIDI_OUTPUT,
                            },
                        ))
                    }
                }
            }
        };

        let start = self.output_bytes.len();
        if start + bytes.len() > self.output_bytes.capacity()
            || self.output_events.len() >= self.output_events.capacity()
        {
            return Err((event, SendEventError::HostBufferFull));
        }

        self.output_bytes.extend_from_slice(bytes);
        self.output_events
            .push((event.timing(), start as u32, bytes.len() as u32));

        Ok(())
    }

    fn set_latency_samples(&self, samples: u32) {
        self.inner.set_latency_samples(samples)
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {
        // This is only supported by CLAP
    }

    // DELTON FORK DELTA: au-4a
    fn audio_workgroup(&self) -> Option<AudioWorkgroup> {
        self.inner.workgroup.current()
    }

    fn request_restart(&self) {
        // AU has no restart request; hosts re-read properties when the wrapper notifies them
        crate::nice_warn!("AUv2 has no restart request, ignoring");
    }
}

/// A [`GuiContext`][nice_plug_core::context::gui::GuiContext] implementation for the wrapper. This
/// is passed to the plugin in `Editor::spawn()` so it can interact with the rest of the plugin and
/// with the host for things like setting parameters.
// DELTON FORK DELTA: au-2
#[cfg(feature = "editor")]
pub(crate) struct WrapperGuiContext<P: AuPlugin> {
    pub(super) inner: Weak<Wrapper<P>>,
    #[cfg(debug_assertions)]
    pub(super) param_gesture_checker:
        atomic_refcell::AtomicRefCell<crate::wrapper::util::context_checks::ParamGestureChecker>,
}

#[cfg(feature = "editor")]
impl<P: AuPlugin> WrapperGuiContext<P> {
    /// The wrapper and the AU parameter ID of `param`, or `None` if the wrapper is gone or the
    /// parameter is not one of the plugin's.
    fn wrapper_and_hash(&self, param: ParamPtr) -> Option<(std::sync::Arc<Wrapper<P>>, u32)> {
        let wrapper = self.inner.upgrade()?;
        let Some(&hash) = wrapper.param_ptr_to_hash.get(&param) else {
            crate::nice_debug_assert_failure!("Unknown parameter: {:?}", param);
            return None;
        };

        Some((wrapper, hash))
    }
}

#[cfg(feature = "editor")]
impl<P: AuPlugin> GuiContextInner for WrapperGuiContext<P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Auv2
    }

    // All of these functions are supposed to be called from the main thread, so we'll put some
    // trust in the caller and assume that this is indeed the case
    unsafe fn raw_begin_set_parameter(&self, param: ParamPtr) {
        let Some((wrapper, hash)) = self.wrapper_and_hash(param) else {
            return;
        };

        wrapper.notify_parameter_event(AudioUnitEventType::BeginParameterChangeGesture, hash);

        #[cfg(debug_assertions)]
        if let Some(entry) = wrapper.table.get(hash) {
            self.param_gesture_checker
                .borrow_mut()
                .begin_set_parameter(&entry.string_id);
        }
    }

    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        let Some((wrapper, hash)) = self.wrapper_and_hash(param) else {
            return;
        };

        wrapper.apply_editor_parameter(hash, normalized);

        #[cfg(debug_assertions)]
        if let Some(entry) = wrapper.table.get(hash) {
            self.param_gesture_checker
                .borrow()
                .set_parameter(&entry.string_id);
        }
    }

    unsafe fn raw_end_set_parameter(&self, param: ParamPtr) {
        let Some((wrapper, hash)) = self.wrapper_and_hash(param) else {
            return;
        };

        wrapper.notify_parameter_event(AudioUnitEventType::EndParameterChangeGesture, hash);

        #[cfg(debug_assertions)]
        if let Some(entry) = wrapper.table.get(hash) {
            self.param_gesture_checker
                .borrow_mut()
                .end_set_parameter(&entry.string_id);
        }
    }

    fn get_state(&self) -> PluginState {
        match self.inner.upgrade() {
            Some(wrapper) => wrapper.get_state_object(),
            None => {
                crate::nice_debug_assert_failure!("get_state() called after the plugin was closed");
                PluginState {
                    version: String::new(),
                    params: Default::default(),
                    fields: Default::default(),
                }
            }
        }
    }

    fn set_state(&self, state: PluginState) {
        if let Some(wrapper) = self.inner.upgrade() {
            wrapper.set_state_object_from_gui(state);
        }
    }

    fn request_restart(&self) {
        // AU has no restart request; hosts re-read properties when the wrapper notifies them
        crate::nice_warn!("AUv2 has no restart request, ignoring");
    }
}
