// DELTON FORK DELTA: au-1, AUv2 wrapper
//! Channel layouts: what `AUDIO_IO_LAYOUTS` looks like to an AU host, and picking the layout that
//! matches the stream formats a host set. Pure: builds and tests on every OS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use nice_plug_core::audio_setup::AudioIOLayout;
use std::num::NonZeroU32;

/// AUChannelInfo, as plain integers (the macOS side copies them into the real struct).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPair {
    pub input: i16,
    pub output: i16,
}

/// The element structure every AU-visible layout must share: taken from layouts[0]. AU has one
/// element count per scope for the lifetime of the unit, only the channel counts may change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementShape {
    pub has_main_input: bool,
    pub has_main_output: bool,
    pub aux_inputs: usize,
    pub aux_outputs: usize,
}

impl ElementShape {
    /// The shape of one layout.
    pub fn of(layout: &AudioIOLayout) -> Self {
        Self {
            has_main_input: layout.main_input_channels.is_some(),
            has_main_output: layout.main_output_channels.is_some(),
            aux_inputs: layout.aux_input_ports.len(),
            aux_outputs: layout.aux_output_ports.len(),
        }
    }

    /// Input elements: main input (if any) at 0, then aux inputs.
    pub fn input_elements(&self) -> u32 {
        self.has_main_input as u32 + self.aux_inputs as u32
    }

    /// Output elements: main output (if any) at 0, then aux outputs.
    pub fn output_elements(&self) -> u32 {
        self.has_main_output as u32 + self.aux_outputs as u32
    }
}

/// Indices of the layouts AU can offer: those whose ElementShape equals layouts[0]'s. Empty input ->
/// empty.
pub fn au_layouts(layouts: &[AudioIOLayout]) -> Vec<usize> {
    let Some(first) = layouts.first() else {
        return Vec::new();
    };

    let shape = ElementShape::of(first);
    layouts
        .iter()
        .enumerate()
        .filter(|(_, layout)| ElementShape::of(layout) == shape)
        .map(|(idx, _)| idx)
        .collect()
}

fn channels_or_zero(channels: Option<NonZeroU32>) -> i16 {
    channels
        .map(|channels| channels.get().min(i16::MAX as u32) as i16)
        .unwrap_or(0)
}

/// One ChannelPair per offered layout, in order, deduplicated (first occurrence kept). A missing main
/// port is 0.
pub fn supported_channels(layouts: &[AudioIOLayout]) -> Vec<ChannelPair> {
    let mut pairs: Vec<ChannelPair> = Vec::new();
    for idx in au_layouts(layouts) {
        let layout = &layouts[idx];
        let pair = ChannelPair {
            input: channels_or_zero(layout.main_input_channels),
            output: channels_or_zero(layout.main_output_channels),
        };
        if !pairs.contains(&pair) {
            pairs.push(pair);
        }
    }

    pairs
}

/// The channel count of each input element and output element of a layout, in element order.
pub fn element_channels(layout: &AudioIOLayout) -> (Vec<u32>, Vec<u32>) {
    let mut inputs = Vec::with_capacity(1 + layout.aux_input_ports.len());
    if let Some(main) = layout.main_input_channels {
        inputs.push(main.get());
    }
    inputs.extend(layout.aux_input_ports.iter().map(|ports| ports.get()));

    let mut outputs = Vec::with_capacity(1 + layout.aux_output_ports.len());
    if let Some(main) = layout.main_output_channels {
        outputs.push(main.get());
    }
    outputs.extend(layout.aux_output_ports.iter().map(|ports| ports.get()));

    (inputs, outputs)
}

/// The first offered layout whose element_channels equal (inputs, outputs). None if none matches.
pub fn pick_layout(layouts: &[AudioIOLayout], inputs: &[u32], outputs: &[u32]) -> Option<usize> {
    au_layouts(layouts).into_iter().find(|&idx| {
        let (layout_inputs, layout_outputs) = element_channels(&layouts[idx]);
        layout_inputs == inputs && layout_outputs == outputs
    })
}

/// Whether `channels` is the count of input (or output, by `is_input`) element `element` in ANY
/// offered layout.
pub fn channels_allowed(
    layouts: &[AudioIOLayout],
    is_input: bool,
    element: u32,
    channels: u32,
) -> bool {
    au_layouts(layouts).into_iter().any(|idx| {
        let (inputs, outputs) = element_channels(&layouts[idx]);
        let elements = if is_input { inputs } else { outputs };
        elements.get(element as usize) == Some(&channels)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nice_plug_core::audio_setup::new_nonzero_u32;

    const L_STEREO: AudioIOLayout = AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    };
    const L_MONO: AudioIOLayout = AudioIOLayout {
        main_input_channels: NonZeroU32::new(1),
        main_output_channels: NonZeroU32::new(1),
        ..AudioIOLayout::const_default()
    };
    const L_SC: AudioIOLayout = AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[new_nonzero_u32(1)],
        ..AudioIOLayout::const_default()
    };
    const L_GEN: AudioIOLayout = AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    };

    #[test]
    fn supported_channels_dedups_and_keeps_order() {
        assert_eq!(
            supported_channels(&[L_STEREO, L_MONO, L_STEREO]),
            vec![
                ChannelPair {
                    input: 2,
                    output: 2
                },
                ChannelPair {
                    input: 1,
                    output: 1
                }
            ]
        );
        assert_eq!(
            supported_channels(&[L_GEN]),
            vec![ChannelPair {
                input: 0,
                output: 2
            }]
        );
        assert!(supported_channels(&[]).is_empty());
    }

    #[test]
    fn au_layouts_keeps_the_first_shape_only() {
        assert_eq!(au_layouts(&[L_STEREO, L_SC, L_MONO]), vec![0, 2]);
        assert!(au_layouts(&[]).is_empty());
    }

    #[test]
    fn element_counts() {
        assert_eq!(ElementShape::of(&L_SC).input_elements(), 2);
        assert_eq!(ElementShape::of(&L_SC).output_elements(), 1);
        assert_eq!(ElementShape::of(&L_GEN).input_elements(), 0);
        assert_eq!(ElementShape::of(&L_GEN).output_elements(), 1);
    }

    #[test]
    fn element_channels_lists_main_then_aux() {
        assert_eq!(element_channels(&L_SC), (vec![2, 1], vec![2]));
        assert_eq!(element_channels(&L_GEN), (vec![], vec![2]));
    }

    #[test]
    fn pick_layout_matches_all_elements() {
        assert_eq!(pick_layout(&[L_STEREO, L_MONO], &[1], &[1]), Some(1));
        assert_eq!(pick_layout(&[L_STEREO, L_MONO], &[2], &[1]), None);
        assert_eq!(pick_layout(&[L_GEN], &[], &[2]), Some(0));
        // A layout of a different shape is not offered, even if its channels match
        assert_eq!(pick_layout(&[L_STEREO, L_SC], &[2, 1], &[2]), None);
    }

    #[test]
    fn channels_allowed_checks_the_element() {
        assert!(channels_allowed(&[L_STEREO, L_MONO], true, 0, 1));
        assert!(!channels_allowed(&[L_STEREO, L_MONO], true, 0, 3));
        assert!(!channels_allowed(&[L_STEREO], true, 1, 2));
        assert!(channels_allowed(&[L_STEREO], false, 0, 2));
        assert!(!channels_allowed(&[L_STEREO], false, 1, 2));
    }
}
