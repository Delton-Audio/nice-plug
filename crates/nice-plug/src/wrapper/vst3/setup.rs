// DELTON FORK DELTA: vst3fix-1, pure VST3 setup helpers (DEFECTS 157, 248)
//! The parts of `setupProcessing` and `setBusArrangements` that need no wrapper, so they can be
//! tested without a host.

use nice_plug_core::audio_setup::{AudioIOLayout, BufferConfig, ProcessMode};
use vst3::Steinberg::Vst::{ProcessModes_, ProcessSetup, SpeakerArrangement};

/// The buffer config `activate` gets for this setup. Its process mode is the one in `setup`: the
/// config used to be stored before the mode was mapped, so it carried the previous call's mode
/// (DEFECTS 157).
pub(super) fn buffer_config_for_setup(setup: &ProcessSetup) -> BufferConfig {
    #[allow(clippy::unnecessary_cast)]
    const K_REALTIME: i32 = ProcessModes_::kRealtime as i32;
    #[allow(clippy::unnecessary_cast)]
    const K_PREFETCH: i32 = ProcessModes_::kPrefetch as i32;
    #[allow(clippy::unnecessary_cast)]
    const K_OFFLINE: i32 = ProcessModes_::kOffline as i32;

    let process_mode = match setup.processMode {
        n if n == K_REALTIME => ProcessMode::Realtime,
        n if n == K_PREFETCH => ProcessMode::Buffered,
        n if n == K_OFFLINE => ProcessMode::Offline,
        n => {
            crate::nice_debug_assert_failure!(
                "Unknown rendering mode '{}', defaulting to realtime",
                n
            );
            ProcessMode::Realtime
        }
    };

    BufferConfig {
        sample_rate: setup.sampleRate as f32,
        min_buffer_size: None,
        max_buffer_size: setup.maxSamplesPerBlock as u32,
        process_mode,
    }
}

/// Whether `layout` is the one the host asks for with these bus arrangements. Only the bus count
/// and each bus's channel count matter; speaker positions are ignored. A layout without a main
/// output has no main output bus, so its first output bus is its first aux output (DEFECTS 248).
pub(super) fn layout_matches(
    layout: &AudioIOLayout,
    inputs: &[SpeakerArrangement],
    outputs: &[SpeakerArrangement],
) -> bool {
    // VST3 doesn't allow for optional switchable ports like CLAP does, so the number of busses
    // must match exactly. Only the channel counts can change.
    let has_main_input = layout.main_input_channels.is_some();
    let has_main_output = layout.main_output_channels.is_some();
    let num_layout_ins = usize::from(has_main_input) + layout.aux_input_ports.len();
    let num_layout_outs = usize::from(has_main_output) + layout.aux_output_ports.len();
    if inputs.len() != num_layout_ins || outputs.len() != num_layout_outs {
        return false;
    }

    let channels = |arrangement: SpeakerArrangement| arrangement.count_ones();

    if let Some(main_input_channels) = layout.main_input_channels
        && channels(inputs[0]) != main_input_channels.get()
    {
        return false;
    }
    let aux_input_start_idx = usize::from(has_main_input);
    for (aux_input_idx, channel_count) in layout.aux_input_ports.iter().enumerate() {
        if channels(inputs[aux_input_idx + aux_input_start_idx]) != channel_count.get() {
            return false;
        }
    }

    if let Some(main_output_channels) = layout.main_output_channels
        && channels(outputs[0]) != main_output_channels.get()
    {
        return false;
    }
    let aux_output_start_idx = usize::from(has_main_output);
    for (aux_output_idx, channel_count) in layout.aux_output_ports.iter().enumerate() {
        if channels(outputs[aux_output_idx + aux_output_start_idx]) != channel_count.get() {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use nice_plug_core::audio_setup::new_nonzero_u32;
    use std::num::NonZeroU32;
    use vst3::Steinberg::Vst::SpeakerArr::{kMono, kStereo};

    const MONO_PORT: &[NonZeroU32] = &[new_nonzero_u32(1)];
    const STEREO_PORT: &[NonZeroU32] = &[new_nonzero_u32(2)];

    fn setup(process_mode: i32) -> ProcessSetup {
        ProcessSetup {
            processMode: process_mode,
            symbolicSampleSize: 0,
            maxSamplesPerBlock: 512,
            sampleRate: 48000.0,
        }
    }

    #[test]
    fn the_config_carries_this_setups_process_mode() {
        let offline = buffer_config_for_setup(&setup(ProcessModes_::kOffline as i32));
        assert_eq!(offline.process_mode, ProcessMode::Offline);
        assert_eq!(offline.sample_rate, 48000.0);
        assert_eq!(offline.max_buffer_size, 512);
        assert_eq!(offline.min_buffer_size, None);

        let prefetch = buffer_config_for_setup(&setup(ProcessModes_::kPrefetch as i32));
        assert_eq!(prefetch.process_mode, ProcessMode::Buffered);
        let realtime = buffer_config_for_setup(&setup(ProcessModes_::kRealtime as i32));
        assert_eq!(realtime.process_mode, ProcessMode::Realtime);
    }

    fn stereo_effect() -> AudioIOLayout {
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        }
    }

    #[test]
    fn a_main_layout_matches_on_bus_and_channel_counts() {
        let layout = stereo_effect();
        assert!(layout_matches(&layout, &[kStereo], &[kStereo]));
        assert!(!layout_matches(&layout, &[kStereo], &[kMono]));
        assert!(!layout_matches(&layout, &[kMono], &[kStereo]));
        assert!(!layout_matches(&layout, &[], &[kStereo]));
        assert!(!layout_matches(&layout, &[kStereo, kStereo], &[kStereo]));
    }

    #[test]
    fn a_sidechain_is_the_input_bus_after_the_main_one() {
        let layout = AudioIOLayout {
            aux_input_ports: MONO_PORT,
            ..stereo_effect()
        };
        assert!(layout_matches(&layout, &[kStereo, kMono], &[kStereo]));
        assert!(!layout_matches(&layout, &[kStereo, kStereo], &[kStereo]));
    }

    #[test]
    fn an_aux_output_is_the_output_bus_after_the_main_one() {
        let layout = AudioIOLayout {
            aux_output_ports: STEREO_PORT,
            ..stereo_effect()
        };
        assert!(layout_matches(&layout, &[kStereo], &[kStereo, kStereo]));
        assert!(!layout_matches(&layout, &[kStereo], &[kStereo, kMono]));
    }

    #[test]
    fn without_a_main_output_the_first_output_bus_is_the_first_aux_output() {
        let layout = AudioIOLayout {
            main_output_channels: None,
            aux_output_ports: STEREO_PORT,
            ..stereo_effect()
        };
        assert!(layout_matches(&layout, &[kStereo], &[kStereo]));
        assert!(!layout_matches(&layout, &[kStereo], &[kMono]));
    }

    #[test]
    fn a_layout_without_outputs_matches_no_output_busses() {
        let layout = AudioIOLayout {
            main_output_channels: None,
            ..stereo_effect()
        };
        assert!(layout_matches(&layout, &[kStereo], &[]));
        assert!(!layout_matches(&layout, &[kStereo], &[kStereo]));
    }
}
