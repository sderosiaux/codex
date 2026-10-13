use crate::config_toml::ConfigToml;
use crate::diagnostics::config_error_from_toml;
use crate::diagnostics::io_error_from_config_error;
use crate::strict_config::config_error_from_ignored_toml_value_fields;
use codex_file_system::ExecutorFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use codex_utils_path_uri::PathUri;
use std::io;
use toml::Value as TomlValue;

pub(super) async fn read_config_from_path(
    fs: &dyn ExecutorFileSystem,
    path: &AbsolutePathBuf,
    log_missing_as_info: bool,
    strict_config: bool,
) -> io::Result<Option<TomlValue>> {
    let path_uri = PathUri::from_abs_path(path);
    match fs.read_file_text(&path_uri, Default::default()).await {
        Ok(contents) => match toml::from_str::<TomlValue>(&contents) {
            Ok(value) => {
                if strict_config {
                    validate_config_toml_strictly(path, &contents, &value)?;
                }
                Ok(Some(value))
            }
            Err(err) => {
                tracing::error!("Failed to parse {}: {err}", path.as_path().display());
                let config_error = config_error_from_toml(path.as_path(), &contents, err.clone());
                Err(io_error_from_config_error(
                    io::ErrorKind::InvalidData,
                    config_error,
                    Some(err),
                ))
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if log_missing_as_info {
                tracing::info!("{} not found, using defaults", path.as_path().display());
            } else {
                tracing::debug!("{} not found", path.as_path().display());
            }
            Ok(None)
        }
        Err(err) => {
            tracing::error!("Failed to read {}: {err}", path.as_path().display());
            Err(err)
        }
    }
}

fn validate_config_toml_strictly(
    path: &AbsolutePathBuf,
    contents: &str,
    value: &TomlValue,
) -> io::Result<()> {
    let Some(base_dir) = path.as_path().parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Config file {} has no parent directory", path.display()),
        ));
    };
    let _guard = AbsolutePathBufGuard::new(base_dir);
    if let Some(config_error) = config_error_from_ignored_toml_value_fields::<ConfigToml>(
        path.as_path(),
        contents,
        value.clone(),
    ) {
        return Err(io_error_from_config_error(
            io::ErrorKind::InvalidData,
            config_error,
            /*source*/ None,
        ));
    }

    Ok(())
}
