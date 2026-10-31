// DELTON FORK DELTA: au-4b, AUv2 wrapper
//! MIDI for the AU wrapper: what an incoming message becomes, which outgoing events a plugin may
//! send, and the timing helpers. Pure: builds and tests on every OS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use nice_plug_core::midi::sysex::SysExMessage;
use nice_plug_core::midi::{MidiConfig, MidiResult, NoteEvent};

/// The three bytes of a `MusicDeviceMIDIEvent` call. Each argument is a UInt32 in the AU API; only
/// its low byte is the MIDI byte.
pub(crate) fn short_message(status: u32, data1: u32, data2: u32) -> [u8; 3] {
    [status as u8, data1 as u8, data2 as u8]
}

/// The event an incoming MIDI message becomes, or `None` when the plugin's `MIDI_INPUT` does not
/// take it. The same rule as the CLAP wrapper (`clap/wrapper.rs:1487-1523`): `None` takes nothing;
/// `Basic` takes note on, note off, polyphonic pressure and SysEx; `MidiCCs` takes everything
/// `NoteEvent::from_midi` parses. A message `from_midi` cannot parse is `None`.
pub(crate) fn midi_in_event<S: SysExMessage>(
    config: MidiConfig,
    offset: u32,
    bytes: &[u8],
) -> Option<NoteEvent<S>> {
    let event = NoteEvent::from_midi(offset, bytes).ok()?;
    match event {
        NoteEvent::NoteOn { .. }
        | NoteEvent::NoteOff { .. }
        | NoteEvent::PolyPressure { .. }
        | NoteEvent::MidiSysEx { .. }
            if config >= MidiConfig::Basic =>
        {
            Some(event)
        }
        _ if config >= MidiConfig::MidiCCs => Some(event),
        _ => None,
    }
}

/// The MIDI an outgoing event becomes, or `None` when the plugin's `MIDI_OUTPUT` does not allow it
/// or the event has no MIDI form (`as_midi()` is `None`). `None` allows nothing; `Basic` allows note
/// on, note off, polyphonic pressure and SysEx; `MidiCCs` allows everything with a MIDI form.
pub(crate) fn midi_out_message<S: SysExMessage>(
    config: MidiConfig,
    event: &NoteEvent<S>,
) -> Option<MidiResult<S>> {
    let allowed = match event {
        NoteEvent::NoteOn { .. }
        | NoteEvent::NoteOff { .. }
        | NoteEvent::PolyPressure { .. }
        | NoteEvent::MidiSysEx { .. } => config >= MidiConfig::Basic,
        _ => config >= MidiConfig::MidiCCs,
    };
    if allowed {
        event.as_midi()
    } else {
        None
    }
}

/// Moves an event at or past the end of a `frames`-long block to the block's last sample (an AU
/// host may pass an offset beyond the next render). `frames == 0` leaves it at 0.
pub(crate) fn clamp_timing<S: SysExMessage>(event: &mut NoteEvent<S>, frames: u32) {
    let last = frames.saturating_sub(1);
    let timing = event.timing();
    if timing > last {
        event.subtract_timing(timing - last);
    }
}

/// Stable insertion sort by `key`: events with equal timing keep their arrival order (a note off
/// and a note on at the same sample must stay in order). No allocation.
pub(crate) fn sort_by_timing<T>(items: &mut [T], key: impl Fn(&T) -> u32) {
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 && key(&items[j - 1]) > key(&items[j]) {
            items.swap(j - 1, j);
            j -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nice_plug_core::midi::{Channel, Key, VoiceID};

    fn note_on(timing: u32, key: u8, velocity: f32) -> NoteEvent<()> {
        NoteEvent::NoteOn {
            timing,
            voice_id: VoiceID::Wildcard,
            channel: Channel::Number(0),
            key: Key::Number(key),
            velocity,
        }
    }

    #[test]
    fn short_message_keeps_low_bytes() {
        assert_eq!(short_message(0x190, 0x13C, 0x64), [0x90, 0x3C, 0x64]);
    }

    #[test]
    fn basic_takes_notes_and_drops_ccs() {
        let on = midi_in_event::<()>(MidiConfig::Basic, 7, &[0x90, 60, 100]);
        match on {
            Some(NoteEvent::NoteOn {
                timing,
                channel,
                key,
                ..
            }) => {
                assert_eq!(timing, 7);
                assert_eq!(key, Key::Number(60));
                assert_eq!(channel, Channel::Number(0));
            }
            other => panic!("expected a NoteOn, got {other:?}"),
        }
        assert_eq!(on.map(|event| event.timing()), Some(7));

        assert!(matches!(
            midi_in_event::<()>(MidiConfig::Basic, 0, &[0x80, 60, 0]),
            Some(NoteEvent::NoteOff { .. })
        ));
        assert!(matches!(
            midi_in_event::<()>(MidiConfig::Basic, 0, &[0xA0, 60, 50]),
            Some(NoteEvent::PolyPressure { .. })
        ));
        assert!(midi_in_event::<()>(MidiConfig::Basic, 0, &[0xB0, 7, 100]).is_none());
        assert!(midi_in_event::<()>(MidiConfig::Basic, 0, &[0xE0, 0, 64]).is_none());
    }

    #[test]
    fn midi_ccs_takes_everything_parsed() {
        assert!(matches!(
            midi_in_event::<()>(MidiConfig::MidiCCs, 0, &[0xB0, 7, 100]),
            Some(NoteEvent::MidiCC { .. })
        ));
        assert!(matches!(
            midi_in_event::<()>(MidiConfig::MidiCCs, 0, &[0xE0, 0, 64]),
            Some(NoteEvent::MidiPitchBend { .. })
        ));
    }

    #[test]
    fn none_takes_nothing() {
        assert!(midi_in_event::<()>(MidiConfig::None, 0, &[0x90, 60, 100]).is_none());
    }

    #[test]
    fn out_filter() {
        let on = note_on(0, 60, 100.0 / 127.0);
        let cc = NoteEvent::<()>::MidiCC {
            timing: 0,
            channel: 0,
            cc: 7,
            value: 0.5,
        };

        match midi_out_message(MidiConfig::Basic, &on) {
            Some(MidiResult::Basic(bytes)) => assert_eq!(bytes, [0x90, 60, 100]),
            other => panic!("expected a basic message, got {other:?}"),
        }
        assert!(midi_out_message(MidiConfig::Basic, &cc).is_none());
        assert!(matches!(
            midi_out_message(MidiConfig::MidiCCs, &cc),
            Some(MidiResult::Basic(_))
        ));
        assert!(midi_out_message(MidiConfig::None, &on).is_none());
    }

    #[test]
    fn clamp_timing_moves_late_events_to_the_last_sample() {
        let mut late = note_on(600, 60, 1.0);
        clamp_timing(&mut late, 512);
        assert_eq!(late.timing(), 511);

        let mut early = note_on(100, 60, 1.0);
        clamp_timing(&mut early, 512);
        assert_eq!(early.timing(), 100);

        let mut empty = note_on(600, 60, 1.0);
        clamp_timing(&mut empty, 0);
        assert_eq!(empty.timing(), 0);
    }

    #[test]
    fn sort_is_stable() {
        let mut items = [(5, 'a'), (1, 'b'), (5, 'c'), (0, 'd'), (1, 'e')];
        sort_by_timing(&mut items, |item| item.0);
        assert_eq!(items, [(0, 'd'), (1, 'b'), (1, 'e'), (5, 'a'), (5, 'c')]);
    }
}
