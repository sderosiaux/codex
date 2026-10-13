use super::*;
use codex_file_system::CopyOptions;
use codex_file_system::CreateDirectoryOptions;
use codex_file_system::ExecutorFileSystemFuture;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemReadStream;
use codex_file_system::GetMetadataOptions;
use codex_file_system::ReadDirectoryEntry;
use codex_file_system::ReadFileOptions;
use codex_file_system::RemoveOptions;
use codex_file_system::WalkOptions;
use codex_file_system::WalkOutcome;
use codex_file_system::WriteFileOptions;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

pub(super) struct TestFileSystem;

impl ExecutorFileSystem for TestFileSystem {
    fn canonicalize<'a>(&'a self, path: &'a PathUri) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move {
            let path = path.to_abs_path()?;
            let canonicalized = path.canonicalize()?;
            Ok(PathUri::from_abs_path(&canonicalized))
        })
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        _options: ReadFileOptions,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let path = path.to_abs_path()?;
            tokio::fs::read(path.as_path()).await
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "test filesystem does not support streaming reads",
            ))
        })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _options: WriteFileOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _create_directory_options: CreateDirectoryOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        _options: GetMetadataOptions,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            let path = path.to_abs_path()?;
            let metadata = tokio::fs::symlink_metadata(path.as_path()).await?;
            Ok(FileMetadata {
                is_directory: metadata.is_dir(),
                is_file: metadata.is_file(),
                is_symlink: metadata.file_type().is_symlink(),
                size: metadata.len(),
                created_at_ms: 0,
                modified_at_ms: 0,
            })
        })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn walk<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: WalkOptions,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        unimplemented!()
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _remove_options: RemoveOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _copy_options: CopyOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move { unimplemented!("test filesystem only supports reads") })
    }
}

#[tokio::test]
async fn packaged_defaults_have_lower_precedence_than_existing_config_layers() {
    let tmp = tempdir().expect("tempdir");
    let packaged_defaults_path =
        AbsolutePathBuf::resolve_path_against_base("packaged-defaults.toml", tmp.path());
    let system_config_path = tmp.path().join("system.toml");
    let user_config_path = tmp.path().join(CONFIG_TOML_FILE);

    std::fs::write(
        packaged_defaults_path.as_path(),
        r#"
model = "packaged-model"
model_provider = "packaged-provider"
model_context_window = 120000
"#,
    )
    .expect("write packaged defaults");
    std::fs::write(
        &system_config_path,
        r#"
model = "system-model"
model_provider = "system-provider"
"#,
    )
    .expect("write system config");
    std::fs::write(&user_config_path, r#"model = "user-model""#).expect("write user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.packaged_defaults_path = Some(packaged_defaults_path.clone());
    overrides.system_config_path = Some(system_config_path.clone());

    let stack = load_config_layers_state(
        &TestFileSystem,
        tmp.path(),
        /*cwd*/ None,
        &[(
            "model".to_string(),
            TomlValue::String("session-model".to_string()),
        )],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect("load config layers");

    assert_eq!(
        stack
            .all_layers_low_to_high()
            .map(|layer| layer.name.clone())
            .collect::<Vec<_>>(),
        vec![
            ConfigLayerSource::PackagedDefaults {
                file: packaged_defaults_path,
            },
            ConfigLayerSource::System {
                file: AbsolutePathBuf::from_absolute_path(system_config_path)
                    .expect("absolute system config path"),
            },
            ConfigLayerSource::User {
                file: AbsolutePathBuf::from_absolute_path(user_config_path)
                    .expect("absolute user config path"),
                profile: None,
            },
            ConfigLayerSource::SessionFlags,
        ]
    );
    assert_eq!(
        stack.effective_config(),
        toml::toml! {
            model = "session-model"
            model_provider = "system-provider"
            model_context_window = 120000
        }
        .into()
    );
}

#[tokio::test]
async fn missing_packaged_defaults_file_returns_an_error() {
    let tmp = tempdir().expect("tempdir");
    let packaged_defaults_path =
        AbsolutePathBuf::resolve_path_against_base("packaged-defaults.toml", tmp.path());
    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.packaged_defaults_path = Some(packaged_defaults_path.clone());

    let err = load_config_layers_state(
        &TestFileSystem,
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect_err("an explicitly configured packaged defaults file must exist");

    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(
        err.to_string(),
        format!(
            "packaged defaults config file {} not found",
            packaged_defaults_path.display()
        )
    );
}

#[cfg(windows)]
#[cfg(windows)]
#[tokio::test]
async fn profile_v2_rejects_matching_legacy_profile_in_base_user_config() {
    let tmp = tempdir().expect("tempdir");
    let selected_config = tmp.path().join("work.config.toml");

    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
model = "gpt-main"

[profiles.work]
model = "gpt-work"
"#,
    )
    .expect("write default user config");
    std::fs::write(&selected_config, r#"model = "gpt-work-v2""#)
        .expect("write selected user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.user_config_path = Some(AbsolutePathBuf::resolve_path_against_base(
        "work.config.toml",
        tmp.path(),
    ));
    overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));

    let err = load_config_layers_state(
        &TestFileSystem,
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect_err("profile-v2 should reject a matching legacy profile in base user config");

    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "a matching legacy profile should be a hard config error"
    );
    let message = err.to_string();
    assert!(
        message.contains("--profile `work` cannot be used"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("config.toml"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("[profiles.work]"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("https://developers.openai.com/codex/config-advanced#profiles"),
        "unexpected error message: {message}"
    );
}

#[tokio::test]
async fn profile_v2_rejects_matching_legacy_profile_selector_in_base_user_config() {
    let tmp = tempdir().expect("tempdir");
    let selected_config = tmp.path().join("work.config.toml");

    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
profile = "work"
model = "gpt-main"
"#,
    )
    .expect("write default user config");
    std::fs::write(&selected_config, r#"model = "gpt-work-v2""#)
        .expect("write selected user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.user_config_path = Some(AbsolutePathBuf::resolve_path_against_base(
        "work.config.toml",
        tmp.path(),
    ));
    overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));

    let err = load_config_layers_state(
        &TestFileSystem,
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect_err("profile-v2 should reject a matching legacy profile selector");

    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidData,
        "a matching legacy profile selector should be a hard config error"
    );
    let message = err.to_string();
    assert!(
        message.contains("--profile `work` cannot be used"),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("profile = \"work\""),
        "unexpected error message: {message}"
    );
    assert!(
        message.contains("work.config.toml"),
        "unexpected error message: {message}"
    );
}

#[tokio::test]
async fn profile_v2_allows_unrelated_legacy_profiles_in_base_user_config() {
    let tmp = tempdir().expect("tempdir");
    let selected_config = tmp.path().join("work.config.toml");

    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
model = "gpt-main"

[profiles.dev]
model = "gpt-dev"
"#,
    )
    .expect("write default user config");
    std::fs::write(&selected_config, r#"model = "gpt-work-v2""#)
        .expect("write selected user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.user_config_path = Some(AbsolutePathBuf::resolve_path_against_base(
        "work.config.toml",
        tmp.path(),
    ));
    overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));

    load_config_layers_state(
        &TestFileSystem,
        tmp.path(),
        /*cwd*/ None,
        &[],
        overrides,
        &crate::NoopThreadConfigLoader,
    )
    .await
    .expect("profile-v2 should allow unrelated legacy profiles in base user config");
}
