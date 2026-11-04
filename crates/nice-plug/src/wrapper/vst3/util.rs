use std::cmp;
#[cfg(feature = "editor")]
use vst3::Steinberg::FIDString;
use vst3::Steinberg::Vst::TChar;
use widestring::U16CString;

/// When `Plugin::MIDI_INPUT` is set to `MidiConfig::MidiCCs` or higher then we'll register 130*16
/// additional parameters to handle MIDI CCs, channel pressure, and pitch bend, in that order.
/// vst3 doesn't expose these constants.
pub const VST3_MIDI_CCS: u32 = 130;
pub const VST3_MIDI_CHANNELS: u32 = 16;
/// The number of parameters we'll need to register if the plugin accepts MIDI CCs.
pub const VST3_MIDI_NUM_PARAMS: u32 = VST3_MIDI_CCS * VST3_MIDI_CHANNELS;
/// The start of the MIDI CC parameter ranges. We'll print an assertion failure if any of the
/// plugin's parameters overlap with this range. The mapping to a parameter index is
/// `VST3_MIDI_PARAMS_START + (cc_idx + (channel * VST3_MIDI_CCS))`.
pub const VST3_MIDI_PARAMS_START: u32 = VST3_MIDI_PARAMS_END - VST3_MIDI_NUM_PARAMS;
/// The (exclusive) end of the MIDI CC parameter range. Anything above this is reserved by the host.
pub const VST3_MIDI_PARAMS_END: u32 = 1 << 31;

/// The id of the one program list a plugin with [`HostPresets`] has. A program list id of 0 would
/// be `kNoProgramListId`, so the id is 1.
// DELTON FORK DELTA: hpre-3
///
/// [`HostPresets`]: nice_plug_core::host_presets::HostPresets
pub const PROGRAM_LIST_ID: i32 = 1;
/// The id of the wrapper-owned program parameter (`kIsProgramChange | kIsList`): the id just below
/// the generated MIDI CC parameters. A plugin parameter whose hash equals it is a collision.
// DELTON FORK DELTA: hpre-3
pub const VST3_PROGRAM_PARAM_ID: u32 = VST3_MIDI_PARAMS_START - 1;
/// VST3's `kCtrlProgramChange`: the controller number `IMidiMapping` maps MIDI program change to.
// DELTON FORK DELTA: hpre-3
pub const VST3_CTRL_PROGRAM_CHANGE: i16 = 130;

/// The number of hidden parameters that carry MIDI bank select and program change: per channel the
/// bank MSB (CC 0), the bank LSB (CC 32) and the program (`kCtrlProgramChange`).
// DELTON FORK DELTA: hpre-3b
pub const VST3_MIDI_PRESET_NUM_PARAMS: u32 = 3 * VST3_MIDI_CHANNELS;
/// The start of the MIDI bank select / program change parameter range, just below the program
/// parameter. The id of channel `c`'s parameter `k` (0 bank MSB, 1 bank LSB, 2 program) is
/// `VST3_MIDI_PRESET_PARAMS_START + c * 3 + k`. A plugin parameter inside the range is a collision.
// DELTON FORK DELTA: hpre-3b
pub const VST3_MIDI_PRESET_PARAMS_START: u32 = VST3_PROGRAM_PARAM_ID - VST3_MIDI_PRESET_NUM_PARAMS;
/// The (exclusive) end of the MIDI bank select / program change parameter range.
// DELTON FORK DELTA: hpre-3b
pub const VST3_MIDI_PRESET_PARAMS_END: u32 = VST3_PROGRAM_PARAM_ID;

/// The MIDI bank select / program change parameter a parameter id is: `(channel, k)` with `k` 0
/// for the bank MSB, 1 for the bank LSB and 2 for the program. `None` for any other id.
// DELTON FORK DELTA: hpre-3b
pub fn midi_preset_param(id: u32) -> Option<(u8, u8)> {
    if !(VST3_MIDI_PRESET_PARAMS_START..VST3_MIDI_PRESET_PARAMS_END).contains(&id) {
        return None;
    }
    let relative = id - VST3_MIDI_PRESET_PARAMS_START;
    Some(((relative / 3) as u8, (relative % 3) as u8))
}

/// The id of channel `channel`'s MIDI bank select / program change parameter `k` (0 bank MSB, 1
/// bank LSB, 2 program).
// DELTON FORK DELTA: hpre-3b
pub fn midi_preset_param_id(channel: u8, k: u8) -> u32 {
    VST3_MIDI_PRESET_PARAMS_START + channel as u32 * 3 + k as u32
}

/// A MIDI value (0 to 127) from a parameter's normalized value.
// DELTON FORK DELTA: hpre-3b
pub fn midi_value_from_normalized(value: f64) -> u8 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * 127.0).round() as u8
}

/// The index into the program list that MIDI Program Change `program` selects while bank select
/// holds `msb` and `lsb`: `(msb * 128 + lsb) * 128 + program`, a flat bank of 128 programs. The
/// backend has the same function for CLAP and AU (the fork cannot depend on it), and both are
/// tested on the same cases.
// DELTON FORK DELTA: hpre-3b
pub fn program_index(msb: u8, lsb: u8, program: u8) -> usize {
    (msb as usize & 0x7f) * 128 * 128 + (lsb as usize & 0x7f) * 128 + (program as usize & 0x7f)
}

/// A list parameter's normalized value for program `index` of a list with `step_count + 1`
/// programs (`index / step_count`; 0 for a list of one or none). An index past the list is the
/// last program.
// DELTON FORK DELTA: hpre-3
pub fn program_to_normalized(index: usize, step_count: i32) -> f64 {
    if step_count <= 0 {
        return 0.0;
    }
    let step_count = step_count as usize;
    index.min(step_count) as f64 / step_count as f64
}

/// The program index a list parameter's normalized value selects: `min(step_count, floor(value *
/// (step_count + 1)))`, the VST3 SDK's rule. A value outside `[0, 1]` clamps; NaN is program 0.
// DELTON FORK DELTA: hpre-3
pub fn program_from_normalized(value: f64, step_count: i32) -> usize {
    if step_count <= 0 || value.is_nan() {
        return 0;
    }
    let step_count = step_count as usize;
    let scaled = (value.clamp(0.0, 1.0) * (step_count as f64 + 1.0)).floor();
    (scaled as usize).min(step_count)
}

/// Early exit out of a VST3 function when one of the passed pointers is null
macro_rules! check_null_ptr {
    ($ptr:expr $(, $ptrs:expr)* $(, )?) => {
        check_null_ptr_msg!("Null pointer passed to function", $ptr $(, $ptrs)*)
    };
}

/// The same as [`check_null_ptr!`], but with a custom message.
macro_rules! check_null_ptr_msg {
    ($msg:expr, $ptr:expr $(, $ptrs:expr)* $(, )?) => {
        if $ptr.is_null() $(|| $ptrs.is_null())* {
            crate::nice_debug_assert_failure!($msg);
            return kInvalidArgument;
        }
    };
}

/// The same as [`strlcpy()`], but for VST3's fun UTF-16 strings instead.
pub fn u16strlcpy(dest: &mut [TChar], src: &str) {
    if dest.is_empty() {
        return;
    }

    let src_utf16 = match U16CString::from_str(src) {
        Ok(s) => s,
        Err(err) => {
            crate::nice_debug_assert_failure!("Invalid UTF-16 string: {}", err);
            return;
        }
    };
    let src_utf16_chars = src_utf16.as_slice();

    // Make sure there's always room for a null terminator
    let copy_len = cmp::min(dest.len() - 1, src_utf16_chars.len());
    dest[..copy_len].copy_from_slice(&src_utf16_chars[..copy_len]);
    dest[copy_len] = 0;
}

/// Compare a host-provided [`FIDString`] to one of the SDK's string constants. These are plain C
/// string pointers, so pointer equality is not sufficient.
#[cfg(feature = "editor")]
pub unsafe fn fid_matches(type_: FIDString, expected: FIDString) -> bool {
    use std::ffi::CStr;

    unsafe { !type_.is_null() && CStr::from_ptr(type_) == CStr::from_ptr(expected) }
}

#[cfg(test)]
mod miri {
    use widestring::U16CStr;

    use super::*;

    #[test]
    fn program_param_id_is_just_below_the_midi_cc_range() {
        assert_eq!(VST3_PROGRAM_PARAM_ID + 1, VST3_MIDI_PARAMS_START);
    }

    #[test]
    fn program_index_is_a_flat_bank_of_128_programs() {
        assert_eq!(program_index(0, 0, 0), 0);
        assert_eq!(program_index(0, 0, 5), 5);
        assert_eq!(program_index(0, 1, 0), 128);
        assert_eq!(program_index(0, 1, 5), 133);
        assert_eq!(program_index(1, 0, 0), 128 * 128);
        assert_eq!(program_index(1, 1, 0), 129 * 128);
        assert_eq!(
            program_index(127, 127, 127),
            127 * 128 * 128 + 127 * 128 + 127
        );
        assert_eq!(program_index(127, 127, 127), 128 * 128 * 128 - 1);
    }

    #[test]
    fn program_midi_preset_params_sit_below_the_program_parameter() {
        assert_eq!(VST3_MIDI_PRESET_PARAMS_END, VST3_PROGRAM_PARAM_ID);
        assert_eq!(VST3_MIDI_PRESET_PARAMS_START + 48, VST3_PROGRAM_PARAM_ID);
        for channel in 0..16u8 {
            for k in 0..3u8 {
                let id = midi_preset_param_id(channel, k);
                assert!((VST3_MIDI_PRESET_PARAMS_START..VST3_MIDI_PRESET_PARAMS_END).contains(&id));
                assert_eq!(midi_preset_param(id), Some((channel, k)));
            }
        }
        assert_eq!(midi_preset_param(VST3_PROGRAM_PARAM_ID), None);
        assert_eq!(midi_preset_param(VST3_MIDI_PRESET_PARAMS_START - 1), None);
    }

    #[test]
    fn program_midi_value_rounds_a_normalized_value() {
        assert_eq!(midi_value_from_normalized(0.0), 0);
        assert_eq!(midi_value_from_normalized(1.0 / 127.0), 1);
        assert_eq!(midi_value_from_normalized(5.0 / 127.0), 5);
        assert_eq!(midi_value_from_normalized(1.0), 127);
        assert_eq!(midi_value_from_normalized(2.0), 127);
        assert_eq!(midi_value_from_normalized(f64::NAN), 0);
    }

    #[test]
    fn program_normalized_of_an_empty_or_one_item_list_is_zero() {
        // step count 0 = a list of one program (or none)
        assert_eq!(program_to_normalized(0, 0), 0.0);
        assert_eq!(program_to_normalized(5, 0), 0.0);
        assert_eq!(program_from_normalized(0.0, 0), 0);
        assert_eq!(program_from_normalized(1.0, 0), 0);
        assert_eq!(program_from_normalized(0.7, 0), 0);
    }

    #[test]
    fn program_normalized_of_two_programs() {
        assert_eq!(program_to_normalized(0, 1), 0.0);
        assert_eq!(program_to_normalized(1, 1), 1.0);
        assert_eq!(program_from_normalized(0.0, 1), 0);
        assert_eq!(program_from_normalized(0.49, 1), 0);
        assert_eq!(program_from_normalized(0.5, 1), 1);
        assert_eq!(program_from_normalized(1.0, 1), 1);
    }

    #[test]
    fn program_normalized_round_trips_for_many_programs() {
        for count in [3usize, 4, 7, 100, 128, 129, 1000, 5000] {
            let step_count = count as i32 - 1;
            for index in 0..count {
                let value = program_to_normalized(index, step_count);
                assert!((0.0..=1.0).contains(&value), "{index} of {count}");
                assert_eq!(
                    program_from_normalized(value, step_count),
                    index,
                    "{index} of {count}"
                );
            }
        }
    }

    #[test]
    fn program_normalized_rounds_at_the_edges() {
        // four programs: 0, 1/3, 2/3, 1; a value selects the bucket it falls in, and 1.0 is the
        // last program, not one past it
        assert_eq!(program_from_normalized(0.0, 3), 0);
        assert_eq!(program_from_normalized(0.2499, 3), 0);
        assert_eq!(program_from_normalized(0.25, 3), 1);
        assert_eq!(program_from_normalized(0.7499, 3), 2);
        assert_eq!(program_from_normalized(0.75, 3), 3);
        assert_eq!(program_from_normalized(1.0, 3), 3);
        // out of range values clamp, NaN is the first program
        assert_eq!(program_from_normalized(-0.5, 3), 0);
        assert_eq!(program_from_normalized(1.5, 3), 3);
        assert_eq!(program_from_normalized(f64::NAN, 3), 0);
        // an index past the list is the last program
        assert_eq!(program_to_normalized(9, 3), 1.0);
    }

    #[test]
    fn u16strlcpy_normal() {
        let mut dest = [0; 256];
        u16strlcpy(&mut dest, "Hello, world!");

        assert_eq!(
            unsafe { U16CStr::from_ptr_str(dest.as_ptr() as *const u16) }
                .to_string()
                .unwrap(),
            "Hello, world!"
        );
    }

    #[test]
    fn u16strlcpy_overflow() {
        let mut dest = [0; 6];
        u16strlcpy(&mut dest, "Hello, world!");

        assert_eq!(
            unsafe { U16CStr::from_ptr_str(dest.as_ptr() as *const u16) }
                .to_string()
                .unwrap(),
            "Hello"
        );
    }
}
