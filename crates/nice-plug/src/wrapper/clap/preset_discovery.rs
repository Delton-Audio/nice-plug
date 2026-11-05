//! CLAP preset discovery: a plugin describes where its presets live and how to read one file's
//! metadata, and the host's own preset browser lists them without ever creating the plugin.
// DELTON FORK DELTA: hpre-2, pmeta-1b

use clap_sys::factory::preset_discovery::{
    CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT, CLAP_PRESET_DISCOVERY_IS_FAVORITE,
    CLAP_PRESET_DISCOVERY_IS_USER_CONTENT, CLAP_PRESET_DISCOVERY_LOCATION_FILE,
    clap_preset_discovery_factory, clap_preset_discovery_filetype, clap_preset_discovery_indexer,
    clap_preset_discovery_location, clap_preset_discovery_location_kind,
    clap_preset_discovery_metadata_receiver, clap_preset_discovery_provider,
    clap_preset_discovery_provider_descriptor,
};
use clap_sys::timestamp::{CLAP_TIMESTAMP_UNKNOWN, clap_timestamp};
use clap_sys::universal_plugin_id::clap_universal_plugin_id;
use clap_sys::version::CLAP_VERSION;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

/// What a plugin tells the host about its presets, with no plugin instance: the files' folders and
/// how to read one file's metadata. Returned by
/// [`ClapPlugin::preset_discovery()`][super::ClapPlugin::preset_discovery()].
pub struct PresetDiscovery {
    /// The provider's ID, `"<clap id>.presets"`.
    pub provider_id: String,
    /// The provider's name, `"<plugin name> presets"`.
    pub name: String,
    /// The provider's vendor.
    pub vendor: String,
    /// The name of the preset file type, `"<plugin name> preset"`.
    pub filetype_name: String,
    /// The preset files' extensions, without the dot: the current one first, then any earlier ones
    /// whose files are still presets. One file type is declared for each.
    pub extensions: Vec<String>,
    /// Where the presets are. Called once in the provider's `init`; it may write (a backend syncs
    /// its factory presets to disk here).
    pub locations: fn() -> Vec<PresetLocation>,
    /// One file's metadata, read without creating the plugin. `Err` means the file is not a preset
    /// of this plugin; the text goes to the host as the error message.
    pub metadata: fn(&Path) -> Result<PresetMetadata, String>,
}

/// A folder of preset files the host should index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetLocation {
    /// What the host calls this folder.
    pub name: String,
    /// The folder.
    pub path: PathBuf,
    /// Who made what is in it.
    pub content: PresetContent,
}

/// Whose preset it is: the flags the host sorts and filters by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetContent {
    /// Shipped with the plugin (`CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT`).
    Factory,
    /// Made by the user (`CLAP_PRESET_DISCOVERY_IS_USER_CONTENT`).
    User,
}

impl PresetContent {
    /// The CLAP preset-discovery flag for this content.
    pub fn clap_flags(self) -> u32 {
        match self {
            PresetContent::Factory => CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT,
            PresetContent::User => CLAP_PRESET_DISCOVERY_IS_USER_CONTENT,
        }
    }
}

/// What the host's browser shows for one preset file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetMetadata {
    /// The preset's name.
    pub name: String,
    /// Who made it; one `add_creator` each.
    pub creators: Vec<String>,
    /// One `add_feature` each.
    pub tags: Vec<String>,
    /// Factory or user: the preset's flags.
    pub content: PresetContent,
    /// The preset's description; empty sends none (`set_description` is not called).
    pub description: String,
    /// Whether the user starred the preset: `CLAP_PRESET_DISCOVERY_IS_FAVORITE` is ORed into the
    /// flags.
    pub favorite: bool,
    /// When the preset was created, if the OS says. With `modified`, sent through
    /// `set_timestamps`.
    pub created: Option<SystemTime>,
    /// When the preset was last changed, if the OS says.
    pub modified: Option<SystemTime>,
}

impl PresetMetadata {
    /// The flags `set_flags` gets: the content flag, plus the favourite flag when starred.
    pub fn clap_flags(&self) -> u32 {
        let favorite = if self.favorite {
            CLAP_PRESET_DISCOVERY_IS_FAVORITE
        } else {
            0
        };
        self.content.clap_flags() | favorite
    }
}

/// A time as the seconds since the UNIX epoch CLAP wants: `CLAP_TIMESTAMP_UNKNOWN` (0) for `None`
/// and for a time before 1970.
fn clap_time(time: Option<SystemTime>) -> clap_timestamp {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(CLAP_TIMESTAMP_UNKNOWN, |since| since.as_secs())
}

/// A copy of `s` the host can keep reading: NUL-terminated, cut at the first NUL if it has one
/// (C strings cannot hold it).
pub(crate) fn c_string_truncated(s: &str) -> CString {
    let end = s.find('\0').unwrap_or(s.len());
    // No interior NUL is left in `s[..end]`.
    CString::new(&s.as_bytes()[..end]).unwrap_or_default()
}

/// One plugin's entry: its provider descriptor and the plugin's description of its presets.
struct ProviderEntry {
    clap_id: CString,
    // The descriptor points into the `CString`s below. They are heap data owned by this entry, which
    // lives in the factory's `Vec` for the rest of the process, so the pointers stay valid.
    id: CString,
    _name: CString,
    _vendor: CString,
    descriptor: clap_preset_discovery_provider_descriptor,
    discovery: PresetDiscovery,
}

/// The preset-discovery factory over the exported plugins that have presets.
///
/// `#[repr(C)]` with the vtable first, so the `*const clap_preset_discovery_factory` the host
/// passes back casts to the whole factory.
#[repr(C)]
pub struct PresetDiscoveryFactory {
    vtable: clap_preset_discovery_factory,
    providers: Vec<ProviderEntry>,
}

unsafe impl Send for PresetDiscoveryFactory {}
unsafe impl Sync for PresetDiscoveryFactory {}

impl PresetDiscoveryFactory {
    /// Build the factory from `(CLAP id, discovery)` pairs. The factory must then stay at one
    /// address, which the export macro does by putting it in a `static`.
    pub fn new(plugins: Vec<(&'static str, PresetDiscovery)>) -> Self {
        let providers = plugins
            .into_iter()
            .map(|(clap_id, discovery)| {
                let id = c_string_truncated(&discovery.provider_id);
                let name = c_string_truncated(&discovery.name);
                let vendor = c_string_truncated(&discovery.vendor);
                let descriptor = clap_preset_discovery_provider_descriptor {
                    clap_version: CLAP_VERSION,
                    id: id.as_ptr(),
                    name: name.as_ptr(),
                    vendor: vendor.as_ptr(),
                };
                ProviderEntry {
                    clap_id: c_string_truncated(clap_id),
                    id,
                    _name: name,
                    _vendor: vendor,
                    descriptor,
                    discovery,
                }
            })
            .collect::<Vec<_>>();

        Self {
            vtable: clap_preset_discovery_factory {
                count: Some(factory_count),
                get_descriptor: Some(factory_get_descriptor),
                create: Some(factory_create),
            },
            providers,
        }
    }

    /// How many plugins have presets. The export macro answers the factory ID only when this is not
    /// zero.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no plugin has presets.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// The pointer to hand the host, for `get_factory`. Valid as long as `self` does not move.
    pub fn as_clap_factory(&self) -> *const c_void {
        (self as *const Self).cast()
    }
}

unsafe extern "C" fn factory_count(factory: *const clap_preset_discovery_factory) -> u32 {
    if factory.is_null() {
        return 0;
    }
    let factory = unsafe { &*(factory as *const PresetDiscoveryFactory) };
    factory.providers.len() as u32
}

unsafe extern "C" fn factory_get_descriptor(
    factory: *const clap_preset_discovery_factory,
    index: u32,
) -> *const clap_preset_discovery_provider_descriptor {
    if factory.is_null() {
        return ptr::null();
    }
    let factory = unsafe { &*(factory as *const PresetDiscoveryFactory) };
    match factory.providers.get(index as usize) {
        Some(entry) => &entry.descriptor,
        None => ptr::null(),
    }
}

unsafe extern "C" fn factory_create(
    factory: *const clap_preset_discovery_factory,
    indexer: *const clap_preset_discovery_indexer,
    provider_id: *const c_char,
) -> *const clap_preset_discovery_provider {
    if factory.is_null() || indexer.is_null() || provider_id.is_null() {
        return ptr::null();
    }
    let factory = unsafe { &*(factory as *const PresetDiscoveryFactory) };
    let provider_id = unsafe { CStr::from_ptr(provider_id) };
    let Some(entry) = factory
        .providers
        .iter()
        .find(|entry| entry.id.as_c_str() == provider_id)
    else {
        return ptr::null();
    };

    let provider = Box::new(Provider {
        raw: clap_preset_discovery_provider {
            desc: &entry.descriptor,
            provider_data: ptr::null_mut(),
            init: Some(provider_init),
            destroy: Some(provider_destroy),
            get_metadata: Some(provider_get_metadata),
            get_extension: Some(provider_get_extension),
        },
        entry,
        indexer,
    });
    let provider = Box::into_raw(provider);
    // `provider_data` points back at the whole provider, so the vtable functions find the entry.
    unsafe { (*provider).raw.provider_data = provider.cast() };
    provider.cast()
}

/// One provider instance the host created. `#[repr(C)]` with the raw provider first.
#[repr(C)]
struct Provider {
    raw: clap_preset_discovery_provider,
    entry: *const ProviderEntry,
    indexer: *const clap_preset_discovery_indexer,
}

fn provider_of<'a>(provider: *const clap_preset_discovery_provider) -> Option<&'a Provider> {
    if provider.is_null() {
        return None;
    }
    let data = unsafe { (*provider).provider_data };
    if data.is_null() {
        return None;
    }
    Some(unsafe { &*(data as *const Provider) })
}

unsafe extern "C" fn provider_init(provider: *const clap_preset_discovery_provider) -> bool {
    let Some(provider) = provider_of(provider) else {
        return false;
    };
    // No panic may cross the C boundary: a panic in the plugin's `locations` is "init failed".
    catch_unwind(AssertUnwindSafe(|| {
        let entry = unsafe { &*provider.entry };
        let indexer = unsafe { &*provider.indexer };
        let locations = (entry.discovery.locations)();

        let filetype_name = c_string_truncated(&entry.discovery.filetype_name);
        let Some(declare_filetype) = indexer.declare_filetype else {
            return false;
        };
        for extension in &entry.discovery.extensions {
            let extension = c_string_truncated(extension);
            let filetype = clap_preset_discovery_filetype {
                name: filetype_name.as_ptr(),
                description: ptr::null(),
                file_extension: extension.as_ptr(),
            };
            if !unsafe { declare_filetype(indexer, &filetype) } {
                return false;
            }
        }

        let Some(declare_location) = indexer.declare_location else {
            return false;
        };
        for location in &locations {
            let name = c_string_truncated(&location.name);
            let path = c_string_truncated(&location.path.to_string_lossy());
            let declared = clap_preset_discovery_location {
                flags: location.content.clap_flags(),
                name: name.as_ptr(),
                kind: CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                location: path.as_ptr(),
            };
            if !unsafe { declare_location(indexer, &declared) } {
                return false;
            }
        }
        true
    }))
    .unwrap_or(false)
}

unsafe extern "C" fn provider_destroy(provider: *const clap_preset_discovery_provider) {
    if let Some(provider) = provider_of(provider) {
        drop(unsafe { Box::from_raw(provider as *const Provider as *mut Provider) });
    }
}

/// Tell the receiver a file is not a preset of this plugin.
fn report_error(receiver: &clap_preset_discovery_metadata_receiver, message: &str) {
    if let Some(on_error) = receiver.on_error {
        let message = c_string_truncated(message);
        unsafe { on_error(receiver, 0, message.as_ptr()) };
    }
}

unsafe extern "C" fn provider_get_metadata(
    provider: *const clap_preset_discovery_provider,
    location_kind: clap_preset_discovery_location_kind,
    location: *const c_char,
    receiver: *const clap_preset_discovery_metadata_receiver,
) -> bool {
    let Some(provider) = provider_of(provider) else {
        return false;
    };
    if receiver.is_null() {
        return false;
    }
    let receiver = unsafe { &*receiver };
    if location_kind != CLAP_PRESET_DISCOVERY_LOCATION_FILE || location.is_null() {
        report_error(receiver, "only file locations hold presets");
        return false;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let entry = unsafe { &*provider.entry };
        let path = PathBuf::from(
            unsafe { CStr::from_ptr(location) }
                .to_string_lossy()
                .as_ref(),
        );
        match (entry.discovery.metadata)(&path) {
            Err(message) => {
                report_error(receiver, &message);
                false
            }
            Ok(metadata) => {
                let (Some(begin_preset), Some(add_plugin_id), Some(set_flags)) = (
                    receiver.begin_preset,
                    receiver.add_plugin_id,
                    receiver.set_flags,
                ) else {
                    return false;
                };
                let name = c_string_truncated(&metadata.name);
                if !unsafe { begin_preset(receiver, name.as_ptr(), ptr::null()) } {
                    return false;
                }
                let abi = c"clap";
                let plugin_id = clap_universal_plugin_id {
                    abi: abi.as_ptr(),
                    id: entry.clap_id.as_ptr(),
                };
                unsafe { add_plugin_id(receiver, &plugin_id) };
                unsafe { set_flags(receiver, metadata.clap_flags()) };
                if let Some(add_creator) = receiver.add_creator {
                    for creator in &metadata.creators {
                        let creator = c_string_truncated(creator);
                        unsafe { add_creator(receiver, creator.as_ptr()) };
                    }
                }
                if !metadata.description.is_empty()
                    && let Some(set_description) = receiver.set_description
                {
                    let description = c_string_truncated(&metadata.description);
                    unsafe { set_description(receiver, description.as_ptr()) };
                }
                if (metadata.created.is_some() || metadata.modified.is_some())
                    && let Some(set_timestamps) = receiver.set_timestamps
                {
                    unsafe {
                        set_timestamps(
                            receiver,
                            clap_time(metadata.created),
                            clap_time(metadata.modified),
                        )
                    };
                }
                if let Some(add_feature) = receiver.add_feature {
                    for tag in &metadata.tags {
                        let tag = c_string_truncated(tag);
                        unsafe { add_feature(receiver, tag.as_ptr()) };
                    }
                }
                true
            }
        }
    }));
    match result {
        Ok(done) => done,
        Err(_) => {
            report_error(receiver, "the plugin failed while reading the preset");
            false
        }
    }
}

unsafe extern "C" fn provider_get_extension(
    _provider: *const clap_preset_discovery_provider,
    _extension_id: *const c_char,
) -> *const c_void {
    ptr::null()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::Duration;

    #[test]
    fn c_string_is_cut_at_the_first_nul() {
        assert_eq!(c_string_truncated("Deep Bass").to_bytes(), b"Deep Bass");
        assert_eq!(c_string_truncated("Deep\0Bass").to_bytes(), b"Deep");
        assert_eq!(c_string_truncated("\0Bass").to_bytes(), b"");
        assert_eq!(c_string_truncated("").to_bytes(), b"");
        // Non-ASCII survives byte for byte
        assert_eq!(
            c_string_truncated("Gr\u{fc}n").to_bytes(),
            "Gr\u{fc}n".as_bytes()
        );
    }

    #[test]
    fn content_flags_are_the_clap_bits() {
        assert_eq!(PresetContent::Factory.clap_flags(), 1);
        assert_eq!(PresetContent::User.clap_flags(), 2);
    }

    // A Rust-side indexer and receiver that record what the provider tells them, so the whole C
    // vtable path runs without a host.

    #[derive(Default)]
    struct Recorded {
        filetypes: Vec<(String, String)>,
        locations: Vec<(u32, String, u32, String)>,
        presets: Vec<String>,
        plugin_ids: Vec<(String, String)>,
        flags: Vec<u32>,
        creators: Vec<String>,
        descriptions: Vec<String>,
        timestamps: Vec<(u64, u64)>,
        features: Vec<String>,
        errors: Vec<String>,
    }

    fn text(s: *const c_char) -> String {
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }

    fn recorded<'a>(data: *mut c_void) -> &'a RefCell<Recorded> {
        unsafe { &*(data as *const RefCell<Recorded>) }
    }

    unsafe extern "C" fn declare_filetype(
        indexer: *const clap_preset_discovery_indexer,
        filetype: *const clap_preset_discovery_filetype,
    ) -> bool {
        let rec = recorded(unsafe { (*indexer).indexer_data });
        let filetype = unsafe { &*filetype };
        assert!(filetype.description.is_null());
        rec.borrow_mut()
            .filetypes
            .push((text(filetype.name), text(filetype.file_extension)));
        true
    }

    unsafe extern "C" fn declare_location(
        indexer: *const clap_preset_discovery_indexer,
        location: *const clap_preset_discovery_location,
    ) -> bool {
        let rec = recorded(unsafe { (*indexer).indexer_data });
        let location = unsafe { &*location };
        rec.borrow_mut().locations.push((
            location.flags,
            text(location.name),
            location.kind,
            text(location.location),
        ));
        true
    }

    unsafe extern "C" fn on_error(
        receiver: *const clap_preset_discovery_metadata_receiver,
        _os_error: i32,
        message: *const c_char,
    ) {
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .errors
            .push(text(message));
    }

    unsafe extern "C" fn begin_preset(
        receiver: *const clap_preset_discovery_metadata_receiver,
        name: *const c_char,
        load_key: *const c_char,
    ) -> bool {
        assert!(load_key.is_null());
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .presets
            .push(text(name));
        true
    }

    unsafe extern "C" fn add_plugin_id(
        receiver: *const clap_preset_discovery_metadata_receiver,
        id: *const clap_universal_plugin_id,
    ) {
        let id = unsafe { &*id };
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .plugin_ids
            .push((text(id.abi), text(id.id)));
    }

    unsafe extern "C" fn set_flags(
        receiver: *const clap_preset_discovery_metadata_receiver,
        flags: u32,
    ) {
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .flags
            .push(flags);
    }

    unsafe extern "C" fn add_creator(
        receiver: *const clap_preset_discovery_metadata_receiver,
        creator: *const c_char,
    ) {
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .creators
            .push(text(creator));
    }

    unsafe extern "C" fn set_description(
        receiver: *const clap_preset_discovery_metadata_receiver,
        description: *const c_char,
    ) {
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .descriptions
            .push(text(description));
    }

    unsafe extern "C" fn set_timestamps(
        receiver: *const clap_preset_discovery_metadata_receiver,
        creation_time: clap_timestamp,
        modification_time: clap_timestamp,
    ) {
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .timestamps
            .push((creation_time, modification_time));
    }

    unsafe extern "C" fn add_feature(
        receiver: *const clap_preset_discovery_metadata_receiver,
        feature: *const c_char,
    ) {
        recorded(unsafe { (*receiver).receiver_data })
            .borrow_mut()
            .features
            .push(text(feature));
    }

    fn test_locations() -> Vec<PresetLocation> {
        vec![
            PresetLocation {
                name: "Factory".to_owned(),
                path: PathBuf::from("/presets/factory"),
                content: PresetContent::Factory,
            },
            PresetLocation {
                name: "User".to_owned(),
                path: PathBuf::from("/presets/user"),
                content: PresetContent::User,
            },
        ]
    }

    fn test_metadata(path: &Path) -> Result<PresetMetadata, String> {
        if path.extension().is_some_and(|ext| ext == "good") {
            Ok(PresetMetadata {
                name: "Deep\0hidden".to_owned(),
                creators: vec!["Sam".to_owned()],
                tags: vec!["bass".to_owned(), "dark".to_owned()],
                content: PresetContent::User,
                description: "warm, round".to_owned(),
                favorite: true,
                created: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                modified: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_500)),
            })
        } else if path.extension().is_some_and(|ext| ext == "bare") {
            Ok(PresetMetadata {
                name: "Bare".to_owned(),
                creators: Vec::new(),
                tags: Vec::new(),
                content: PresetContent::Factory,
                description: String::new(),
                favorite: false,
                created: None,
                modified: None,
            })
        } else if path.extension().is_some_and(|ext| ext == "half") {
            // One time the OS did not say, one before 1970
            Ok(PresetMetadata {
                name: "Half".to_owned(),
                creators: Vec::new(),
                tags: Vec::new(),
                content: PresetContent::User,
                description: String::new(),
                favorite: false,
                created: None,
                modified: Some(UNIX_EPOCH - Duration::from_secs(5)),
            })
        } else if path.extension().is_some_and(|ext| ext == "panic") {
            panic!("a plugin bug");
        } else {
            Err("not a preset of this plugin".to_owned())
        }
    }

    fn test_factory() -> PresetDiscoveryFactory {
        PresetDiscoveryFactory::new(vec![(
            "audio.test.plugin",
            PresetDiscovery {
                provider_id: "audio.test.plugin.presets".to_owned(),
                name: "Test presets".to_owned(),
                vendor: "Test".to_owned(),
                filetype_name: "Test preset".to_owned(),
                extensions: vec!["good".to_owned(), "older".to_owned()],
                locations: test_locations,
                metadata: test_metadata,
            },
        )])
    }

    #[test]
    fn provider_declares_and_reports_through_the_c_vtable() {
        let factory = test_factory();
        let vtable = factory.as_clap_factory() as *const clap_preset_discovery_factory;
        let vtable = unsafe { &*vtable };
        assert_eq!(unsafe { vtable.count.unwrap()(vtable) }, 1);
        assert!(unsafe { vtable.get_descriptor.unwrap()(vtable, 1) }.is_null());
        let descriptor = unsafe { &*vtable.get_descriptor.unwrap()(vtable, 0) };
        assert_eq!(text(descriptor.id), "audio.test.plugin.presets");
        assert_eq!(text(descriptor.name), "Test presets");

        let record = RefCell::new(Recorded::default());
        let indexer = clap_preset_discovery_indexer {
            clap_version: CLAP_VERSION,
            name: c"indexer".as_ptr(),
            vendor: c"v".as_ptr(),
            url: c"u".as_ptr(),
            version: c"1".as_ptr(),
            indexer_data: &record as *const _ as *mut c_void,
            declare_filetype: Some(declare_filetype),
            declare_location: Some(declare_location),
            declare_soundpack: None,
            get_extension: None,
        };
        // An unknown provider id makes none
        assert!(unsafe { vtable.create.unwrap()(vtable, &indexer, c"nope".as_ptr()) }.is_null());
        let provider = unsafe {
            vtable.create.unwrap()(vtable, &indexer, c"audio.test.plugin.presets".as_ptr())
        };
        assert!(!provider.is_null());
        let provider_ref = unsafe { &*provider };

        assert!(unsafe { provider_ref.init.unwrap()(provider) });
        {
            let rec = record.borrow();
            assert_eq!(
                rec.filetypes,
                vec![
                    ("Test preset".to_owned(), "good".to_owned()),
                    ("Test preset".to_owned(), "older".to_owned())
                ]
            );
            assert_eq!(
                rec.locations,
                vec![
                    (
                        CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT,
                        "Factory".to_owned(),
                        CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                        "/presets/factory".to_owned()
                    ),
                    (
                        CLAP_PRESET_DISCOVERY_IS_USER_CONTENT,
                        "User".to_owned(),
                        CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                        "/presets/user".to_owned()
                    ),
                ]
            );
        }

        let receiver = clap_preset_discovery_metadata_receiver {
            receiver_data: &record as *const _ as *mut c_void,
            on_error: Some(on_error),
            begin_preset: Some(begin_preset),
            add_plugin_id: Some(add_plugin_id),
            set_soundpack_id: None,
            set_flags: Some(set_flags),
            add_creator: Some(add_creator),
            set_description: Some(set_description),
            set_timestamps: Some(set_timestamps),
            add_feature: Some(add_feature),
            add_extra_info: None,
        };
        let get_metadata = provider_ref.get_metadata.unwrap();

        // A preset: the name is cut at the NUL, the flags are the user flag
        assert!(unsafe {
            get_metadata(
                provider,
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                c"/presets/user/a.good".as_ptr(),
                &receiver,
            )
        });
        {
            let rec = record.borrow();
            assert_eq!(rec.presets, vec!["Deep".to_owned()]);
            assert_eq!(
                rec.plugin_ids,
                vec![("clap".to_owned(), "audio.test.plugin".to_owned())]
            );
            // The user content flag ORed with the favourite flag
            assert_eq!(
                rec.flags,
                vec![CLAP_PRESET_DISCOVERY_IS_USER_CONTENT | CLAP_PRESET_DISCOVERY_IS_FAVORITE]
            );
            assert_eq!(rec.descriptions, vec!["warm, round".to_owned()]);
            assert_eq!(rec.timestamps, vec![(1_700_000_000, 1_700_000_500)]);
            assert_eq!(rec.creators, vec!["Sam".to_owned()]);
            assert_eq!(rec.features, vec!["bass".to_owned(), "dark".to_owned()]);
            assert!(rec.errors.is_empty());
        }

        // No description, no favourite, no times: no description call and no timestamps call, and
        // the flags are the content flag alone
        assert!(unsafe {
            get_metadata(
                provider,
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                c"/presets/factory/a.bare".as_ptr(),
                &receiver,
            )
        });
        {
            let rec = record.borrow();
            assert_eq!(rec.presets.len(), 2);
            assert_eq!(rec.flags.len(), 2);
            assert_eq!(rec.flags[1], CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT);
            assert_eq!(
                rec.descriptions.len(),
                1,
                "an empty description is not sent"
            );
            assert_eq!(rec.timestamps.len(), 1, "no times, no set_timestamps call");
        }

        // One time unknown (0) and one before 1970 (0)
        assert!(unsafe {
            get_metadata(
                provider,
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                c"/presets/user/a.half".as_ptr(),
                &receiver,
            )
        });
        {
            let rec = record.borrow();
            assert_eq!(rec.presets.len(), 3);
            assert_eq!(rec.timestamps, vec![(1_700_000_000, 1_700_000_500), (0, 0)]);
            assert_eq!(rec.descriptions.len(), 1);
        }

        // Not this plugin's file: false and an error, no preset
        assert!(!unsafe {
            get_metadata(
                provider,
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                c"/presets/user/a.other".as_ptr(),
                &receiver,
            )
        });
        // A plugin panic never crosses the boundary
        assert!(!unsafe {
            get_metadata(
                provider,
                CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                c"/presets/user/a.panic".as_ptr(),
                &receiver,
            )
        });
        // A non-file location
        assert!(!unsafe {
            get_metadata(
                provider,
                clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN,
                ptr::null(),
                &receiver,
            )
        });
        {
            let rec = record.borrow();
            assert_eq!(rec.presets.len(), 3, "no error path begins a preset");
            assert_eq!(rec.errors.len(), 3);
            assert_eq!(rec.errors[0], "not a preset of this plugin");
        }

        assert!(
            unsafe { provider_ref.get_extension.unwrap()(provider, c"anything".as_ptr()) }
                .is_null()
        );
        unsafe { provider_ref.destroy.unwrap()(provider) };
    }
}
