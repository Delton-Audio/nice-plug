// DELTON FORK DELTA: au-1, AUv2 wrapper
//! Render argument validation. Pure: builds and tests on every OS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// What a valid render call should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderCheck {
    /// The arguments are fine: render the block.
    Proceed,
    /// Zero frames: return `noErr` without touching anything.
    NothingToDo,
}

/// Why a render call is refused. The macOS side maps each to its `kAudioUnitErr_*` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// The unit is not initialized.
    Uninitialized,
    /// The output element is not 0.
    InvalidElement,
    /// More frames than `MaximumFramesPerSlice`.
    TooManyFrames,
    /// The host's buffer list does not have one buffer per output channel.
    FormatNotSupported,
}

/// In this order: not initialized -> Uninitialized; output_element != 0 -> InvalidElement;
/// frames > max_frames -> TooManyFrames; frames == 0 -> Ok(NothingToDo);
/// buffer_count != expected_channels -> FormatNotSupported; else Ok(Proceed).
pub fn check_render(
    initialized: bool,
    output_element: u32,
    frames: u32,
    max_frames: u32,
    buffer_count: u32,
    expected_channels: u32,
) -> Result<RenderCheck, RenderError> {
    if !initialized {
        return Err(RenderError::Uninitialized);
    }
    if output_element != 0 {
        return Err(RenderError::InvalidElement);
    }
    if frames > max_frames {
        return Err(RenderError::TooManyFrames);
    }
    if frames == 0 {
        return Ok(RenderCheck::NothingToDo);
    }
    if buffer_count != expected_channels {
        return Err(RenderError::FormatNotSupported);
    }

    Ok(RenderCheck::Proceed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninitialized_is_refused() {
        assert_eq!(
            check_render(false, 0, 64, 512, 2, 2),
            Err(RenderError::Uninitialized)
        );
    }

    #[test]
    fn wrong_element_is_refused() {
        assert_eq!(
            check_render(true, 1, 64, 512, 2, 2),
            Err(RenderError::InvalidElement)
        );
    }

    #[test]
    fn too_many_frames_is_refused() {
        assert_eq!(
            check_render(true, 0, 513, 512, 2, 2),
            Err(RenderError::TooManyFrames)
        );
        assert_eq!(
            check_render(true, 0, 512, 512, 2, 2),
            Ok(RenderCheck::Proceed)
        );
    }

    #[test]
    fn zero_frames_does_nothing() {
        assert_eq!(
            check_render(true, 0, 0, 512, 2, 2),
            Ok(RenderCheck::NothingToDo)
        );
    }

    #[test]
    fn buffer_count_mismatch_is_refused() {
        assert_eq!(
            check_render(true, 0, 64, 512, 1, 2),
            Err(RenderError::FormatNotSupported)
        );
    }

    #[test]
    fn valid_call_proceeds() {
        assert_eq!(
            check_render(true, 0, 64, 512, 2, 2),
            Ok(RenderCheck::Proceed)
        );
    }

    #[test]
    fn checks_run_in_the_documented_order() {
        // Everything is wrong at once: the first check wins
        assert_eq!(
            check_render(false, 3, 9999, 512, 1, 2),
            Err(RenderError::Uninitialized)
        );
        assert_eq!(
            check_render(true, 3, 9999, 512, 1, 2),
            Err(RenderError::InvalidElement)
        );
        assert_eq!(
            check_render(true, 0, 9999, 512, 1, 2),
            Err(RenderError::TooManyFrames)
        );
        // Zero frames wins over the buffer mismatch
        assert_eq!(
            check_render(true, 0, 0, 512, 1, 2),
            Ok(RenderCheck::NothingToDo)
        );
    }
}
