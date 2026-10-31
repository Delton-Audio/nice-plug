// DELTON FORK DELTA: au-1, AUv2 wrapper
//! An Audio Unit v2 wrapper (macOS only). The pure parts (channel layouts, the parameter table,
//! unit conversions, render argument checks) build and test on every OS; everything that calls
//! into Apple's frameworks is behind `cfg(target_os = "macos")`.

pub(crate) mod convert;
pub(crate) mod layout;
pub(crate) mod param_table;
pub(crate) mod render_check;
// DELTON FORK DELTA: au-2
pub(crate) mod view_names;
// DELTON FORK DELTA: au-4a
pub(crate) mod workgroup;
// DELTON FORK DELTA: au-4b
pub(crate) mod midi;

#[cfg(target_os = "macos")]
pub(crate) mod buffers;
#[cfg(target_os = "macos")]
pub(crate) mod context;
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod factory;
#[cfg(target_os = "macos")]
pub(crate) mod state;
#[cfg(target_os = "macos")]
pub(crate) mod sys;
// DELTON FORK DELTA: au-2
#[cfg(all(target_os = "macos", feature = "editor"))]
pub(crate) mod view;
#[cfg(target_os = "macos")]
pub(crate) mod wrapper;

use nice_plug_core::midi::MidiConfig;
use nice_plug_core::plugin::Plugin;

/// The Audio Unit type, as the four-char code in the component description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuType {
    /// `aufx`: an effect.
    Effect,
    /// `aumf`: an effect that takes MIDI.
    MusicEffect,
    /// `aumu`: an instrument.
    Instrument,
    /// `aumi`: a MIDI processor.
    MidiProcessor,
}

impl AuType {
    /// The four-char code of this type, as the bytes appear in the component description.
    pub const fn four_cc(self) -> [u8; 4] {
        match self {
            AuType::Effect => *b"aufx",
            AuType::MusicEffect => *b"aumf",
            AuType::Instrument => *b"aumu",
            AuType::MidiProcessor => *b"aumi",
        }
    }
}

/// Provides auxiliary metadata needed for an AUv2 plugin.
pub trait AuPlugin: Plugin {
    /// The manufacturer code. Saved in every host project: never change it after release. Printable
    /// ASCII; Apple reserves codes made only of lowercase letters.
    const AU_MANUFACTURER: [u8; 4];
    /// The plugin's own code under that manufacturer. Saved in every host project.
    const AU_SUBTYPE: [u8; 4];
    /// `None` derives the type from the plugin's layouts and MIDI input (see [`au_type_of`]).
    const AU_TYPE: Option<AuType> = None;
}

/// The type a plugin registers as: `AU_TYPE` if set, else derived from the FIRST audio IO layout and
/// `MIDI_INPUT`: main input + no MIDI input = Effect; main input + MIDI input = MusicEffect; no main
/// input + MIDI input = Instrument; no main input and no MIDI input = Effect (and `factory` logs once
/// that such a plugin should set `AU_TYPE`).
pub const fn au_type_of<P: AuPlugin>() -> AuType {
    if let Some(au_type) = P::AU_TYPE {
        return au_type;
    }

    let has_main_input = match P::AUDIO_IO_LAYOUTS.first() {
        Some(layout) => layout.main_input_channels.is_some(),
        None => false,
    };
    // `MidiConfig` comparisons are not const-callable, so this matches instead of using `>=`
    let has_midi_input = !matches!(P::MIDI_INPUT, MidiConfig::None);

    match (has_main_input, has_midi_input) {
        (true, false) => AuType::Effect,
        (true, true) => AuType::MusicEffect,
        (false, true) => AuType::Instrument,
        (false, false) => AuType::Effect,
    }
}

/// Export an AUv2 plugin from this library using the provided plugin type. On macOS this exports the
/// `NiceAuv2Factory` symbol that the bundle's `Info.plist` names as its `factoryFunction`.
///
/// The `@unexported` arm only defines the `auv2::factory` function, for callers that put their own
/// trampoline in front of the exported symbol.
#[macro_export]
macro_rules! nice_export_auv2 {
    (@unexported $plugin_ty:ty) => {
        /// The AUv2 factory for this plugin, without the exported symbol.
        #[cfg(target_os = "macos")]
        #[doc(hidden)]
        mod auv2 {
            // Because the `$plugin_ty` is likely defined in the enclosing scope. This works even
            // if the type is not public because this is a child module.
            #[allow(unused_imports)]
            use super::*;

            /// # Safety
            /// Called by the AudioComponent loader with a valid description pointer (or null).
            pub unsafe extern "C-unwind" fn factory(
                desc: *const ::std::ffi::c_void,
            ) -> *mut ::std::ffi::c_void {
                unsafe { $crate::wrapper::auv2::factory::factory::<$plugin_ty>(desc) }
            }
        }
    };
    ($plugin_ty:ty) => {
        $crate::nice_export_auv2!(@unexported $plugin_ty);

        /// The factory function named in the bundle's `Info.plist` (`factoryFunction`).
        #[cfg(target_os = "macos")]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub unsafe extern "C-unwind" fn NiceAuv2Factory(
            desc: *const ::std::ffi::c_void,
        ) -> *mut ::std::ffi::c_void {
            unsafe { self::auv2::factory(desc) }
        }
    };
}
