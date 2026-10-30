use anyhow::Context;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod symbols;
mod util;

/// Re-export for the main function.
pub use anyhow::Result;

fn build_usage_string(command_name: &str) -> String {
    format!(
        "Usage:
  {command_name} bundle <package> [--release]
  {command_name} bundle -p <package1> -p <package2> ... [--release]

  {command_name} bundle-universal <package> [--release]  (macOS only)
  {command_name} bundle-universal -p <package1> -p <package2> ... [--release]  (macOS only)

  All other 'cargo build' options are supported, including '--target' and '--profile'."
    )
}

/// Any additional configuration that might be useful for creating plugin bundles, stored as
/// `bundler.toml` alongside the workspace's main `Cargo.toml` file.
type BundlerConfig = HashMap<String, PackageConfig>;

#[derive(Debug, Clone, Deserialize)]
struct PackageConfig {
    name: Option<String>,
    // DELTON FORK DELTA: optional macOS `CFBundleIdentifier` and
    // `CFBundleShortVersionString`/`CFBundleVersion` per package.
    #[serde(default)]
    bundle_identifier: Option<String>,
    #[serde(default)]
    version: Option<String>,
    // DELTON FORK DELTA (au-3): the Audio Unit identity of a `.component` bundle.
    #[serde(default)]
    au_type: Option<String>,
    #[serde(default)]
    au_subtype: Option<String>,
    #[serde(default)]
    au_manufacturer: Option<String>,
    #[serde(default)]
    au_name: Option<String>,
    #[serde(default)]
    au_description: Option<String>,
    #[serde(default)]
    au_tags: Option<Vec<String>>,
}

// DELTON FORK DELTA: values a caller supplies for the macOS `Info.plist`. They take precedence over
// the `bundler.toml` fields, which take precedence over the defaults.
/// The macOS bundle identifier and version a caller wants written to `Info.plist`.
#[derive(Debug, Clone, Default)]
pub struct BundleInfo {
    pub identifier: Option<String>,
    pub version: Option<String>,
    // DELTON FORK DELTA (au-3)
    /// The Audio Unit identity. Required when the library exports `NiceAuv2Factory`.
    pub au: Option<AuComponentInfo>,
}

// DELTON FORK DELTA (au-3): the Audio Unit identity of a `.component` bundle.
/// The `AudioComponents` entry a `.component` bundle's `Info.plist` declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuComponentInfo {
    /// `aufx`, `aumf`, `aumu` or `aumi`.
    pub au_type: String,
    /// Four printable ASCII characters. Saved in every host project.
    pub subtype: String,
    /// Four printable ASCII characters, at least one of them not a lowercase letter.
    pub manufacturer: String,
    /// Shown by hosts. Apple's convention is "Vendor: Name".
    pub name: String,
    pub description: String,
    /// Apple's category tags (`Effects`, `Delay`, ...). Empty writes no `tags` key.
    pub tags: Vec<String>,
}

// DELTON FORK DELTA: the version the plist used to hardcode.
const DEFAULT_BUNDLE_VERSION: &str = "1.0.0";

// DELTON FORK DELTA: resolve the identifier and version: caller, then `bundler.toml`, then default.
fn resolve_bundle_identity(
    package: &str,
    info: &BundleInfo,
    config: Option<&PackageConfig>,
) -> (String, String) {
    let identifier = info
        .identifier
        .clone()
        .or_else(|| config.and_then(|c| c.bundle_identifier.clone()))
        .unwrap_or_else(|| format!("com.nice-plug.{package}"));
    let version = info
        .version
        .clone()
        .or_else(|| config.and_then(|c| c.version.clone()))
        .unwrap_or_else(|| DEFAULT_BUNDLE_VERSION.to_string());
    (identifier, version)
}

// DELTON FORK DELTA (au-3): moved here from the AU wrapper, where nothing else used it.
/// `major << 16 | minor << 8 | patch` from "major.minor.patch" (the AU component version).
/// Anything from the first '-' or '+' on is ignored. A missing part is 0. A part that does not
/// parse as u32 makes the whole result 0. major is clamped to 0xFFFF, minor and patch to 0xFF.
pub fn au_component_version(version: &str) -> u32 {
    let core = version.split(['-', '+']).next().unwrap_or("");

    let mut parts = [0u32; 3];
    let mut text_parts = core.split('.');
    for part in parts.iter_mut() {
        match text_parts.next() {
            Some(text) => match text.parse::<u32>() {
                Ok(number) => *part = number,
                Err(_) => return 0,
            },
            None => break,
        }
    }

    (parts[0].min(0xFFFF) << 16) | (parts[1].min(0xFF) << 8) | parts[2].min(0xFF)
}

// DELTON FORK DELTA (au-3): why an Audio Unit identity is not valid.
/// Why `info` is not a valid Audio Unit identity, or `None` when it is.
fn au_component_error(info: &AuComponentInfo) -> Option<String> {
    if !matches!(info.au_type.as_str(), "aufx" | "aumf" | "aumu" | "aumi") {
        return Some(format!(
            "AU type \"{}\" is not one of aufx, aumf, aumu, aumi",
            info.au_type
        ));
    }
    for (role, code) in [
        ("subtype", &info.subtype),
        ("manufacturer", &info.manufacturer),
    ] {
        if code.len() != 4 {
            return Some(format!(
                "AU {role} \"{code}\" must be exactly 4 characters (got {} bytes)",
                code.len()
            ));
        }
        if code.bytes().any(|byte| !(0x20..=0x7E).contains(&byte)) {
            return Some(format!("AU {role} \"{code}\" must be printable ASCII"));
        }
    }
    if info
        .manufacturer
        .bytes()
        .all(|byte| byte.is_ascii_lowercase())
    {
        return Some(format!(
            "AU manufacturer \"{}\" is all lowercase letters, which Apple reserves",
            info.manufacturer
        ));
    }
    None
}

// DELTON FORK DELTA (au-3): the AU identity for a bundle.
/// The AU identity for a bundle: the caller's, else `bundler.toml`'s, else none.
fn resolve_au_component(
    info: &BundleInfo,
    config: Option<&PackageConfig>,
    bundle_name: &str,
) -> Result<Option<AuComponentInfo>> {
    if let Some(au) = &info.au {
        return Ok(Some(au.clone()));
    }
    let Some(config) = config else {
        return Ok(None);
    };
    let any_set = config.au_type.is_some()
        || config.au_subtype.is_some()
        || config.au_manufacturer.is_some()
        || config.au_name.is_some()
        || config.au_description.is_some()
        || config.au_tags.is_some();
    if !any_set {
        return Ok(None);
    }
    let (Some(au_type), Some(subtype), Some(manufacturer)) = (
        config.au_type.clone(),
        config.au_subtype.clone(),
        config.au_manufacturer.clone(),
    ) else {
        anyhow::bail!(
            "bundler.toml: au_type, au_subtype and au_manufacturer go together (set all three or none)"
        );
    };
    let name = config
        .au_name
        .clone()
        .unwrap_or_else(|| bundle_name.to_string());
    let description = config
        .au_description
        .clone()
        .unwrap_or_else(|| name.clone());
    Ok(Some(AuComponentInfo {
        au_type,
        subtype,
        manufacturer,
        name,
        description,
        tags: config.au_tags.clone().unwrap_or_default(),
    }))
}

// DELTON FORK DELTA: the plist interpolates user text, so it must be XML-escaped.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The target we're generating a plugin for. This can be either the native target or a cross
/// compilation target, so to reduce redundancy when determining the correct bundle paths we'll use
/// an enum for this.
#[derive(Debug, Clone, Copy)]
pub enum CompilationTarget {
    Linux(Architecture),
    MacOS(Architecture),
    /// A special case for lipo'd `x86_64-apple-darwin` and `aarch64-apple-darwin` builds.
    MacOSUniversal,
    Windows(Architecture),
}

#[derive(Debug, Clone, Copy)]
pub enum Architecture {
    X86,
    X86_64,
    RISCV64,
    // There are also a ton of different 32-bit ARM architectures, we'll just pretend they don't
    // exist for now
    AArch64,
}

/// The type of a MacOS bundle.
#[derive(Debug, Clone, Copy)]
pub enum BundleType {
    Plugin,
    Binary,
}

/// The main xtask entry point function. See the readme for instructions on how to use this.
pub fn main() -> Result<()> {
    let args = std::env::args().skip(1);
    main_with_args("cargo xtask", args)
}

/// The main xtask entry point function, but with custom command line arguments. `args` should not
/// contain the command name, so you should always skip at least one argument from
/// `std::env::args()` before passing it to this function.
pub fn main_with_args(command_name: &str, args: impl IntoIterator<Item = String>) -> Result<()> {
    chdir_workspace_root()?;
    let cargo_metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path("./Cargo.toml")
        .exec()
        .context("Could not parse `cargo-metadata`")?;
    let target_dir = cargo_metadata.target_directory.as_std_path();

    let mut args = args.into_iter();
    let usage_string = build_usage_string(command_name);
    let command = args
        .next()
        .with_context(|| format!("Missing command name\n\n{usage_string}",))?;
    match command.as_str() {
        "bundle" => {
            // For convenience's sake we'll allow building multiple packages with `-p` just like
            // cargo build, but you can also build a single package without specifying `-p`. Since
            // multiple packages can be built in parallel if we pass all of these flags to a single
            // `cargo build` we'll first build all of these packages and only then bundle them.
            let (packages, other_args) = split_bundle_args(args, &usage_string)?;

            // As explained above, for efficiency's sake this is a two step process
            build(&packages, &other_args)?;

            bundle(target_dir, &packages[0], &other_args, false)?;
            for package in packages.into_iter().skip(1) {
                bundle(target_dir, &package, &other_args, false)?;
            }

            Ok(())
        }
        "bundle-universal" => {
            // The same as `--bundle`, but builds universal binaries for macOS Cargo will also error
            // out on duplicate `--target` options, but it seems like a good idea to preemptively
            // abort the bundling process if that happens
            let (packages, other_args) = split_bundle_args(args, &usage_string)?;

            for arg in &other_args {
                if arg == "--target" || arg.starts_with("--target=") {
                    anyhow::bail!(
                        "'{command_name} xtask bundle-universal' is incompatible with the '{arg}' \
                         option."
                    )
                }
            }

            // We can just use the regular build function here. There's sadly no way to build both
            // targets in parallel, so this will likely take twice as logn as a regular build.
            // TODO: Explicitly specifying the target even on the native target causes a rebuild in
            //       the target `target/<target_triple>` directory. This makes bundling much simpler
            //       because there's no conditional logic required based on the current platform,
            //       but it does waste some resources and requires a rebuild if the native target
            //       was already built.
            let mut x86_64_args = other_args.clone();
            x86_64_args.push(String::from("--target=x86_64-apple-darwin"));
            build(&packages, &x86_64_args)?;
            let mut aarch64_args = other_args.clone();
            aarch64_args.push(String::from("--target=aarch64-apple-darwin"));
            build(&packages, &aarch64_args)?;

            // This `true` indicates a universal build. This will cause the two sets of built
            // binaries to beq lipo'd together into universal binaries before bundling
            bundle(target_dir, &packages[0], &other_args, true)?;
            for package in packages.into_iter().skip(1) {
                bundle(target_dir, &package, &other_args, true)?;
            }

            Ok(())
        }
        // This is only meant to be used by the CI, since using awk for this can be a bit spotty on
        // macOS
        "known-packages" => list_known_packages(),
        _ => anyhow::bail!("Unknown command '{command}'\n\n{usage_string}"),
    }
}

/// Change the current directory into the Cargo workspace's root.
///
/// This is using a heuristic to find the workspace root. It considers all ancestor directories of
/// either `CARGO_MANIFEST_DIR` or the current directory, and finds the leftmost one containing a
/// `Cargo.toml` file.
pub fn chdir_workspace_root() -> Result<()> {
    // This is either the directory of the xtask binary when using `nice-plug-xtask` normally, or any
    // random project when using it through `cargo nice-plug`.
    let project_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .context(
            "'$CARGO_MANIFEST_DIR' was not set and the current working directory could not be \
             found",
        )?;

    let workspace_root = project_dir
        .ancestors()
        .filter(|dir| dir.join("Cargo.toml").exists())
        // The ancestors are ordered starting from `project_dir` going up to the filesystem root. So
        // this is the leftmost matching ancestor.
        .last()
        .with_context(|| {
            format!(
                "Could not find a 'Cargo.toml' file in '{}' or any of its parent directories",
                project_dir.display()
            )
        })?;

    std::env::set_current_dir(workspace_root)
        .context("Could not change to workspace root directory")
}

/// Build one or more packages using the provided `cargo build` arguments. This should be called
/// before calling [`bundle()`]. This requires the current working directory to have been set to
/// the workspace's root using [`chdir_workspace_root()`].
pub fn build(packages: &[String], args: &[String]) -> Result<()> {
    let package_args = packages.iter().flat_map(|package| ["-p", package]);

    let status = Command::new("cargo")
        .arg("build")
        .args(package_args)
        .args(args)
        .status()
        .with_context(|| format!("Could not call cargo to build {}", packages.join(", ")))?;
    if !status.success() {
        anyhow::bail!("Could not build {}", packages.join(", "));
    } else {
        Ok(())
    }
}

/// Bundle a package that was previously built by a call to [`build()`] using the provided `cargo
/// build` arguments. These two functions are split up because building can be done in parallel by
/// Cargo itself while bundling is sequential. Options from the `bundler.toml` file in the
/// workspace's root are respected (see
/// <https://codeberg.org/BillyDM/nice-plug/src/branch/main/bundler-example.toml>). This requires the
/// current working directory to have been set to the workspace's root using [`chdir_workspace_root()`].
///
/// If the package also exposes a binary target in addition to a library (or just a binary, in case
/// the binary target has a different name) then this will also be copied into the `bundled`
/// directory.
///
/// Normally this respects the `--target` option for cross compilation. If the `universal` option is
/// specified instead, then this will assume both `x86_64-apple-darwin` and `aarch64-apple-darwin`
/// have been built and it will try to lipo those together instead.
pub fn bundle(target_dir: &Path, package: &str, args: &[String], universal: bool) -> Result<()> {
    bundle_with_info(target_dir, package, args, universal, &BundleInfo::default())
}

// DELTON FORK DELTA: like [`bundle()`], but the caller supplies the macOS bundle identifier and
// version (see [`BundleInfo`]).
pub fn bundle_with_info(
    target_dir: &Path,
    package: &str,
    args: &[String],
    universal: bool,
    info: &BundleInfo,
) -> Result<()> {
    let mut build_type_dir = "debug";
    let mut cross_compile_target: Option<String> = None;
    for arg_idx in (0..args.len()).rev() {
        let arg = &args[arg_idx];
        match arg.as_str() {
            "--profile" => {
                // Since Rust 1.57 you can have custom profiles
                build_type_dir = args.get(arg_idx + 1).context("Missing profile name")?;
            }
            "--release" => build_type_dir = "release",
            "--target" => {
                // When cross compiling we should generate the correct bundle type
                cross_compile_target = Some(
                    args.get(arg_idx + 1)
                        .context("Missing cross-compile target")?
                        .to_owned(),
                );
            }
            arg if arg.starts_with("--profile=") => {
                build_type_dir = arg
                    .strip_prefix("--profile=")
                    .context("Missing profile name")?;
            }
            arg if arg.starts_with("--target=") => {
                cross_compile_target = Some(
                    arg.strip_prefix("--target=")
                        .context("Missing cross-compile target")?
                        .to_owned(),
                );
            }
            _ => (),
        }
    }

    // We can bundle both library targets (for plugins) and binary targets (for standalone
    // applications)
    if universal {
        let x86_64_target_base =
            target_base(target_dir, Some("x86_64-apple-darwin"))?.join(build_type_dir);
        let x86_64_bin_path = x86_64_target_base.join(binary_basename(
            package,
            CompilationTarget::MacOS(Architecture::X86_64),
        ));
        let x86_64_lib_path = x86_64_target_base.join(library_basename(
            package,
            CompilationTarget::MacOS(Architecture::X86_64),
        ));

        let aarch64_target_base =
            target_base(target_dir, Some("aarch64-apple-darwin"))?.join(build_type_dir);
        let aarch64_bin_path = aarch64_target_base.join(binary_basename(
            package,
            CompilationTarget::MacOS(Architecture::AArch64),
        ));
        let aarch64_lib_path = aarch64_target_base.join(library_basename(
            package,
            CompilationTarget::MacOS(Architecture::AArch64),
        ));

        let build_bin = x86_64_bin_path.exists() && aarch64_bin_path.exists();
        let build_lib = x86_64_lib_path.exists() && aarch64_lib_path.exists();
        if !build_bin && !build_lib {
            anyhow::bail!("Could not find built libraries for universal build.");
        }

        eprintln!();
        if build_bin {
            bundle_binary(
                target_dir,
                package,
                &[&x86_64_bin_path, &aarch64_bin_path],
                CompilationTarget::MacOSUniversal,
                info,
            )?;
        }
        if build_lib {
            bundle_plugin(
                target_dir,
                package,
                &[&x86_64_lib_path, &aarch64_lib_path],
                CompilationTarget::MacOSUniversal,
                info,
            )?;
        }
    } else {
        let compilation_target = compilation_target(cross_compile_target.as_deref())?;
        let target_base =
            target_base(target_dir, cross_compile_target.as_deref())?.join(build_type_dir);
        let bin_path = target_base.join(binary_basename(package, compilation_target));
        let lib_path = target_base.join(library_basename(package, compilation_target));
        if !bin_path.exists() && !lib_path.exists() {
            anyhow::bail!(
                r#"Could not find a built library at '{}'.

Hint: Maybe you forgot to add:

[lib]
crate-type = ["cdylib"]

to your Cargo.toml file?"#,
                lib_path.display()
            );
        }

        eprintln!();
        if bin_path.exists() {
            bundle_binary(target_dir, package, &[&bin_path], compilation_target, info)?;
        }
        if lib_path.exists() {
            bundle_plugin(target_dir, package, &[&lib_path], compilation_target, info)?;
        }
    }

    Ok(())
}

/// Bundle a standalone target. If `bin_path` contains more than one path, then the binaries will be
/// combined into a single binary using a method that depends on the compilation target. For
/// universal macOS builds this uses lipo.
fn bundle_binary(
    target_dir: &Path,
    package: &str,
    bin_paths: &[&Path],
    compilation_target: CompilationTarget,
    info: &BundleInfo,
) -> Result<()> {
    let bundle_home_dir = bundle_home(target_dir);
    let package_config = load_bundler_config()?.and_then(|c| c.get(package).cloned());
    let bundle_name = match package_config.as_ref().and_then(|c| c.name.clone()) {
        Some(name) => name,
        None => package.to_string(),
    };
    let (bundle_identifier, bundle_version) =
        resolve_bundle_identity(package, info, package_config.as_ref());

    // On MacOS the standalone target needs to be in a bundle
    let standalone_bundle_binary_name =
        standalone_bundle_binary_name(&bundle_name, compilation_target);
    let standalone_binary_path = bundle_home_dir.join(&standalone_bundle_binary_name);

    fs::create_dir_all(standalone_binary_path.parent().unwrap())
        .context("Could not create standalone bundle directory")?;
    util::reflink_or_combine(bin_paths, &standalone_binary_path, compilation_target)
        .context("Could not create standalone bundle")?;

    // FIXME: The reflink crate seems to sometime strip away the executable bit, so we need to help
    //        it a little here
    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(&standalone_binary_path) {
        // These are the executable bits
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0b0001001001);

        fs::set_permissions(&standalone_binary_path, permissions).with_context(|| {
            format!(
                "Could not make '{}' executable",
                standalone_binary_path.display()
            )
        })?;
    }

    let standalone_bundle_home = bundle_home_dir.join(
        Path::new(&standalone_bundle_binary_name)
            .components()
            .next()
            .expect("Malformed standalone binary path"),
    );
    maybe_create_macos_bundle_metadata_with_info(
        package,
        &bundle_name,
        &standalone_bundle_home,
        compilation_target,
        BundleType::Binary,
        &bundle_identifier,
        &bundle_version,
        None,
    )?;
    maybe_codesign(&standalone_bundle_home, compilation_target);

    eprintln!(
        "Created a standalone bundle at '{}'",
        standalone_bundle_home.display()
    );

    Ok(())
}

/// Bundle all plugin targets for a plugin library. If `lib_path` contains more than one path, then
/// the libraries will be combined into a single library using a method that depends on the
/// compilation target. For universal macOS builds this uses lipo.
fn bundle_plugin(
    target_dir: &Path,
    package: &str,
    lib_paths: &[&Path],
    compilation_target: CompilationTarget,
    info: &BundleInfo,
) -> Result<()> {
    let bundle_home_dir = bundle_home(target_dir);
    let package_config = load_bundler_config()?.and_then(|c| c.get(package).cloned());
    let bundle_name = match package_config.as_ref().and_then(|c| c.name.clone()) {
        Some(name) => name,
        None => package.to_string(),
    };
    let (bundle_identifier, bundle_version) =
        resolve_bundle_identity(package, info, package_config.as_ref());

    // We'll detect the plugin formats supported by the plugin binary and create bundled accordingly.
    // If `lib_path` contains paths to multiple plugins that need to be combined into a macOS
    // universal binary, then we'll assume all of them export the same symbols and only check the
    // first one.
    let first_lib_path = lib_paths.first().context("Empty library paths slice")?;

    let bundle_clap = symbols::exported(first_lib_path, "clap_entry")
        .with_context(|| format!("Could not parse '{}'", first_lib_path.display()))?;
    // We'll ignore the platform-specific entry points for VST2 plugins since there's no reason to
    // create a new Rust VST2 plugin that doesn't work in modern DAWs
    // NOTE: nice-plug does not support VST2, but we'll support bundling VST2 plugins anyways because
    //       this bundler can also be used standalone.
    let bundle_vst2 = symbols::exported(first_lib_path, "VSTPluginMain")
        .with_context(|| format!("Could not parse '{}'", first_lib_path.display()))?;
    let bundle_vst3 = symbols::exported(first_lib_path, "GetPluginFactory")
        .with_context(|| format!("Could not parse '{}'", first_lib_path.display()))?;
    // DELTON FORK DELTA (au-3)
    let bundle_au = symbols::exported(first_lib_path, "NiceAuv2Factory")
        .with_context(|| format!("Could not parse '{}'", first_lib_path.display()))?;
    let bundled_plugin = bundle_clap || bundle_vst2 || bundle_vst3 || bundle_au;

    if bundle_clap {
        let clap_bundle_library_name = clap_bundle_library_name(&bundle_name, compilation_target);
        let clap_lib_path = bundle_home_dir.join(&clap_bundle_library_name);

        fs::create_dir_all(clap_lib_path.parent().unwrap())
            .context("Could not create CLAP bundle directory")?;
        util::reflink_or_combine(lib_paths, &clap_lib_path, compilation_target)
            .context("Could not create CLAP bundle")?;

        // In contrast to VST3, CLAP only uses bundles on macOS, so we'll just take the first
        // component of the library name instead
        let clap_bundle_home = bundle_home_dir.join(
            Path::new(&clap_bundle_library_name)
                .components()
                .next()
                .expect("Malformed CLAP library path"),
        );
        maybe_create_macos_bundle_metadata_with_info(
            package,
            &bundle_name,
            &clap_bundle_home,
            compilation_target,
            BundleType::Plugin,
            &bundle_identifier,
            &bundle_version,
            None,
        )?;
        maybe_codesign(&clap_bundle_home, compilation_target);

        eprintln!("Created a CLAP bundle at '{}'", clap_bundle_home.display());
    }
    if bundle_vst2 {
        let vst2_bundle_library_name = vst2_bundle_library_name(&bundle_name, compilation_target);
        let vst2_lib_path = bundle_home_dir.join(&vst2_bundle_library_name);

        fs::create_dir_all(vst2_lib_path.parent().unwrap())
            .context("Could not create VST2 bundle directory")?;
        util::reflink_or_combine(lib_paths, &vst2_lib_path, compilation_target)
            .context("Could not create VST2 bundle")?;

        // VST2 only uses bundles on macOS, so we'll just take the first component of the library
        // name instead
        let vst2_bundle_home = bundle_home_dir.join(
            Path::new(&vst2_bundle_library_name)
                .components()
                .next()
                .expect("Malformed VST2 library path"),
        );
        maybe_create_macos_bundle_metadata_with_info(
            package,
            &bundle_name,
            &vst2_bundle_home,
            compilation_target,
            BundleType::Plugin,
            &bundle_identifier,
            &bundle_version,
            None,
        )?;
        maybe_codesign(&vst2_bundle_home, compilation_target);

        eprintln!("Created a VST2 bundle at '{}'", vst2_bundle_home.display());
    }
    if bundle_vst3 {
        let vst3_lib_path =
            bundle_home_dir.join(vst3_bundle_library_name(&bundle_name, compilation_target));

        fs::create_dir_all(vst3_lib_path.parent().unwrap())
            .context("Could not create VST3 bundle directory")?;
        util::reflink_or_combine(lib_paths, &vst3_lib_path, compilation_target)
            .context("Could not create VST3 bundle")?;

        let vst3_bundle_home = vst3_lib_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        maybe_create_macos_bundle_metadata_with_info(
            package,
            &bundle_name,
            vst3_bundle_home,
            compilation_target,
            BundleType::Plugin,
            &bundle_identifier,
            &bundle_version,
            None,
        )?;
        maybe_codesign(vst3_bundle_home, compilation_target);

        eprintln!("Created a VST3 bundle at '{}'", vst3_bundle_home.display());
    }
    // DELTON FORK DELTA (au-3): an Audio Unit is a `.component` bundle, macOS only.
    if bundle_au {
        if !matches!(
            compilation_target,
            CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal
        ) {
            eprintln!(
                "WARNING: '{package}' exports NiceAuv2Factory, but Audio Units exist only on macOS; not creating an AU bundle"
            );
        } else {
            let Some(au) = resolve_au_component(info, package_config.as_ref(), &bundle_name)?
            else {
                anyhow::bail!(
                    "'{package}' exports NiceAuv2Factory, so it is an Audio Unit, but it has no AU identity: pass BundleInfo::au, or set au_type, au_subtype and au_manufacturer for it in bundler.toml"
                );
            };
            if let Some(message) = au_component_error(&au) {
                anyhow::bail!("'{package}': {message}");
            }

            let au_library_name = au_bundle_library_name(&bundle_name);
            let au_lib_path = bundle_home_dir.join(&au_library_name);

            fs::create_dir_all(au_lib_path.parent().unwrap())
                .context("Could not create AU bundle directory")?;
            util::reflink_or_combine(lib_paths, &au_lib_path, compilation_target)
                .context("Could not create AU bundle")?;

            let au_bundle_home = bundle_home_dir.join(
                Path::new(&au_library_name)
                    .components()
                    .next()
                    .expect("Malformed AU library path"),
            );
            maybe_create_macos_bundle_metadata_with_info(
                package,
                &bundle_name,
                &au_bundle_home,
                compilation_target,
                BundleType::Plugin,
                &bundle_identifier,
                &bundle_version,
                Some(&au),
            )?;
            maybe_codesign(&au_bundle_home, compilation_target);

            eprintln!("Created an AU bundle at '{}'", au_bundle_home.display());
        }
    }
    if !bundled_plugin {
        eprintln!("Not creating any plugin bundles because the package does not export any plugins")
    }

    Ok(())
}

/// This lists the packages configured in `bundler.toml`. This is only used as part of the CI when
/// bundling plugins.
pub fn list_known_packages() -> Result<()> {
    if let Some(config) = load_bundler_config()? {
        for package in config.keys() {
            println!("{package}");
        }
    }

    Ok(())
}

/// Load the `bundler.toml` file, if it exists. If it does exist but it cannot be parsed, then this
/// will return an error.
fn load_bundler_config() -> Result<Option<BundlerConfig>> {
    // We're already in the project root
    let bundler_config_path = Path::new("bundler.toml");
    if !bundler_config_path.exists() {
        return Ok(None);
    }

    let result = toml::from_str(
        &fs::read_to_string(bundler_config_path)
            .with_context(|| format!("Could not read '{}'", bundler_config_path.display()))?,
    )
    .with_context(|| format!("Could not parse '{}'", bundler_config_path.display()))?;

    Ok(Some(result))
}

/// Split the `xtask bundle` arguments into a list of packages and a list of other arguments. The
/// package vector either contains just the first argument, or if the arguments iterator starts with
/// one or more occurences of `-p <package>` then this will contain all those packages.
fn split_bundle_args(
    args: impl Iterator<Item = String>,
    usage_string: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut args = args.peekable();
    let mut packages = Vec::new();
    if args.peek().map(|s| s.as_str()) == Some("-p") {
        while args.peek().map(|s| s.as_str()) == Some("-p") {
            packages.push(
                args.nth(1)
                    .with_context(|| format!("Missing package name after -p\n\n{usage_string}"))?,
            );
        }
    } else {
        packages.push(
            args.next()
                .with_context(|| format!("Missing package name\n\n{usage_string}"))?,
        );
    };
    let other_args: Vec<_> = args.collect();

    Ok((packages, other_args))
}

/// The target we're compiling for. This is used to determine the paths and options for creating
/// plugin bundles.
fn compilation_target(cross_compile_target: Option<&str>) -> Result<CompilationTarget> {
    match cross_compile_target {
        Some("i686-unknown-linux-gnu") => Ok(CompilationTarget::Linux(Architecture::X86)),
        Some("i686-apple-darwin") => Ok(CompilationTarget::MacOS(Architecture::X86)),
        Some("i686-pc-windows-gnu") | Some("i686-pc-windows-msvc") => {
            Ok(CompilationTarget::Windows(Architecture::X86))
        }
        Some("x86_64-unknown-linux-gnu") => Ok(CompilationTarget::Linux(Architecture::X86_64)),
        Some("x86_64-apple-darwin") => Ok(CompilationTarget::MacOS(Architecture::X86_64)),
        Some("x86_64-pc-windows-gnu") | Some("x86_64-pc-windows-msvc") => {
            Ok(CompilationTarget::Windows(Architecture::X86_64))
        }
        Some("aarch64-unknown-linux-gnu") => Ok(CompilationTarget::Linux(Architecture::AArch64)),
        Some("aarch64-apple-darwin") => Ok(CompilationTarget::MacOS(Architecture::AArch64)),
        Some("aarch64-pc-windows-gnu") | Some("aarch64-pc-windows-msvc") => {
            Ok(CompilationTarget::Windows(Architecture::AArch64))
        }
        Some(target) => anyhow::bail!("Unhandled cross-compilation target: {}", target),
        None => {
            #[cfg(target_arch = "x86")]
            let architecture = Architecture::X86;
            #[cfg(target_arch = "x86_64")]
            let architecture = Architecture::X86_64;
            #[cfg(target_arch = "aarch64")]
            let architecture = Architecture::AArch64;
            #[cfg(target_arch = "riscv64")]
            let architecture = Architecture::RISCV64;

            #[cfg(all(target_family = "unix", not(target_os = "macos")))]
            return Ok(CompilationTarget::Linux(architecture));
            #[cfg(target_os = "macos")]
            return Ok(CompilationTarget::MacOS(architecture));
            #[cfg(target_os = "windows")]
            return Ok(CompilationTarget::Windows(architecture));
        }
    }
}

/// The directory bundled plugins should be written to.
fn bundle_home(target_directory: &Path) -> PathBuf {
    target_directory.join("bundled")
}

/// The base directory for the compiled binaries. This does not use [`CompilationTarget`] as we need
/// to be able to differentiate between native and cross-compilation.
fn target_base(target_directory: &Path, cross_compile_target: Option<&str>) -> Result<PathBuf> {
    match cross_compile_target {
        // Unhandled targets will already be handled in `compilation_target`
        Some(target) => Ok(target_directory.join(target)),
        None => Ok(target_directory.to_owned()),
    }
}

/// The file name of the compiled library for a binary crate.
fn binary_basename(package: &str, target: CompilationTarget) -> String {
    // Cargo will replace dashes with underscores
    let bin_name = package.replace('-', "_");

    match target {
        CompilationTarget::Linux(_)
        | CompilationTarget::MacOS(_)
        | CompilationTarget::MacOSUniversal => bin_name,
        CompilationTarget::Windows(_) => format!("{bin_name}.exe"),
    }
}

/// The file name of the compiled library for a `cdylib` crate.
fn library_basename(package: &str, target: CompilationTarget) -> String {
    // Cargo will replace dashes with underscores
    let lib_name = package.replace('-', "_");

    match target {
        CompilationTarget::Linux(_) => format!("lib{lib_name}.so"),
        CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal => {
            format!("lib{lib_name}.dylib")
        }
        CompilationTarget::Windows(_) => format!("{lib_name}.dll"),
    }
}

/// The filename of the binary target. On macOS this is part of a bundle.
fn standalone_bundle_binary_name(package: &str, target: CompilationTarget) -> String {
    match target {
        CompilationTarget::Linux(_) => package.to_owned(),
        CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal => {
            format!("{package}.app/Contents/MacOS/{package}")
        }
        CompilationTarget::Windows(_) => format!("{package}.exe"),
    }
}

/// The filename of the CLAP plugin for Linux and Windows, or the full path to the library file
/// inside of a CLAP bundle on macOS.
fn clap_bundle_library_name(package: &str, target: CompilationTarget) -> String {
    match target {
        CompilationTarget::Linux(_) | CompilationTarget::Windows(_) => format!("{package}.clap"),
        CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal => {
            format!("{package}.clap/Contents/MacOS/{package}")
        }
    }
}

/// On Linux and Windows VST2 plugins are regular library files, and on macOS they are put in a
/// bundle.
fn vst2_bundle_library_name(package: &str, target: CompilationTarget) -> String {
    match target {
        CompilationTarget::Linux(_) => format!("{package}.so"),
        CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal => {
            format!("{package}.vst/Contents/MacOS/{package}")
        }
        CompilationTarget::Windows(_) => format!("{package}.dll"),
    }
}

/// The full path to the library file inside of a VST3 bundle, including the leading `.vst3`
/// directory.
///
/// See <https://developer.steinberg.help/display/VST/Plug-in+Format+Structure>.
fn vst3_bundle_library_name(package: &str, target: CompilationTarget) -> String {
    match target {
        CompilationTarget::Linux(Architecture::X86) => {
            format!("{package}.vst3/Contents/i386-linux/{package}.so")
        }
        CompilationTarget::Linux(Architecture::X86_64) => {
            format!("{package}.vst3/Contents/x86_64-linux/{package}.so")
        }
        CompilationTarget::Linux(Architecture::RISCV64) => {
            format!("{package}.vst3/Contents/riscv64-linux/{package}.so")
        }
        CompilationTarget::Linux(Architecture::AArch64) => {
            format!("{package}.vst3/Contents/aarch64-linux/{package}.so")
        }
        CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal => {
            format!("{package}.vst3/Contents/MacOS/{package}")
        }
        CompilationTarget::Windows(Architecture::X86) => {
            format!("{package}.vst3/Contents/x86-win/{package}.vst3")
        }
        CompilationTarget::Windows(Architecture::X86_64) => {
            format!("{package}.vst3/Contents/x86_64-win/{package}.vst3")
        }
        CompilationTarget::Windows(Architecture::AArch64) => {
            format!("{package}.vst3/Contents/arm_64-win/{package}.vst3")
        }
        CompilationTarget::Windows(Architecture::RISCV64) => {
            panic!("riscv64 are not supported by windows currently!")
        }
    }
}

// DELTON FORK DELTA (au-3)
/// The full path to the library inside an Audio Unit bundle, including the leading `.component`
/// directory.
fn au_bundle_library_name(package: &str) -> String {
    format!("{package}.component/Contents/MacOS/{package}")
}

/// If compiling for macOS, create all of the bundl-y stuff Steinberg and Apple require you to have.
///
/// This still requires you to move the dylib file to `{bundle_home}/Contents/macOS/{package}`
/// yourself first. This uses the default identifier (`com.nice-plug.{package}`) and version
/// (`1.0.0`); see [`maybe_create_macos_bundle_metadata_with_info()`].
pub fn maybe_create_macos_bundle_metadata(
    package: &str,
    display_name: &str,
    bundle_home: &Path,
    target: CompilationTarget,
    bundle_type: BundleType,
) -> Result<()> {
    // DELTON FORK DELTA: delegates with the defaults.
    maybe_create_macos_bundle_metadata_with_info(
        package,
        display_name,
        bundle_home,
        target,
        bundle_type,
        &format!("com.nice-plug.{package}"),
        DEFAULT_BUNDLE_VERSION,
        None,
    )
}

// DELTON FORK DELTA: like [`maybe_create_macos_bundle_metadata()`] with an explicit bundle
// identifier and version.
// DELTON FORK DELTA (au-3): the Audio Unit identity is the eighth argument.
#[allow(clippy::too_many_arguments)]
pub fn maybe_create_macos_bundle_metadata_with_info(
    _package: &str,
    display_name: &str,
    bundle_home: &Path,
    target: CompilationTarget,
    bundle_type: BundleType,
    identifier: &str,
    version: &str,
    // DELTON FORK DELTA (au-3)
    au: Option<&AuComponentInfo>,
) -> Result<()> {
    if !matches!(
        target,
        CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal
    ) {
        return Ok(());
    }

    let package_type = match bundle_type {
        BundleType::Plugin => "BNDL",
        BundleType::Binary => "APPL",
    };

    // TODO: May want to add a bundler.toml field for the signature at some point.
    fs::write(
        bundle_home.join("Contents").join("PkgInfo"),
        format!("{package_type}????"),
    )
    .context("Could not create PkgInfo file")?;
    fs::write(
        bundle_home.join("Contents").join("Info.plist"),
        macos_info_plist(display_name, identifier, version, bundle_type, au),
    )
    .context("Could not create Info.plist file")?;

    Ok(())
}

// DELTON FORK DELTA: the `Info.plist` text as a pure function. Every interpolated value is
// XML-escaped.
/// The `Info.plist` text for a macOS bundle.
pub fn macos_info_plist(
    display_name: &str,
    identifier: &str,
    version: &str,
    bundle_type: BundleType,
    // DELTON FORK DELTA (au-3)
    au: Option<&AuComponentInfo>,
) -> String {
    let package_type = match bundle_type {
        BundleType::Plugin => "BNDL",
        BundleType::Binary => "APPL",
    };
    let au_block = au
        .map(|au| au_components_block(au, version))
        .unwrap_or_default();
    let display_name = xml_escape(display_name);
    let identifier = xml_escape(identifier);
    let version = xml_escape(version);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>

<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist>
  <dict>
    <key>CFBundleExecutable</key>
    <string>{display_name}</string>
    <key>CFBundleIconFile</key>
    <string></string>
    <key>CFBundleIdentifier</key>
    <string>{identifier}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundleDisplayName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>{package_type}</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
    <key>NSHumanReadableCopyright</key>
    <string></string>
    <key>NSHighResolutionCapable</key>
    <true/>
{au_block}  </dict>
</plist>
"#
    )
}

// DELTON FORK DELTA (au-3): the `AudioComponents` entry, spliced in before the closing `</dict>`.
fn au_components_block(au: &AuComponentInfo, version: &str) -> String {
    let tags = if au.tags.is_empty() {
        String::new()
    } else {
        let mut tags = String::from("        <key>tags</key>\n        <array>\n");
        for tag in &au.tags {
            tags.push_str(&format!("          <string>{}</string>\n", xml_escape(tag)));
        }
        tags.push_str("        </array>\n");
        tags
    };
    format!(
        r#"    <key>AudioComponents</key>
    <array>
      <dict>
        <key>type</key>
        <string>{au_type}</string>
        <key>subtype</key>
        <string>{subtype}</string>
        <key>manufacturer</key>
        <string>{manufacturer}</string>
        <key>name</key>
        <string>{name}</string>
        <key>description</key>
        <string>{description}</string>
        <key>version</key>
        <integer>{version_int}</integer>
        <key>factoryFunction</key>
        <string>NiceAuv2Factory</string>
        <key>sandboxSafe</key>
        <false/>
{tags}      </dict>
    </array>
"#,
        au_type = xml_escape(&au.au_type),
        subtype = xml_escape(&au.subtype),
        manufacturer = xml_escape(&au.manufacturer),
        name = xml_escape(&au.name),
        description = xml_escape(&au.description),
        version_int = au_component_version(version),
    )
}

/// If compiling for macOS, try to self-sign the bundle at the given path. This shouldn't be
/// necessary, but AArch64 macOS is stricter about these things and sometimes self built plugins may
/// not load otherwise. Presumably in combination with hardened runtimes.
///
/// If the codesigning command could not be run then this merely prints a warning.
pub fn maybe_codesign(bundle_home: &Path, target: CompilationTarget) {
    if !matches!(
        target,
        CompilationTarget::MacOS(_) | CompilationTarget::MacOSUniversal
    ) {
        return;
    }

    let success = Command::new("codesign")
        .arg("-f")
        .arg("-s")
        .arg("-")
        .arg(bundle_home)
        .status()
        .is_ok();
    if !success {
        eprintln!(
            "WARNING: Could not self-sign '{}', it may fail to run depending on the environment",
            bundle_home.display()
        )
    }
}

// DELTON FORK DELTA: tests for the plist text and the identity resolution.
#[cfg(test)]
mod tests {
    use super::*;

    // The text the plist had before the identifier and version became configurable.
    fn legacy_plist(package: &str, display_name: &str, package_type: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>

<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist>
  <dict>
    <key>CFBundleExecutable</key>
    <string>{display_name}</string>
    <key>CFBundleIconFile</key>
    <string></string>
    <key>CFBundleIdentifier</key>
    <string>com.nice-plug.{package}</string>
    <key>CFBundleName</key>
    <string>{display_name}</string>
    <key>CFBundleDisplayName</key>
    <string>{display_name}</string>
    <key>CFBundlePackageType</key>
    <string>{package_type}</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0.0</string>
    <key>CFBundleVersion</key>
    <string>1.0.0</string>
    <key>NSHumanReadableCopyright</key>
    <string></string>
    <key>NSHighResolutionCapable</key>
    <true/>
  </dict>
</plist>
"#
        )
    }

    #[test]
    fn defaults_reproduce_the_old_text() {
        let (id, ver) = resolve_bundle_identity("my_plug", &BundleInfo::default(), None);
        assert_eq!(
            macos_info_plist("My Plug", &id, &ver, BundleType::Plugin, None),
            legacy_plist("my_plug", "My Plug", "BNDL")
        );
        assert_eq!(
            macos_info_plist("My Plug", &id, &ver, BundleType::Binary, None),
            legacy_plist("my_plug", "My Plug", "APPL")
        );
    }

    #[test]
    fn supplied_values_appear() {
        let text = macos_info_plist("P", "com.acme.p", "2.3.4", BundleType::Plugin, None);
        assert!(text.contains("<string>com.acme.p</string>"));
        assert_eq!(text.matches("<string>2.3.4</string>").count(), 2);
        assert!(!text.contains("com.nice-plug"));
        assert!(!text.contains("1.0.0"));
    }

    #[test]
    fn values_are_escaped() {
        let text = macos_info_plist(
            "A & <B> \"C\" 'D'",
            "com.a&b<\"",
            "1",
            BundleType::Plugin,
            None,
        );
        assert!(text.contains("<string>A &amp; &lt;B&gt; &quot;C&quot; &apos;D&apos;</string>"));
        assert!(text.contains("<string>com.a&amp;b&lt;&quot;</string>"));
        assert!(!text.contains("A & "));
    }

    #[test]
    fn precedence_is_caller_then_toml_then_default() {
        let config = PackageConfig {
            name: None,
            bundle_identifier: Some("com.toml.id".into()),
            version: Some("3.0.0".into()),
            au_type: None,
            au_subtype: None,
            au_manufacturer: None,
            au_name: None,
            au_description: None,
            au_tags: None,
        };
        let caller = BundleInfo {
            identifier: Some("com.caller.id".into()),
            version: Some("4.0.0".into()),
            au: None,
        };
        assert_eq!(
            resolve_bundle_identity("p", &caller, Some(&config)),
            ("com.caller.id".to_string(), "4.0.0".to_string())
        );
        assert_eq!(
            resolve_bundle_identity("p", &BundleInfo::default(), Some(&config)),
            ("com.toml.id".to_string(), "3.0.0".to_string())
        );
        let partial = BundleInfo {
            identifier: Some("com.caller.id".into()),
            version: None,
            au: None,
        };
        assert_eq!(
            resolve_bundle_identity("p", &partial, Some(&config)),
            ("com.caller.id".to_string(), "3.0.0".to_string())
        );
        assert_eq!(
            resolve_bundle_identity("p", &BundleInfo::default(), None),
            ("com.nice-plug.p".to_string(), "1.0.0".to_string())
        );
    }

    #[test]
    fn au_component_version_packs_the_parts() {
        assert_eq!(au_component_version("1.2.3"), 0x0001_0203);
        assert_eq!(au_component_version("0.10.0"), 0x0000_0A00);
        assert_eq!(au_component_version("2"), 0x0002_0000);
        assert_eq!(au_component_version("1.2.3-beta.4"), 0x0001_0203);
        assert_eq!(au_component_version("1.2.3+build.9"), 0x0001_0203);
        assert_eq!(au_component_version("1.x.3"), 0);
        assert_eq!(au_component_version("1.300.2"), 0x0001_FF02);
        assert_eq!(au_component_version(""), 0);
        assert_eq!(au_component_version("70000.0.0"), 0xFFFF_0000);
    }

    // DELTON FORK DELTA (au-3): tests for the Audio Unit identity.
    fn au_fixture() -> AuComponentInfo {
        AuComponentInfo {
            au_type: "aufx".into(),
            subtype: "DlFt".into(),
            manufacturer: "Dltn".into(),
            name: "Delton Audio: Delay".into(),
            description: "A delay".into(),
            tags: vec!["Effects".into(), "Delay".into()],
        }
    }

    fn au_config() -> PackageConfig {
        PackageConfig {
            name: None,
            bundle_identifier: None,
            version: None,
            au_type: None,
            au_subtype: None,
            au_manufacturer: None,
            au_name: None,
            au_description: None,
            au_tags: None,
        }
    }

    #[test]
    fn au_plist_block_exact() {
        let text = macos_info_plist(
            "Delay",
            "com.d.delay",
            "1.2.3",
            BundleType::Plugin,
            Some(&au_fixture()),
        );
        let expected = r#"<?xml version="1.0" encoding="UTF-8"?>

<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist>
  <dict>
    <key>CFBundleExecutable</key>
    <string>Delay</string>
    <key>CFBundleIconFile</key>
    <string></string>
    <key>CFBundleIdentifier</key>
    <string>com.d.delay</string>
    <key>CFBundleName</key>
    <string>Delay</string>
    <key>CFBundleDisplayName</key>
    <string>Delay</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleShortVersionString</key>
    <string>1.2.3</string>
    <key>CFBundleVersion</key>
    <string>1.2.3</string>
    <key>NSHumanReadableCopyright</key>
    <string></string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>AudioComponents</key>
    <array>
      <dict>
        <key>type</key>
        <string>aufx</string>
        <key>subtype</key>
        <string>DlFt</string>
        <key>manufacturer</key>
        <string>Dltn</string>
        <key>name</key>
        <string>Delton Audio: Delay</string>
        <key>description</key>
        <string>A delay</string>
        <key>version</key>
        <integer>66051</integer>
        <key>factoryFunction</key>
        <string>NiceAuv2Factory</string>
        <key>sandboxSafe</key>
        <false/>
        <key>tags</key>
        <array>
          <string>Effects</string>
          <string>Delay</string>
        </array>
      </dict>
    </array>
  </dict>
</plist>
"#;
        assert_eq!(text, expected);
    }

    #[test]
    fn au_plist_without_tags() {
        let mut au = au_fixture();
        au.tags.clear();
        let text = macos_info_plist(
            "Delay",
            "com.d.delay",
            "1.2.3",
            BundleType::Plugin,
            Some(&au),
        );
        assert!(!text.contains("<key>tags</key>"));
        assert_eq!(text.matches("<string>NiceAuv2Factory</string>").count(), 1);
    }

    #[test]
    fn au_plist_escapes() {
        let mut au = au_fixture();
        au.name = "A&B: <C>".into();
        let text = macos_info_plist(
            "Delay",
            "com.d.delay",
            "1.2.3",
            BundleType::Plugin,
            Some(&au),
        );
        assert!(text.contains("<string>A&amp;B: &lt;C&gt;</string>"));
        assert!(!text.contains("A&B"));
    }

    #[test]
    fn au_component_errors() {
        let with = |au_type: &str, subtype: &str, manufacturer: &str| {
            let mut au = au_fixture();
            au.au_type = au_type.into();
            au.subtype = subtype.into();
            au.manufacturer = manufacturer.into();
            au_component_error(&au)
        };
        assert_eq!(with("aufx", "DlFt", "Dltn"), None);
        assert_eq!(
            with("aufz", "DlFt", "Dltn"),
            Some("AU type \"aufz\" is not one of aufx, aumf, aumu, aumi".to_string())
        );
        assert_eq!(
            with("aufx", "DlF", "Dltn"),
            Some("AU subtype \"DlF\" must be exactly 4 characters (got 3 bytes)".to_string())
        );
        assert_eq!(
            with("aufx", "Dl\u{7f}t", "Dltn"),
            Some("AU subtype \"Dl\u{7f}t\" must be printable ASCII".to_string())
        );
        assert_eq!(
            with("aufx", "DlFt", "dltn"),
            Some(
                "AU manufacturer \"dltn\" is all lowercase letters, which Apple reserves"
                    .to_string()
            )
        );
        assert_eq!(with("aufx", "DlFt", "dlt1"), None);
        assert_eq!(with("aufx", "DlFt", "DLTN"), None);
        assert_eq!(with("aufx", "DlFt", "d lt"), None);
        assert_eq!(
            with("aufx", "DlFt", "Dltnx"),
            Some(
                "AU manufacturer \"Dltnx\" must be exactly 4 characters (got 5 bytes)".to_string()
            )
        );
    }

    #[test]
    fn au_resolution() {
        // The caller's value wins over a complete bundler.toml entry.
        let full = PackageConfig {
            au_type: Some("aumu".into()),
            au_subtype: Some("Xxxx".into()),
            au_manufacturer: Some("Yyyy".into()),
            au_name: Some("From toml".into()),
            au_description: Some("From toml too".into()),
            au_tags: Some(vec!["Synth".into()]),
            ..au_config()
        };
        let caller = BundleInfo {
            au: Some(au_fixture()),
            ..BundleInfo::default()
        };
        assert_eq!(
            resolve_au_component(&caller, Some(&full), "Bn").unwrap(),
            Some(au_fixture())
        );

        // Some AU fields without all three codes is an error.
        let partial = PackageConfig {
            au_subtype: Some("Xxxx".into()),
            ..au_config()
        };
        let err = resolve_au_component(&BundleInfo::default(), Some(&partial), "Bn").unwrap_err();
        assert_eq!(
            err.to_string(),
            "bundler.toml: au_type, au_subtype and au_manufacturer go together (set all three or none)"
        );

        // The three codes alone: name and description fall back to the bundle name.
        let codes = PackageConfig {
            au_type: Some("aufx".into()),
            au_subtype: Some("Xxxx".into()),
            au_manufacturer: Some("Yyyy".into()),
            ..au_config()
        };
        let resolved = resolve_au_component(&BundleInfo::default(), Some(&codes), "Bn")
            .unwrap()
            .unwrap();
        assert_eq!(resolved.name, "Bn");
        assert_eq!(resolved.description, "Bn");
        assert!(resolved.tags.is_empty());
        assert_eq!(resolved.au_type, "aufx");
        assert_eq!(resolved.subtype, "Xxxx");
        assert_eq!(resolved.manufacturer, "Yyyy");

        // A name alone becomes the description too.
        let named = PackageConfig {
            au_name: Some("N".into()),
            au_description: None,
            ..codes.clone()
        };
        let resolved = resolve_au_component(&BundleInfo::default(), Some(&named), "Bn")
            .unwrap()
            .unwrap();
        assert_eq!(resolved.name, "N");
        assert_eq!(resolved.description, "N");

        // Neither.
        assert_eq!(
            resolve_au_component(&BundleInfo::default(), None, "Bn").unwrap(),
            None
        );
        assert_eq!(
            resolve_au_component(&BundleInfo::default(), Some(&au_config()), "Bn").unwrap(),
            None
        );
    }

    #[test]
    fn au_library_path() {
        assert_eq!(
            au_bundle_library_name("Delton Delay Fit"),
            "Delton Delay Fit.component/Contents/MacOS/Delton Delay Fit"
        );
    }
}
