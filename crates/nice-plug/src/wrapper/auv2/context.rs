// DELTON FORK DELTA: au-1, AUv2 wrapper
//! The contexts handed to the plugin: activate and process. (The GUI context arrives with the
//! editor in au-2.)
use atomic_refcell::AtomicRef;
use nice_plug_core::audio_setup::ProcessMode;
use nice_plug_core::context::activate::ActivateContext;
use nice_plug_core::context::process::{
    ParamAutomationPoint, ProcessContext, SendEventError, Transport,
};
use nice_plug_core::context::PluginApi;
use nice_plug_core::midi::PluginNoteEvent;
use std::cell::Cell;
use std::sync::atomic::Ordering;

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

    fn next_event(&mut self) -> Option<PluginNoteEvent<P>> {
        // MIDI input arrives with au-4
        None
    }

    fn try_send_event(
        &mut self,
        event: PluginNoteEvent<P>,
    ) -> Result<(), (PluginNoteEvent<P>, SendEventError)> {
        // MIDI output arrives with au-4
        Err((event, SendEventError::NoOutputBuffer))
    }

    fn set_latency_samples(&self, samples: u32) {
        self.inner.set_latency_samples(samples)
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {
        // This is only supported by CLAP
    }

    fn request_restart(&self) {
        // AU has no restart request; hosts re-read properties when the wrapper notifies them
        crate::nice_warn!("AUv2 has no restart request, ignoring");
    }
}
