// DELTON FORK DELTA: au-1, AUv2 wrapper
//! The AU-facing parameter table, built from `Params::param_map()`: IDs, ranges, kinds, flags and
//! clump (group) numbers. Pure: builds and tests on every OS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use nice_plug_core::params::internals::ParamPtr;
use nice_plug_core::params::ParamFlags;
use std::collections::HashMap;

use crate::wrapper::util::hash_param_id;

// AU flag bits (AudioUnitParameterOptions). On macOS a test checks each against the generated
// bindings.

/// `kAudioUnitParameterFlag_CFNameRelease`: never set by us (the name strings live as long as the
/// wrapper).
#[allow(dead_code)]
pub const FLAG_CF_NAME_RELEASE: u32 = 1 << 4;
/// `kAudioUnitParameterFlag_HasClump`.
pub const FLAG_HAS_CLUMP: u32 = 1 << 20;
/// `kAudioUnitParameterFlag_ValuesHaveStrings`.
pub const FLAG_VALUES_HAVE_STRINGS: u32 = 1 << 21;
/// `kAudioUnitParameterFlag_HasCFNameString`.
pub const FLAG_HAS_CF_NAME_STRING: u32 = 1 << 27;
/// `kAudioUnitParameterFlag_NonRealTime`.
pub const FLAG_NON_REAL_TIME: u32 = 1 << 24;
/// `kAudioUnitParameterFlag_IsReadable`.
pub const FLAG_IS_READABLE: u32 = 1 << 30;
/// `kAudioUnitParameterFlag_IsWritable`.
pub const FLAG_IS_WRITABLE: u32 = 1 << 31;

/// How a parameter's value is expressed to the host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuParamKind {
    /// A float parameter: the AU value is the normalized value, 0.0 to 1.0.
    Continuous,
    /// An enum parameter: the AU value is the step number, 0 to `steps`, and the host gets the
    /// whole list of value strings (a menu).
    Indexed { steps: u32 },
    // DELTON FORK DELTA: au-3, an integer is not a menu (one had 1048513 value strings)
    /// An integer parameter: the AU value is the step number, 0 to `steps`, and the host asks for
    /// one value's string at a time.
    Stepped { steps: u32 },
    /// A boolean parameter: 0.0 or 1.0.
    Boolean,
}

/// The AU-facing description of one parameter. Flag bits use the AU values above. The fields are
/// the table's whole contract; the macOS side reads most of them and tests read the rest.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct AuParamEntry {
    /// `hash_param_id(string_id)`.
    pub id: u32,
    pub string_id: String,
    /// `ParamPtr::name()`.
    pub name: String,
    /// `ParamPtr::unit()` trimmed. Only usable as a C-string unit name (may be empty).
    pub unit_label: String,
    pub kind: AuParamKind,
    /// 0.0 always.
    pub min: f32,
    /// Continuous 1.0; Indexed and Stepped steps as f32; Boolean 1.0.
    pub max: f32,
    /// `au_from_normalized(kind, default_normalized_value)`.
    pub default: f32,
    pub flags: u32,
    /// 1-based index into `clump_names`, `None` when the group is "".
    pub clump: Option<u32>,
    /// `ParamFlags::HIDDEN`.
    pub hidden: bool,
    /// `ParamFlags::BYPASS`.
    pub bypass: bool,
    pub ptr: ParamPtr,
}

/// Every parameter, as an AU host sees it.
pub struct AuParamTable {
    /// In `param_map` order.
    pub entries: Vec<AuParamEntry>,
    /// Distinct non-empty groups, first-seen order.
    pub clump_names: Vec<String>,
    by_id: HashMap<u32, usize>,
}

impl AuParamTable {
    /// # Safety
    ///
    /// The `ParamPtr`s must be live (they point into the plugin's `Arc<dyn Params>`).
    pub unsafe fn build(param_map: &[(String, ParamPtr, String)]) -> Self {
        let mut entries: Vec<AuParamEntry> = Vec::with_capacity(param_map.len());
        let mut clump_names: Vec<String> = Vec::new();
        let mut by_id: HashMap<u32, usize> = HashMap::with_capacity(param_map.len());

        for (string_id, ptr, group) in param_map {
            let (kind, name, unit_label, default_normalized, param_flags) = unsafe {
                let kind = match ptr {
                    ParamPtr::BoolParam(_) => AuParamKind::Boolean,
                    ParamPtr::IntParam(_) => AuParamKind::Stepped {
                        steps: ptr.step_count().unwrap_or(0) as u32,
                    },
                    ParamPtr::EnumParam(_) => AuParamKind::Indexed {
                        steps: ptr.step_count().unwrap_or(0) as u32,
                    },
                    ParamPtr::FloatParam(_) => AuParamKind::Continuous,
                };

                (
                    kind,
                    ptr.name().to_owned(),
                    ptr.unit().trim().to_owned(),
                    ptr.default_normalized_value(),
                    ptr.flags(),
                )
            };

            let clump = if group.is_empty() {
                None
            } else {
                let idx = match clump_names.iter().position(|name| name == group) {
                    Some(idx) => idx,
                    None => {
                        clump_names.push(group.clone());
                        clump_names.len() - 1
                    }
                };
                Some(idx as u32 + 1)
            };

            let mut flags = FLAG_IS_READABLE | FLAG_IS_WRITABLE | FLAG_HAS_CF_NAME_STRING;
            if matches!(kind, AuParamKind::Continuous | AuParamKind::Stepped { .. }) {
                flags |= FLAG_VALUES_HAVE_STRINGS;
            }
            if param_flags.contains(ParamFlags::NON_AUTOMATABLE) {
                flags |= FLAG_NON_REAL_TIME;
            }
            if clump.is_some() {
                flags |= FLAG_HAS_CLUMP;
            }

            let max = match kind {
                AuParamKind::Continuous | AuParamKind::Boolean => 1.0,
                AuParamKind::Indexed { steps } | AuParamKind::Stepped { steps } => steps as f32,
            };

            let id = hash_param_id(string_id);
            by_id.insert(id, entries.len());
            entries.push(AuParamEntry {
                id,
                string_id: string_id.clone(),
                name,
                unit_label,
                kind,
                min: 0.0,
                max,
                default: au_from_normalized(kind, default_normalized),
                flags,
                clump,
                hidden: param_flags.contains(ParamFlags::HIDDEN),
                bypass: param_flags.contains(ParamFlags::BYPASS),
                ptr: *ptr,
            });
        }

        Self {
            entries,
            clump_names,
            by_id,
        }
    }

    /// The position of a parameter in [`entries`][Self::entries].
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.by_id.get(&id).copied()
    }

    /// The entry with this ID.
    pub fn get(&self, id: u32) -> Option<&AuParamEntry> {
        self.index_of(id).map(|idx| &self.entries[idx])
    }

    /// IDs of every entry with hidden == false, in order.
    pub fn listed_ids(&self) -> Vec<u32> {
        self.entries
            .iter()
            .filter(|entry| !entry.hidden)
            .map(|entry| entry.id)
            .collect()
    }

    /// The first entry with bypass == true.
    pub fn bypass_id(&self) -> Option<u32> {
        self.entries
            .iter()
            .find(|entry| entry.bypass)
            .map(|entry| entry.id)
    }
}

/// AU value from a normalized value: Continuous -> n; Indexed -> (n * steps).round(); Boolean -> 1.0
/// if n >= 0.5 else 0.0.
pub fn au_from_normalized(kind: AuParamKind, normalized: f32) -> f32 {
    match kind {
        AuParamKind::Continuous => normalized,
        AuParamKind::Indexed { steps } | AuParamKind::Stepped { steps } => {
            (normalized * steps as f32).round()
        }
        AuParamKind::Boolean => {
            if normalized >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
    }
}

/// Normalized value from an AU value, clamped to [min, max] first: Continuous -> v; Indexed ->
/// v.round() / steps (steps 0 -> 0.0); Boolean -> 1.0 if v >= 0.5 else 0.0. NaN is treated as 0.0.
pub fn normalized_from_au(kind: AuParamKind, value: f32) -> f32 {
    let value = if value.is_nan() { 0.0 } else { value };
    match kind {
        AuParamKind::Continuous => value.clamp(0.0, 1.0),
        AuParamKind::Indexed { steps } | AuParamKind::Stepped { steps } => {
            if steps == 0 {
                0.0
            } else {
                value.clamp(0.0, steps as f32).round() / steps as f32
            }
        }
        AuParamKind::Boolean => {
            if value.clamp(0.0, 1.0) >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nice_plug_core::params::enums::Enum;
    use nice_plug_core::params::range::{FloatRange, IntRange};
    use nice_plug_core::params::{BoolParam, EnumParam, FloatParam, IntParam, Param};

    #[test]
    fn table_from_a_hand_built_param_map() {
        let gain = FloatParam::new("Gain", 1.0, FloatRange::Linear { min: 0.0, max: 2.0 });
        let mode = IntParam::new("Mode", 2, IntRange::Linear { min: 0, max: 3 });
        let on = BoolParam::new("On", true);
        let secret =
            FloatParam::new("Secret", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 }).hide();
        let byp = BoolParam::new("Bypass", false).make_bypass();
        let map = vec![
            ("gain".to_string(), gain.as_ptr(), String::new()),
            ("mode".to_string(), mode.as_ptr(), "Main".to_string()),
            ("on".to_string(), on.as_ptr(), "Main".to_string()),
            ("secret".to_string(), secret.as_ptr(), String::new()),
            ("byp".to_string(), byp.as_ptr(), String::new()),
        ];
        let table = unsafe { AuParamTable::build(&map) };

        // The ID is the JUCE String::hashCode of "gain", top bit clear
        let gain_id = hash_param_id("gain");
        assert_eq!(table.get(gain_id).unwrap().id, 3165055);

        let gain_entry = table.get(gain_id).unwrap();
        assert_eq!(gain_entry.kind, AuParamKind::Continuous);
        assert_eq!(gain_entry.min, 0.0);
        assert_eq!(gain_entry.max, 1.0);
        assert_eq!(gain_entry.default, 0.5);
        assert_eq!(
            gain_entry.flags,
            FLAG_IS_READABLE
                | FLAG_IS_WRITABLE
                | FLAG_HAS_CF_NAME_STRING
                | FLAG_VALUES_HAVE_STRINGS
        );
        assert_eq!(gain_entry.clump, None);
        assert_eq!(gain_entry.name, "Gain");

        let mode_entry = table.get(hash_param_id("mode")).unwrap();
        assert_eq!(mode_entry.kind, AuParamKind::Stepped { steps: 3 });
        assert_ne!(mode_entry.flags & FLAG_VALUES_HAVE_STRINGS, 0);
        assert_eq!(mode_entry.max, 3.0);
        assert_eq!(mode_entry.default, 2.0);
        assert_eq!(mode_entry.clump, Some(1));
        assert_ne!(mode_entry.flags & FLAG_HAS_CLUMP, 0);
        assert_eq!(table.clump_names, vec!["Main".to_string()]);

        let on_entry = table.get(hash_param_id("on")).unwrap();
        assert_eq!(on_entry.kind, AuParamKind::Boolean);
        assert_eq!(on_entry.max, 1.0);
        assert_eq!(on_entry.default, 1.0);
        assert_eq!(on_entry.clump, Some(1));

        let secret_id = hash_param_id("secret");
        let listed = table.listed_ids();
        assert_eq!(listed.len(), 4);
        assert!(!listed.contains(&secret_id));
        assert!(table.get(secret_id).is_some());
        assert!(table.get(secret_id).unwrap().hidden);

        assert_eq!(table.bypass_id(), Some(hash_param_id("byp")));
        assert_eq!(table.get(0xDEAD_BEEF), None);
    }

    #[derive(PartialEq)]
    enum Shape {
        Sine,
        Square,
        Saw,
    }

    impl Enum for Shape {
        fn variants() -> &'static [&'static str] {
            &["Sine", "Square", "Saw"]
        }

        fn ids() -> Option<&'static [&'static str]> {
            None
        }

        fn to_index(self) -> usize {
            self as usize
        }

        fn from_index(index: usize) -> Self {
            match index {
                1 => Shape::Square,
                2 => Shape::Saw,
                _ => Shape::Sine,
            }
        }
    }

    // An enum is a menu (the whole list of value strings); an integer is not, however wide
    #[test]
    fn enums_are_menus_and_integers_are_stepped() {
        let shape = EnumParam::new("Shape", Shape::Saw);
        let trials = IntParam::new(
            "Trials",
            49_088,
            IntRange::Linear {
                min: 64,
                max: 1_048_576,
            },
        );
        let map = vec![
            ("shape".to_string(), shape.as_ptr(), String::new()),
            ("trials".to_string(), trials.as_ptr(), String::new()),
        ];
        let table = unsafe { AuParamTable::build(&map) };

        let shape_entry = table.get(hash_param_id("shape")).unwrap();
        assert_eq!(shape_entry.kind, AuParamKind::Indexed { steps: 2 });
        assert_eq!(shape_entry.flags & FLAG_VALUES_HAVE_STRINGS, 0);
        assert_eq!((shape_entry.max, shape_entry.default), (2.0, 2.0));

        let trials_entry = table.get(hash_param_id("trials")).unwrap();
        assert_eq!(trials_entry.kind, AuParamKind::Stepped { steps: 1_048_512 });
        assert_ne!(trials_entry.flags & FLAG_VALUES_HAVE_STRINGS, 0);
        assert_eq!(trials_entry.max, 1_048_512.0);
        assert_eq!(trials_entry.default, 49_024.0);
    }

    #[test]
    fn clumps_number_distinct_groups_in_first_seen_order() {
        let a = FloatParam::new("A", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 });
        let b = FloatParam::new("B", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 });
        let c = FloatParam::new("C", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 });
        let map = vec![
            ("a".to_string(), a.as_ptr(), "Two".to_string()),
            ("b".to_string(), b.as_ptr(), "One".to_string()),
            ("c".to_string(), c.as_ptr(), "Two".to_string()),
        ];
        let table = unsafe { AuParamTable::build(&map) };
        assert_eq!(
            table.clump_names,
            vec!["Two".to_string(), "One".to_string()]
        );
        assert_eq!(table.entries[0].clump, Some(1));
        assert_eq!(table.entries[1].clump, Some(2));
        assert_eq!(table.entries[2].clump, Some(1));
    }

    #[test]
    fn non_automatable_params_are_not_real_time() {
        let slow = FloatParam::new("Slow", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 })
            .non_automatable();
        let map = vec![("slow".to_string(), slow.as_ptr(), String::new())];
        let table = unsafe { AuParamTable::build(&map) };
        assert_ne!(table.entries[0].flags & FLAG_NON_REAL_TIME, 0);
    }

    #[test]
    fn value_conversions() {
        let indexed = AuParamKind::Indexed { steps: 3 };
        // 1.5 rounds to 2.0: `f32::round` rounds half away from zero
        assert_eq!(au_from_normalized(indexed, 0.5), 2.0);
        assert_eq!(normalized_from_au(indexed, 2.0), 2.0 / 3.0);
        assert_eq!(normalized_from_au(indexed, 7.0), 1.0);
        assert_eq!(normalized_from_au(AuParamKind::Continuous, -0.5), 0.0);
        assert_eq!(normalized_from_au(AuParamKind::Continuous, f32::NAN), 0.0);
        assert_eq!(au_from_normalized(AuParamKind::Boolean, 0.49), 0.0);
        assert_eq!(au_from_normalized(AuParamKind::Boolean, 0.5), 1.0);
        assert_eq!(normalized_from_au(AuParamKind::Boolean, 0.5), 1.0);
        assert_eq!(normalized_from_au(AuParamKind::Boolean, 0.49), 0.0);
        assert_eq!(
            normalized_from_au(AuParamKind::Indexed { steps: 0 }, 0.0),
            0.0
        );
        assert_eq!(
            normalized_from_au(AuParamKind::Indexed { steps: 0 }, 5.0),
            0.0
        );
        assert_eq!(
            normalized_from_au(AuParamKind::Continuous, f32::INFINITY),
            1.0
        );
        let stepped = AuParamKind::Stepped { steps: 3 };
        assert_eq!(au_from_normalized(stepped, 0.5), 2.0);
        assert_eq!(normalized_from_au(stepped, 2.0), 2.0 / 3.0);
        assert_eq!(normalized_from_au(stepped, 7.0), 1.0);
    }

    // The flag values are the AU header's; check them against the generated bindings.
    #[cfg(target_os = "macos")]
    #[test]
    fn flags_match_the_generated_bindings() {
        use objc2_audio_toolbox::AudioUnitParameterOptions as Options;
        assert_eq!(FLAG_CF_NAME_RELEASE, Options::Flag_CFNameRelease.0);
        assert_eq!(FLAG_HAS_CLUMP, Options::Flag_HasClump.0);
        assert_eq!(FLAG_VALUES_HAVE_STRINGS, Options::Flag_ValuesHaveStrings.0);
        assert_eq!(FLAG_HAS_CF_NAME_STRING, Options::Flag_HasCFNameString.0);
        assert_eq!(FLAG_NON_REAL_TIME, Options::Flag_NonRealTime.0);
        assert_eq!(FLAG_IS_READABLE, Options::Flag_IsReadable.0);
        assert_eq!(FLAG_IS_WRITABLE, Options::Flag_IsWritable.0);
    }
}
