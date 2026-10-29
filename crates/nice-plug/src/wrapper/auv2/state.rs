// DELTON FORK DELTA: au-1, AUv2 wrapper
//! ClassInfo (the AU state dictionary) to and from the plugin's JSON state.
//!
//! The dictionary has the keys `version`, `type`, `subtype`, `manufacturer`, `name` and `data`. The
//! `data` entry is exactly what the VST3 wrapper writes for `getState`: the bytes of
//! `state::serialize_json`, with no length prefix.

use std::ffi::c_void;
use std::ptr::NonNull;

use super::convert::four_cc;
use super::sys::{
    kAudioUnitErr_CannotDoInCurrentContext, kAudioUnitErr_InvalidPropertyValue, CFData,
    CFDictionary, CFMutableDictionary, CFNumber, CFRetained, CFString, CFType, OSStatus, NO_ERR,
};
use super::wrapper::Wrapper;
use super::{au_type_of, AuPlugin};
use crate::wrapper::state;

/// The largest state we will read, as CLAP's loader allows.
const MAX_STATE_BYTES: usize = 256 * 1024 * 1024;

/// What the AU host may store the state as, from the plugin's own codes.
struct Codes {
    au_type: i32,
    subtype: i32,
    manufacturer: i32,
}

fn codes<P: AuPlugin>() -> Codes {
    Codes {
        au_type: four_cc(au_type_of::<P>().four_cc()) as i32,
        subtype: four_cc(P::AU_SUBTYPE) as i32,
        manufacturer: four_cc(P::AU_MANUFACTURER) as i32,
    }
}

/// Build the ClassInfo dictionary. The caller owns the returned reference.
pub(crate) fn get_class_info<P: AuPlugin>(
    wrapper: &Wrapper<P>,
) -> Result<CFRetained<CFMutableDictionary<CFString, CFType>>, OSStatus> {
    let serialized = unsafe {
        state::serialize_json::<P>(
            wrapper.params.clone(),
            state::make_params_iter(&wrapper.param_by_hash, &wrapper.param_id_to_hash),
        )
    };
    let serialized = match serialized {
        Ok(serialized) => serialized,
        Err(err) => {
            crate::nice_error!("AU: could not save state: {:#}", err);
            return Err(kAudioUnitErr_CannotDoInCurrentContext);
        }
    };

    let codes = codes::<P>();
    let dictionary = CFMutableDictionary::<CFString, CFType>::empty();
    let set_number = |key: &'static str, value: i32| {
        let key = CFString::from_static_str(key);
        let value = CFNumber::new_i32(value);
        dictionary.set(&key, &value);
    };
    set_number("version", 0);
    set_number("type", codes.au_type);
    set_number("subtype", codes.subtype);
    set_number("manufacturer", codes.manufacturer);

    let name = CFString::from_str(&wrapper.present_preset_name.lock());
    dictionary.set(&CFString::from_static_str("name"), &name);

    let data = CFData::from_bytes(&serialized);
    dictionary.set(&CFString::from_static_str("data"), &data);

    crate::nice_trace!("AU: saved state ({} bytes)", serialized.len());

    Ok(dictionary)
}

/// Look up `key` in the dictionary and downcast the value to `T`.
fn entry<T: objc2_core_foundation::ConcreteType + 'static>(
    dictionary: &CFDictionary<CFString, CFType>,
    key: &'static str,
) -> Option<CFRetained<T>> {
    dictionary
        .get(&CFString::from_static_str(key))
        .and_then(|value| value.downcast::<T>().ok())
}

/// Whether a code stored in the dictionary is absent (tolerated) or ours.
fn code_matches(dictionary: &CFDictionary<CFString, CFType>, key: &'static str, ours: i32) -> bool {
    match entry::<CFNumber>(dictionary, key) {
        Some(number) => match number.as_i32() {
            Some(code) => code == ours,
            // Present but not an integer: not something we wrote
            None => false,
        },
        None => true,
    }
}

/// Apply a ClassInfo property list. `plist` is the `CFPropertyListRef` the host passed.
///
/// A project must always load, so state that cannot be parsed is logged and ignored. A dictionary
/// for another plugin is refused.
///
/// # Safety
///
/// `plist` must be null or point to a live CoreFoundation object.
pub(crate) unsafe fn set_class_info<P: AuPlugin>(
    wrapper: &Wrapper<P>,
    plist: *const c_void,
) -> OSStatus {
    let Some(plist) = NonNull::new(plist as *mut CFType) else {
        return kAudioUnitErr_InvalidPropertyValue;
    };

    // SAFETY: The caller guarantees `plist` is a live CF object. Retaining it first means the
    // downcast below owns a reference of its own.
    let object: CFRetained<CFType> = unsafe { CFRetained::retain(plist) };
    let Ok(dictionary) = object.downcast::<CFDictionary>() else {
        return kAudioUnitErr_InvalidPropertyValue;
    };
    // SAFETY: Property list dictionaries have string keys and CF values, and the dictionary
    // callbacks of every CFDictionary retain and release any CF type.
    let dictionary =
        unsafe { CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(dictionary) };

    let codes = codes::<P>();
    if !code_matches(&dictionary, "type", codes.au_type)
        || !code_matches(&dictionary, "subtype", codes.subtype)
        || !code_matches(&dictionary, "manufacturer", codes.manufacturer)
    {
        return kAudioUnitErr_InvalidPropertyValue;
    }

    if let Some(name) = entry::<CFString>(&dictionary, "name") {
        *wrapper.present_preset_name.lock() = name.to_string();
    }

    // JUCE saves through AUBase, which writes its own parameter blob under `data`, and adds
    // `jucePluginState` beside it. Our own state never has that key, so it decides which this is.
    if let Some(juce_state) = entry::<CFData>(&dictionary, "jucePluginState") {
        return foreign_state_hook(&juce_state.to_vec());
    }

    if let Some(data) = entry::<CFData>(&dictionary, "data") {
        if data.len() > MAX_STATE_BYTES {
            crate::nice_error!("AU: state is too large ({} bytes), ignoring it", data.len());
            return NO_ERR;
        }

        let bytes = data.to_vec();
        match unsafe { state::deserialize_json(&bytes) } {
            Some(mut plugin_state) => {
                if wrapper.set_state_inner(&mut plugin_state) {
                    crate::nice_trace!("AU: loaded state ({} bytes)", bytes.len());
                }
            }
            None => crate::nice_error!("AU: could not parse the saved state, ignoring it"),
        }
    }

    NO_ERR
}

/// The hook for state saved by another implementation of this plugin (JUCE writes
/// `jucePluginState`). The import itself arrives with PORT_PLAN item 12; until then the project
/// loads with the plugin's defaults.
pub(crate) fn foreign_state_hook(state: &[u8]) -> OSStatus {
    crate::nice_warn!(
        "AU: JUCE state found ({} bytes); the import hook arrives with PORT_PLAN item 12",
        state.len()
    );
    NO_ERR
}
