mod layer_io;
mod local;

use crate::CONFIG_TOML_FILE;
use crate::ConfigLayerSource;
use crate::ProfileV2Name;
use crate::config_toml::ConfigToml;
use crate::diagnostics::config_error_from_toml;
use crate::diagnostics::first_layer_config_error_from_entries as typed_first_layer_config_error_from_entries;
use crate::diagnostics::io_error_from_config_error;
use crate::merge::merge_toml_values;
use crate::overrides::build_cli_overrides_layer;
use crate::project_root_markers::default_project_root_markers;
use crate::project_root_markers::project_root_markers_from_config;
use crate::shell_environment_policy::ShellEnvironmentPolicyFilterConfigToml;
use crate::state::ConfigLayerEntry;
use crate::state::ConfigLayerStack;
use crate::state::ConfigLoadOptions;
use crate::state::LoaderOverrides;
use crate::state::validate_enabled_config_layers;
use crate::strict_config::config_error_from_ignored_toml_value_fields;
use crate::strict_config::ignored_config_warning;
use crate::strict_config::ignored_toml_value_fields;
use crate::strict_config::unknown_feature_toml_value_field;
use crate::thread_config::ThreadConfigContext;
use crate::thread_config::ThreadConfigLoader;
use codex_file_system::ExecutorFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use codex_utils_path_uri::PathUri;
use dunce::canonicalize as normalize_path;
use std::io;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use toml::Value as TomlValue;

pub use local::LocalConfigLayers;
pub use local::LocalTomlLayer;
pub use local::LocalTomlLayerStack;
pub use local::load_local_config_layers;

#[cfg(unix)]
const SYSTEM_CONFIG_TOML_FILE_UNIX: &str = "/etc/codex/config.toml";

#[cfg(windows)]
const DEFAULT_PROGRAM_DATA_DIR_WINDOWS: &str = r"C:\ProgramData";

async fn first_layer_config_error_from_entries(
    layers: &[ConfigLayerEntry],
) -> Option<crate::diagnostics::ConfigError> {
    typed_first_layer_config_error_from_entries::<ConfigToml>(layers, CONFIG_TOML_FILE).await
}

/// Configuration is built up from multiple layers in the following order:
///
/// - package:  optional default configuration supplied with the Codex package
/// - system    `/etc/codex/config.toml` (Unix) or
///   `%ProgramData%\OpenAI\Codex\config.toml` (Windows)
/// - user      `${CODEX_HOME}/config.toml`
/// - profile   `${CODEX_HOME}/<name>.config.toml`, when selected
/// - tree      `./.codex/config.toml` in every directory from the project root down to `cwd`
/// - runtime   e.g., --config flags, model selector in UI
///
/// When loading the config stack for a thread, there should be a `cwd`
/// associated with it such that `cwd` should be `Some(...)`. Only for
/// thread-agnostic config loading should `cwd` be `None`.
pub async fn load_config_layers_state(
    fs: &dyn ExecutorFileSystem,
    codex_home: &Path,
    cwd: Option<AbsolutePathBuf>,
    cli_overrides: &[(String, TomlValue)],
    options: impl Into<ConfigLoadOptions>,
    thread_config_loader: &dyn ThreadConfigLoader,
) -> io::Result<ConfigLayerStack> {
    let ConfigLoadOptions {
        loader_overrides: overrides,
        strict_config,
    } = options.into();
    let packaged_defaults_layer = if let Some(file) = &overrides.packaged_defaults_path {
        let config = layer_io::read_config_from_path(
            fs,
            file,
            /*log_missing_as_info*/ false,
            strict_config,
        )
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("packaged defaults config file {} not found", file.display()),
            )
        })?;
        let base_dir = file.as_path().parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "packaged defaults config file {} has no parent directory",
                    file.display()
                ),
            )
        })?;
        ConfigLayerEntry::new(
            ConfigLayerSource::PackagedDefaults { file: file.clone() },
            resolve_relative_paths_in_config_toml(config, base_dir)?,
        )
    } else {
        let file = AbsolutePathBuf::from_absolute_path(std::env::current_exe()?)?;
        let raw_toml = include_str!("../../defaults.toml");
        let config = toml::from_str(raw_toml).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid embedded packaged defaults; this is a Codex build error: {error}"),
            )
        })?;
        ConfigLayerEntry::new_with_raw_toml(
            ConfigLayerSource::PackagedDefaults { file },
            config,
            raw_toml.to_owned(),
            AbsolutePathBuf::from_absolute_path(codex_home)?,
        )
    };
    let active_user_profile = overrides.user_config_profile.clone();
    let ignore_user_config = overrides.ignore_user_config;
    let ignore_project_config = overrides.ignore_project_config;

    let thread_config_context = ThreadConfigContext {
        thread_id: None,
        cwd: cwd.clone(),
    };
    let thread_config_layers = thread_config_loader
        .load_config_layers(thread_config_context)
        .await
        .map_err(io::Error::other)?;

    let mut layers = Vec::<ConfigLayerEntry>::new();
    layers.push(packaged_defaults_layer);

    let cli_overrides_layer = if cli_overrides.is_empty() {
        None
    } else {
        let cli_overrides_layer = build_cli_overrides_layer(cli_overrides);
        let base_dir = cwd
            .as_ref()
            .map(AbsolutePathBuf::as_path)
            .unwrap_or(codex_home);
        if strict_config {
            validate_cli_overrides_strictly(&cli_overrides_layer, base_dir)?;
        }
        Some(resolve_relative_paths_in_config_toml(
            cli_overrides_layer,
            base_dir,
        )?)
    };

    let system_config_toml_file = system_config_toml_file_with_overrides(&overrides)?;
    let system_layer = load_config_toml_for_required_layer(
        fs,
        &system_config_toml_file,
        strict_config,
        |config_toml| {
            ConfigLayerEntry::new(
                ConfigLayerSource::System {
                    file: system_config_toml_file.clone(),
                },
                config_toml,
            )
        },
    )
    .await?;
    layers.push(system_layer);

    // Add the base user config layer. When profile-v2 is selected, add the
    // profile config as a second user layer on top so the profile only needs to
    // contain overrides.
    let active_user_file = overrides.user_config_path(codex_home)?;
    let base_user_file = AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, codex_home);
    let base_user_layer = load_user_config_layer(
        fs,
        &base_user_file,
        /*profile*/ None,
        ignore_user_config,
        strict_config,
    )
    .await?;
    if let Some(active_user_profile) = active_user_profile.as_ref()
        && let Some(base_user_config) = base_user_layer.config.as_table()
    {
        let legacy_profile_is_selected = base_user_config
            .get("profile")
            .and_then(TomlValue::as_str)
            .is_some_and(|profile| profile == active_user_profile.as_str());
        let legacy_profile_table_exists = base_user_config
            .get("profiles")
            .and_then(TomlValue::as_table)
            .is_some_and(|profiles| profiles.contains_key(active_user_profile.as_str()));
        if legacy_profile_is_selected || legacy_profile_table_exists {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "--profile `{active_user_profile}` cannot be used while {} contains legacy `profile = \"{active_user_profile}\"` or `[profiles.{active_user_profile}]` config; move those settings into {} and remove the legacy profile selector/table. See https://developers.openai.com/codex/config-advanced#profiles for more information.",
                    base_user_file.as_path().display(),
                    active_user_file.as_path().display()
                ),
            ));
        }
    }
    layers.push(base_user_layer);

    if active_user_file != base_user_file {
        layers.push(
            load_user_config_layer(
                fs,
                &active_user_file,
                active_user_profile.as_ref(),
                ignore_user_config,
                strict_config,
            )
            .await?,
        );
    }

    let mut is_projectless = false;
    if !ignore_project_config && let Some(cwd) = cwd {
        let mut merged_so_far = TomlValue::Table(toml::map::Map::new());
        for layer in &layers {
            merge_toml_values(&mut merged_so_far, &layer.config);
        }
        if let Some(cli_overrides_layer) = cli_overrides_layer.as_ref() {
            merge_toml_values(&mut merged_so_far, cli_overrides_layer);
        }

        let project_root_markers = match project_root_markers_from_config(&merged_so_far) {
            Ok(markers) => markers.unwrap_or_else(default_project_root_markers),
            Err(err) => {
                if let Some(config_error) = first_layer_config_error_from_entries(&layers).await {
                    return Err(io_error_from_config_error(
                        io::ErrorKind::InvalidData,
                        config_error,
                        /*source*/ None,
                    ));
                }
                return Err(err);
            }
        };
        let discovered_root = discover_project_root(fs, &cwd, &project_root_markers).await?;
        let has_project_root_marker = discovered_root.is_some();
        let project_root = discovered_root.unwrap_or_else(|| cwd.clone());
        let checkout_root = find_git_checkout_root(fs, &cwd).await;
        let project_layers =
            load_project_layers(fs, &cwd, &project_root, codex_home, strict_config).await?;
        is_projectless =
            !has_project_root_marker && checkout_root.is_none() && project_layers.is_empty();
        layers.extend(project_layers);
    }

    // Add a layer for runtime overrides from the CLI or UI, if any exist.
    if let Some(cli_overrides_layer) = cli_overrides_layer {
        layers.push(ConfigLayerEntry::new(
            ConfigLayerSource::SessionFlags,
            cli_overrides_layer,
        ));
    }

    for thread_config_layer in thread_config_layers {
        insert_layer_by_precedence(&mut layers, thread_config_layer);
    }

    if let Err(err) = validate_enabled_config_layers(&layers) {
        if let Some(config_error) = typed_first_layer_config_error_from_entries::<
            ShellEnvironmentPolicyFilterConfigToml,
        >(&layers, CONFIG_TOML_FILE)
        .await
        {
            return Err(io_error_from_config_error(
                io::ErrorKind::InvalidData,
                config_error,
                /*source*/ None,
            ));
        }
        return Err(err);
    }

    let mut config_layer_stack = ConfigLayerStack::new(layers)?;
    config_layer_stack.is_projectless = is_projectless;
    let startup_warnings = ignored_config_warning(&config_layer_stack)
        .into_iter()
        .collect();
    Ok(config_layer_stack.with_startup_warnings(startup_warnings))
}

async fn load_user_config_layer(
    fs: &dyn ExecutorFileSystem,
    user_file: &AbsolutePathBuf,
    profile: Option<&ProfileV2Name>,
    ignore_user_config: bool,
    strict_config: bool,
) -> io::Result<ConfigLayerEntry> {
    let profile = profile.map(ToString::to_string);
    if ignore_user_config {
        return Ok(ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: user_file.clone(),
                profile,
            },
            TomlValue::Table(toml::map::Map::new()),
        ));
    }

    load_config_toml_for_required_layer(fs, user_file, strict_config, |config_toml| {
        ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: user_file.clone(),
                profile: profile.clone(),
            },
            config_toml,
        )
    })
    .await
}

fn insert_layer_by_precedence(layers: &mut Vec<ConfigLayerEntry>, layer: ConfigLayerEntry) {
    match layers
        .iter()
        .position(|existing| existing.name.precedence() > layer.name.precedence())
    {
        Some(index) => layers.insert(index, layer),
        None => layers.push(layer),
    }
}

/// Attempts to load a config.toml file from `config_toml`.
/// - If the file exists and is valid TOML, passes the parsed `toml::Value` to
///   `create_entry` and returns the resulting layer entry.
/// - If the file does not exist, uses an empty `Table` with `create_entry` and
///   returns the resulting layer entry.
/// - If there is an error reading the file or parsing the TOML, returns an
///   error.
async fn load_config_toml_for_required_layer(
    fs: &dyn ExecutorFileSystem,
    toml_file: &AbsolutePathBuf,
    strict_config: bool,
    create_entry: impl FnOnce(TomlValue) -> ConfigLayerEntry,
) -> io::Result<ConfigLayerEntry> {
    let loaded = load_config_toml_for_required_layer_raw(fs, toml_file, strict_config).await?;
    let toml_value = resolve_relative_paths_in_config_toml(loaded.toml, loaded.base_dir.as_path())?;

    Ok(create_entry(toml_value))
}

#[derive(Debug, Clone)]
struct LoadedTomlFile {
    toml: TomlValue,
    base_dir: AbsolutePathBuf,
}

async fn load_config_toml_for_required_layer_raw(
    fs: &dyn ExecutorFileSystem,
    toml_file: &AbsolutePathBuf,
    strict_config: bool,
) -> io::Result<LoadedTomlFile> {
    let config_parent = toml_file.as_path().parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Config file {} has no parent directory",
                toml_file.as_path().display()
            ),
        )
    })?;
    let base_dir = AbsolutePathBuf::from_absolute_path(config_parent)?;
    let toml_file_uri = PathUri::from_abs_path(toml_file);
    let toml_value = match fs.read_file_text(&toml_file_uri, Default::default()).await {
        Ok(contents) => {
            let config: TomlValue = toml::from_str(&contents).map_err(|err| {
                let config_error =
                    config_error_from_toml(toml_file.as_path(), &contents, err.clone());
                io_error_from_config_error(io::ErrorKind::InvalidData, config_error, Some(err))
            })?;
            if strict_config {
                validate_config_toml_strictly(
                    toml_file.as_path(),
                    &contents,
                    &config,
                    config_parent,
                )?;
            }
            Ok(config)
        }
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                Ok(TomlValue::Table(toml::map::Map::new()))
            } else {
                Err(io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to read config file {}: {e}",
                        toml_file.as_path().display()
                    ),
                ))
            }
        }
    }?;

    Ok(LoadedTomlFile {
        toml: toml_value,
        base_dir,
    })
}

fn validate_config_toml_strictly(
    toml_file: &Path,
    contents: &str,
    value: &TomlValue,
    base_dir: &Path,
) -> io::Result<()> {
    let _guard = AbsolutePathBufGuard::new(base_dir);
    if let Some(config_error) = config_error_from_ignored_toml_value_fields::<ConfigToml>(
        toml_file,
        contents,
        value.clone(),
    ) {
        Err(io_error_from_config_error(
            io::ErrorKind::InvalidData,
            config_error,
            /*source*/ None,
        ))
    } else {
        Ok(())
    }
}

fn validate_cli_overrides_strictly(
    cli_overrides_layer: &TomlValue,
    base_dir: &Path,
) -> io::Result<()> {
    let _guard = AbsolutePathBufGuard::new(base_dir);
    let ignored_path = ignored_toml_value_fields::<ConfigToml>(cli_overrides_layer.clone())
        .into_iter()
        .next()
        .map(|path| path.join("."))
        .or_else(|| unknown_feature_toml_value_field(cli_overrides_layer));
    if let Some(ignored_path) = ignored_path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown configuration field `{ignored_path}` in -c/--config override"),
        ));
    }

    Ok(())
}

#[cfg(unix)]
pub fn system_config_toml_file() -> io::Result<AbsolutePathBuf> {
    AbsolutePathBuf::from_absolute_path(Path::new(SYSTEM_CONFIG_TOML_FILE_UNIX))
}

#[cfg(windows)]
pub fn system_config_toml_file() -> io::Result<AbsolutePathBuf> {
    windows_system_config_toml_file()
}

fn system_config_toml_file_with_overrides(
    overrides: &LoaderOverrides,
) -> io::Result<AbsolutePathBuf> {
    match &overrides.system_config_path {
        Some(path) => AbsolutePathBuf::from_absolute_path(path),
        None => system_config_toml_file(),
    }
}

#[cfg(windows)]
fn windows_codex_system_dir() -> PathBuf {
    let program_data = windows_program_data_dir_from_known_folder().unwrap_or_else(|err| {
        tracing::warn!(
            error = %err,
            "Failed to resolve ProgramData known folder; using default path"
        );
        PathBuf::from(DEFAULT_PROGRAM_DATA_DIR_WINDOWS)
    });
    program_data.join("OpenAI").join("Codex")
}

#[cfg(windows)]
fn windows_system_config_toml_file() -> io::Result<AbsolutePathBuf> {
    let config_toml_file = windows_codex_system_dir().join("config.toml");
    AbsolutePathBuf::try_from(config_toml_file)
}

#[cfg(windows)]
fn windows_program_data_dir_from_known_folder() -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramData;
    use windows_sys::Win32::UI::Shell::KF_FLAG_DEFAULT;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;

    let mut path_ptr = std::ptr::null_mut::<u16>();
    let known_folder_flags = u32::try_from(KF_FLAG_DEFAULT).map_err(|_| {
        io::Error::other(format!(
            "KF_FLAG_DEFAULT did not fit in u32: {KF_FLAG_DEFAULT}"
        ))
    })?;
    // Known folder IDs reference:
    // https://learn.microsoft.com/en-us/windows/win32/shell/knownfolderid
    // SAFETY: SHGetKnownFolderPath initializes path_ptr with a CoTaskMem-allocated,
    // null-terminated UTF-16 string on success.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramData,
            known_folder_flags,
            std::ptr::null_mut(),
            &mut path_ptr,
        )
    };
    if hr != 0 {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath(FOLDERID_ProgramData) failed with HRESULT {hr:#010x}"
        )));
    }
    if path_ptr.is_null() {
        return Err(io::Error::other(
            "SHGetKnownFolderPath(FOLDERID_ProgramData) returned a null pointer",
        ));
    }

    // SAFETY: path_ptr is a valid null-terminated UTF-16 string allocated by
    // SHGetKnownFolderPath and must be freed with CoTaskMemFree.
    let path = unsafe {
        let mut len = 0usize;
        while *path_ptr.add(len) != 0 {
            len += 1;
        }
        let wide = std::slice::from_raw_parts(path_ptr, len);
        let path = PathBuf::from(OsString::from_wide(wide));
        CoTaskMemFree(path_ptr.cast());
        path
    };

    Ok(path)
}

fn project_layer_entry(
    dot_codex_folder: &AbsolutePathBuf,
    config: TomlValue,
    hooks_config_folder_override: Option<AbsolutePathBuf>,
) -> ConfigLayerEntry {
    let source = ConfigLayerSource::Project {
        dot_codex_folder: dot_codex_folder.clone(),
    };
    ConfigLayerEntry::new(source, config)
        .with_hooks_config_folder_override(hooks_config_folder_override)
}

/// Takes a `toml::Value` parsed from a config.toml file and walks through it,
/// resolving any `AbsolutePathBuf` fields against `base_dir`, returning a new
/// `toml::Value` with the same shape but with paths resolved.
///
/// This ensures that multiple config layers can be merged together correctly
/// even if they were loaded from different directories.
#[doc(hidden)]
pub fn resolve_relative_paths_in_config_toml(
    value_from_config_toml: TomlValue,
    base_dir: &Path,
) -> io::Result<TomlValue> {
    // Use the serialize/deserialize round-trip to convert the
    // `toml::Value` into a `ConfigToml` with `AbsolutePath
    let _guard = AbsolutePathBufGuard::new(base_dir);
    let Ok(resolved) = value_from_config_toml.clone().try_into::<ConfigToml>() else {
        return Ok(value_from_config_toml);
    };
    drop(_guard);

    let resolved_value = TomlValue::try_from(resolved).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to serialize resolved config: {e}"),
        )
    })?;

    Ok(copy_shape_from_original(
        &value_from_config_toml,
        &resolved_value,
    ))
}

/// Ensure that every field in `original` is present in the returned
/// `toml::Value`, taking the value from `resolved` where possible. This ensures
/// the fields that we "removed" during the serialize/deserialize round-trip in
/// `resolve_config_paths` are preserved, out of an abundance of caution.
fn copy_shape_from_original(original: &TomlValue, resolved: &TomlValue) -> TomlValue {
    match (original, resolved) {
        (TomlValue::Table(original_table), TomlValue::Table(resolved_table)) => {
            let mut table = toml::map::Map::new();
            for (key, original_value) in original_table {
                let resolved_value = resolved_table.get(key).unwrap_or(original_value);
                table.insert(
                    key.clone(),
                    copy_shape_from_original(original_value, resolved_value),
                );
            }
            TomlValue::Table(table)
        }
        (TomlValue::Array(original_array), TomlValue::Array(resolved_array)) => {
            let mut items = Vec::new();
            for (index, original_value) in original_array.iter().enumerate() {
                let resolved_value = resolved_array.get(index).unwrap_or(original_value);
                items.push(copy_shape_from_original(original_value, resolved_value));
            }
            TomlValue::Array(items)
        }
        (_, resolved_value) => resolved_value.clone(),
    }
}

/// Finds the nearest ancestor with a configured project marker, or returns `cwd`.
/// Callers must use markers from configuration loaded before project layers.
pub async fn find_project_root(
    fs: &dyn ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
    project_root_markers: &[String],
) -> io::Result<AbsolutePathBuf> {
    Ok(discover_project_root(fs, cwd, project_root_markers)
        .await?
        .unwrap_or_else(|| cwd.clone()))
}

/// Find a project marker, preserving absence instead of falling back to cwd.
pub async fn discover_project_root(
    fs: &dyn ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
    project_root_markers: &[String],
) -> io::Result<Option<AbsolutePathBuf>> {
    if project_root_markers.is_empty() {
        return Ok(None);
    }

    for ancestor in cwd.ancestors() {
        for marker in project_root_markers {
            let marker_path = ancestor.join(marker);
            let marker_path_uri = PathUri::from_abs_path(&marker_path);
            let Ok(metadata) = fs.get_metadata(&marker_path_uri, Default::default()).await else {
                continue;
            };
            if marker == ".git"
                && metadata.is_directory
                && fs
                    .get_metadata(
                        &PathUri::from_abs_path(&marker_path.join("HEAD")),
                        Default::default(),
                    )
                    .await
                    .is_err()
            {
                continue;
            }
            return Ok(Some(ancestor));
        }
    }
    Ok(None)
}

async fn find_git_checkout_root(
    fs: &dyn ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
) -> Option<AbsolutePathBuf> {
    let cwd_uri = PathUri::from_abs_path(cwd);
    let base = match fs.get_metadata(&cwd_uri, Default::default()).await {
        Ok(metadata) if metadata.is_directory => cwd.clone(),
        _ => cwd.parent()?,
    };

    for dir in base.ancestors() {
        let dot_git = dir.join(".git");
        let dot_git_uri = PathUri::from_abs_path(&dot_git);
        let Ok(metadata) = fs.get_metadata(&dot_git_uri, Default::default()).await else {
            continue;
        };
        if metadata.is_directory
            && fs
                .get_metadata(
                    &PathUri::from_abs_path(&dot_git.join("HEAD")),
                    Default::default(),
                )
                .await
                .is_err()
        {
            continue;
        }
        return Some(dir);
    }
    None
}

/// Return the appropriate list of layers (each with
/// [ConfigLayerSource::Project] as the source) between `cwd` and
/// `project_root`, inclusive. The list is ordered in _increasing_ precdence,
/// starting from folders closest to `project_root` (which is the lowest
/// precedence) to those closest to `cwd` (which is the highest precedence).
async fn load_project_layers(
    fs: &dyn ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
    project_root: &AbsolutePathBuf,
    codex_home: &Path,
    strict_config: bool,
) -> io::Result<Vec<ConfigLayerEntry>> {
    let checkout_root = find_git_checkout_root(fs, cwd).await;
    let repo_root = resolve_main_checkout_root(fs, checkout_root.as_ref()).await;
    let discovered =
        discover_project_layers(fs, cwd, project_root, codex_home, strict_config).await?;
    let mut layers = Vec::with_capacity(discovered.len());
    for layer in discovered {
        let hooks_config_folder_override = root_checkout_hooks_folder_for_dir(
            checkout_root.as_ref(),
            repo_root.as_ref(),
            &layer.dot_codex_folder,
        );
        let config =
            resolve_relative_paths_in_config_toml(layer.config, layer.dot_codex_folder.as_path())?;
        let config =
            merge_root_checkout_project_hooks(fs, config, hooks_config_folder_override.as_ref())
                .await?;
        layers.push(project_layer_entry(
            &layer.dot_codex_folder,
            config,
            hooks_config_folder_override,
        ));
    }
    Ok(layers)
}

#[derive(Debug, Clone)]
struct DiscoveredProjectLayer {
    dot_codex_folder: AbsolutePathBuf,
    config: TomlValue,
}

async fn discover_project_layers(
    fs: &dyn ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
    project_root: &AbsolutePathBuf,
    codex_home: &Path,
    strict_config: bool,
) -> io::Result<Vec<DiscoveredProjectLayer>> {
    let codex_home_abs = AbsolutePathBuf::from_absolute_path(codex_home)?;
    let codex_home_normalized =
        normalize_path(codex_home_abs.as_path()).unwrap_or_else(|_| codex_home_abs.to_path_buf());
    let mut dirs = cwd
        .ancestors()
        .scan(false, |done, a| {
            if *done {
                None
            } else {
                if &a == project_root {
                    *done = true;
                }
                Some(a)
            }
        })
        .collect::<Vec<_>>();
    dirs.reverse();

    let mut layers = Vec::new();
    for dir in dirs {
        let dot_codex_abs = dir.join(".codex");
        let dot_codex_uri = PathUri::from_abs_path(&dot_codex_abs);
        if !fs
            .get_metadata(&dot_codex_uri, Default::default())
            .await
            .map(|metadata| metadata.is_directory)
            .unwrap_or(false)
        {
            continue;
        }
        let dot_codex_normalized =
            normalize_path(dot_codex_abs.as_path()).unwrap_or_else(|_| dot_codex_abs.to_path_buf());
        if dot_codex_abs == codex_home_abs || dot_codex_normalized == codex_home_normalized {
            continue;
        }
        let config_file = dot_codex_abs.join(CONFIG_TOML_FILE);
        let config_file_uri = PathUri::from_abs_path(&config_file);
        let config = match fs
            .read_file_text(&config_file_uri, Default::default())
            .await
        {
            Ok(contents) => {
                let config: TomlValue = toml::from_str(&contents).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Error parsing project config file {}: {e}",
                            config_file.as_path().display()
                        ),
                    )
                })?;
                if strict_config {
                    validate_config_toml_strictly(
                        config_file.as_path(),
                        &contents,
                        &config,
                        dot_codex_abs.as_path(),
                    )?;
                }
                config
            }
            // A `.codex` folder without config.toml may still hold hooks and other files that
            // are significant in the overall ConfigLayerStack.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                TomlValue::Table(toml::map::Map::new())
            }
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!(
                        "Failed to read project config file {}: {err}",
                        config_file.as_path().display()
                    ),
                ));
            }
        };
        layers.push(DiscoveredProjectLayer {
            dot_codex_folder: dot_codex_abs,
            config,
        });
    }
    Ok(layers)
}

/// Linked worktrees share hook declarations with their main checkout.
fn root_checkout_hooks_folder_for_dir(
    checkout_root: Option<&AbsolutePathBuf>,
    repo_root: Option<&AbsolutePathBuf>,
    dot_codex_folder: &AbsolutePathBuf,
) -> Option<AbsolutePathBuf> {
    let checkout_root = checkout_root?;
    let repo_root = repo_root?;
    // Regular checkouts resolve both paths to the same root; linked worktrees do not.
    if checkout_root == repo_root {
        return None;
    }
    let dir = dot_codex_folder.parent()?;
    let relative_dir = dir.as_path().strip_prefix(checkout_root.as_path()).ok()?;
    Some(repo_root.join(relative_dir).join(".codex"))
}

/// Resolves a linked worktree's `.git` pointer file to its main checkout.
async fn resolve_main_checkout_root(
    fs: &dyn ExecutorFileSystem,
    checkout_root: Option<&AbsolutePathBuf>,
) -> Option<AbsolutePathBuf> {
    let checkout_root = checkout_root?;
    let dot_git = checkout_root.join(".git");
    let dot_git_uri = PathUri::from_abs_path(&dot_git);
    let metadata = fs
        .get_metadata(&dot_git_uri, Default::default())
        .await
        .ok()?;
    if metadata.is_directory {
        return Some(checkout_root.clone());
    }
    let contents = fs
        .read_file_text(&dot_git_uri, Default::default())
        .await
        .ok()?;
    let gitdir = contents.trim().strip_prefix("gitdir:")?.trim();
    let gitdir = AbsolutePathBuf::resolve_path_against_base(gitdir, checkout_root.as_path());
    // `<main>/.git/worktrees/<name>` -> `<main>`
    let worktrees_dir = gitdir.parent()?;
    if worktrees_dir.as_path().file_name()? != "worktrees" {
        return None;
    }
    worktrees_dir.parent()?.parent()
}

/// For linked worktrees, preserve ordinary worktree-local project config while
/// replacing only hook declarations with the matching root-checkout layer.
async fn merge_root_checkout_project_hooks(
    fs: &dyn ExecutorFileSystem,
    mut config: TomlValue,
    hooks_config_folder_override: Option<&AbsolutePathBuf>,
) -> io::Result<TomlValue> {
    let Some(hooks_config_folder) = hooks_config_folder_override else {
        return Ok(config);
    };
    let root_config = load_root_checkout_project_config(fs, hooks_config_folder).await?;
    let root_config =
        resolve_relative_paths_in_config_toml(root_config, hooks_config_folder.as_path())?;

    let Some(config_table) = config.as_table_mut() else {
        return Ok(config);
    };
    config_table.remove("hooks");
    if let Some(hooks) = root_config.get("hooks") {
        config_table.insert("hooks".to_string(), hooks.clone());
    }
    Ok(config)
}

async fn load_root_checkout_project_config(
    fs: &dyn ExecutorFileSystem,
    hooks_config_folder: &AbsolutePathBuf,
) -> io::Result<TomlValue> {
    let hooks_config_file = hooks_config_folder.join(CONFIG_TOML_FILE);
    let hooks_config_file_uri = PathUri::from_abs_path(&hooks_config_file);
    Ok(
        match fs
            .read_file_text(&hooks_config_file_uri, Default::default())
            .await
        {
            Ok(contents) => toml::from_str::<TomlValue>(&contents).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Error parsing project hooks config file {}: {err}",
                        hooks_config_file.as_path().display()
                    ),
                )
            })?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                TomlValue::Table(toml::map::Map::new())
            }
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!(
                        "Failed to read project hooks config file {}: {err}",
                        hooks_config_file.as_path().display()
                    ),
                ));
            }
        },
    )
}
// Cannot name this `mod tests` because of tests.rs in this folder.
