// DELTON FORK DELTA: au-2, AUv2 editor
//! The pure parts of the AUv2 editor's Objective-C plumbing: the names of the view classes, the
//! identity of the loaded image they are derived from, and the bundle directory the host is told
//! about. Objective-C class names are process-wide, so two builds of one plugin loaded into one
//! host must never end up with the same name. None of this touches Apple's frameworks, so it
//! builds and tests on every OS.

use std::path::{Path, PathBuf};

/// FNV-1a, 64 bit.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }

    hash
}

/// The identity of one loaded build of one plugin, for Objective-C class names (which are
/// process-wide).
///
/// Hashes, in this order: manufacturer (4 bytes), subtype (4 bytes), `version` bytes, one 0x00
/// byte, the 16 uuid bytes (16 zero bytes if `uuid` is `None`), one byte that is 1 if `uuid` is
/// `Some` and 0 if not, then `image_base` as 8 little-endian bytes.
#[cfg_attr(not(all(target_os = "macos", feature = "editor")), allow(dead_code))]
pub(crate) fn build_hash(
    manufacturer: [u8; 4],
    subtype: [u8; 4],
    version: &str,
    uuid: Option<[u8; 16]>,
    image_base: u64,
) -> u64 {
    let mut bytes = Vec::with_capacity(4 + 4 + version.len() + 1 + 16 + 1 + 8);
    bytes.extend_from_slice(&manufacturer);
    bytes.extend_from_slice(&subtype);
    bytes.extend_from_slice(version.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&uuid.unwrap_or([0; 16]));
    bytes.push(u8::from(uuid.is_some()));
    bytes.extend_from_slice(&image_base.to_le_bytes());

    fnv1a64(&bytes)
}

/// `<prefix>_<manufacturer><subtype>_<hash as 16 lowercase hex digits>`. Each code byte that is
/// ASCII alphanumeric is kept; any other byte becomes `_` followed by its two lowercase hex
/// digits.
#[cfg_attr(not(all(target_os = "macos", feature = "editor")), allow(dead_code))]
pub(crate) fn class_name(
    prefix: &str,
    manufacturer: [u8; 4],
    subtype: [u8; 4],
    hash: u64,
) -> String {
    let mut name = String::with_capacity(prefix.len() + 1 + 8 * 3 + 1 + 16);
    name.push_str(prefix);
    name.push('_');
    for &byte in manufacturer.iter().chain(subtype.iter()) {
        if byte.is_ascii_alphanumeric() {
            name.push(byte as char);
        } else {
            name.push_str(&format!("_{byte:02x}"));
        }
    }
    name.push_str(&format!("_{hash:016x}"));

    name
}

/// The LC_UUID of a 64-bit Mach-O image, from the bytes starting at its mach header.
#[cfg_attr(not(all(target_os = "macos", feature = "editor")), allow(dead_code))]
pub(crate) fn macho_uuid(image: &[u8]) -> Option<[u8; 16]> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    const HEADER_SIZE: usize = 32;

    let read_u32 = |offset: usize| -> Option<u32> {
        let end = offset.checked_add(4)?;
        let bytes = image.get(offset..end)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    };

    if image.len() < HEADER_SIZE || read_u32(0)? != MH_MAGIC_64 {
        return None;
    }
    let command_count = read_u32(16)?;

    let mut offset = HEADER_SIZE;
    for _ in 0..command_count {
        if offset.checked_add(8)? > image.len() {
            return None;
        }
        let command = read_u32(offset)?;
        let command_size = read_u32(offset + 4)? as usize;
        if command_size < 8 || offset.checked_add(command_size)? > image.len() {
            return None;
        }

        if command == LC_UUID {
            if command_size < 24 {
                return None;
            }
            return image[offset + 8..offset + 24].try_into().ok();
        }

        offset += command_size;
    }

    None
}

/// The bundle directory a host should be told about: the nearest ancestor of `executable` (the
/// image path) whose extension is exactly `component`; if there is none, `executable`'s parent
/// directory; if it has no parent, `executable` itself.
#[cfg_attr(not(all(target_os = "macos", feature = "editor")), allow(dead_code))]
pub(crate) fn bundle_dir_of(executable: &Path) -> PathBuf {
    for ancestor in executable.ancestors().skip(1) {
        if ancestor.extension().is_some_and(|ext| ext == "component") {
            return ancestor.to_path_buf();
        }
    }

    match executable.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => executable.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_known_values() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    fn sequence_uuid() -> [u8; 16] {
        std::array::from_fn(|i| i as u8)
    }

    #[test]
    fn build_hash_known_values() {
        let hash = |version: &str, uuid: Option<[u8; 16]>, base: u64| {
            build_hash(*b"Nplg", *b"Tvew", version, uuid, base)
        };

        assert_eq!(
            hash("1.0.0", Some(sequence_uuid()), 0x1_0000_0000),
            0x5a9bf7bb05a30709
        );
        assert_eq!(hash("1.0.0", None, 0x1_0000_0000), 0x7cdf08dce8a6a7aa);
        assert_eq!(
            hash("1.0.1", Some(sequence_uuid()), 0x1_0000_0000),
            0xa0ed0a7a786e3a32
        );
        assert_eq!(
            hash("1.0.0", Some(sequence_uuid()), 0x1_0000_4000),
            0x05fd152690ae5049
        );
    }

    #[test]
    fn class_name_plain_and_escaped() {
        assert_eq!(
            class_name("NiceAuv2View", *b"Dltn", *b"DrVb", 0x0123456789abcdef),
            "NiceAuv2View_DltnDrVb_0123456789abcdef"
        );
        // A space is 0x20, a dash is 0x2d and a dot is 0x2e
        assert_eq!(
            class_name("X", *b"Ab c", *b"a-1.", 0xff),
            "X_Ab_20ca_2d1_2e_00000000000000ff"
        );
    }

    /// A mach header with `command_count` commands, then the segment command, then the UUID.
    fn macho(magic: u32, command_count: u32) -> Vec<u8> {
        let mut image = Vec::new();
        image.extend_from_slice(&magic.to_le_bytes());
        image.extend_from_slice(&[0; 12]);
        image.extend_from_slice(&command_count.to_le_bytes());
        image.extend_from_slice(&96u32.to_le_bytes());
        image.extend_from_slice(&[0; 8]);
        assert_eq!(image.len(), 32);

        // LC_SEGMENT_64 stand-in
        image.extend_from_slice(&0x19u32.to_le_bytes());
        image.extend_from_slice(&72u32.to_le_bytes());
        image.extend_from_slice(&[0; 64]);

        // LC_UUID
        image.extend_from_slice(&0x1bu32.to_le_bytes());
        image.extend_from_slice(&24u32.to_le_bytes());
        image.extend(0x10u8..=0x1f);
        assert_eq!(image.len(), 32 + 72 + 24);

        image
    }

    #[test]
    fn macho_uuid_finds_the_uuid_command() {
        let expected: [u8; 16] = std::array::from_fn(|i| 0x10 + i as u8);
        assert_eq!(macho_uuid(&macho(0xfeed_facf, 2)), Some(expected));
    }

    #[test]
    fn macho_uuid_refuses_bad_images() {
        // 32-bit magic
        assert_eq!(macho_uuid(&macho(0xfeed_face, 2)), None);

        // Truncated by one byte
        let mut truncated = macho(0xfeed_facf, 2);
        truncated.pop();
        assert_eq!(macho_uuid(&truncated), None);

        // Only the segment command is walked
        assert_eq!(macho_uuid(&macho(0xfeed_facf, 1)), None);

        // A zero command size must not loop forever
        let mut zero_size = macho(0xfeed_facf, 2);
        zero_size[36..40].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(macho_uuid(&zero_size), None);

        // Too short for a header
        assert_eq!(macho_uuid(&[0xcf, 0xfa, 0xed, 0xfe]), None);
        assert_eq!(macho_uuid(&[]), None);
    }

    #[test]
    fn bundle_dir_of_cases() {
        assert_eq!(
            bundle_dir_of(Path::new(
                "/Library/Audio/Plug-Ins/Components/DrawVerb.component/Contents/MacOS/DrawVerb"
            )),
            Path::new("/Library/Audio/Plug-Ins/Components/DrawVerb.component")
        );
        assert_eq!(
            bundle_dir_of(Path::new("/tmp/target/debug/deps/auv2_view-0123")),
            Path::new("/tmp/target/debug/deps")
        );
        // The extension must be exactly `component`
        assert_eq!(
            bundle_dir_of(Path::new("/a/X.component.bak/Contents/MacOS/X")),
            Path::new("/a/X.component.bak/Contents/MacOS")
        );
        // The nearest one wins
        assert_eq!(
            bundle_dir_of(Path::new("/A.component/B.component/Contents/MacOS/B")),
            Path::new("/A.component/B.component")
        );
    }
}
