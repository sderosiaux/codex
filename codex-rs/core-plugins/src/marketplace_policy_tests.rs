use super::*;
use crate::installed_marketplaces::marketplace_install_root;
use crate::marketplace_add::parse_marketplace_source;
use crate::marketplace_upgrade::upgrade_configured_git_marketplaces;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use pretty_assertions::assert_eq;
use std::fs;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
use tempfile::TempDir;

fn config_layer_stack_with_user_config(contents: &str, file: AbsolutePathBuf) -> ConfigLayerStack {
    ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file,
            profile: None,
        },
        toml::from_str(contents).expect("parse user config"),
    )])
    .expect("build config layer stack")
}

fn parse_source(source: &str, ref_name: Option<&str>) -> MarketplaceSource {
    parse_marketplace_source(source, ref_name.map(str::to_string)).expect("parse source")
}

#[cfg(target_os = "windows")]
#[test]
fn primary_runtime_cache_uses_user_profile_on_windows() {
    assert_eq!(
        primary_runtime_cache_dir_from_user_profile(Some(PathBuf::from(r"C:\Users\user"))),
        Some(PathBuf::from(r"C:\Users\user\.cache"))
    );
    assert!(is_expected_managed_path(
        Path::new(r"\\?\c:\users\USER\.cache"),
        Path::new(r"C:\Users/user\.cache"),
    ));
}

#[cfg(windows)]
#[test]
fn managed_windows_root_accepts_redirected_target_but_not_unowned_alias() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("redirected-cache");
    let owned = temp.path().join(".cache");
    let alias = temp.path().join("unowned-alias");
    let other = temp.path().join("different-source");
    fs::create_dir_all(&target).unwrap();
    fs::create_dir_all(&other).unwrap();
    crate::test_support::create_directory_junction(&target, &owned);
    crate::test_support::create_directory_junction(&target, &alias);
    let canonical = owned.canonicalize().unwrap();
    assert!(is_expected_managed_path(&canonical, &owned));
    assert!(is_expected_managed_path(&owned, &owned));
    assert!(!is_expected_managed_path(&alias, &owned));
    assert!(!is_expected_managed_path(&other, &owned));
}

#[test]
fn curated_marketplace_requires_its_expected_name() {
    let codex_home = TempDir::new().expect("create Codex home");
    let marketplace_path = AbsolutePathBuf::try_from(
        curated_plugins_repo_path(codex_home.path()).join(".agents/plugins/marketplace.json"),
    )
    .expect("absolute marketplace path");

    assert_eq!(
        validate_marketplace_install(
            codex_home.path(),
            &marketplace_path,
            crate::OPENAI_CURATED_MARKETPLACE_NAME,
        ),
        Ok(())
    );
    assert!(
        validate_marketplace_install(
            codex_home.path(),
            &marketplace_path,
            crate::OPENAI_API_CURATED_MARKETPLACE_NAME,
        )
        .is_err()
    );

    let repository_manifest = AbsolutePathBuf::try_from(
        codex_home
            .path()
            .join("repository/.agents/plugins/marketplace.json"),
    )
    .expect("absolute repository manifest");
    for marketplace in [
        OPENAI_CURATED_MARKETPLACE_NAME,
        OPENAI_API_CURATED_MARKETPLACE_NAME,
        OPENAI_BUNDLED_MARKETPLACE_NAME,
        OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME,
        OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME,
        crate::remote::REMOTE_GLOBAL_MARKETPLACE_NAME,
        crate::remote::REMOTE_CREATED_BY_ME_MARKETPLACE_NAME,
        crate::remote::REMOTE_WORKSPACE_MARKETPLACE_NAME,
        crate::remote::REMOTE_WORKSPACE_SHARED_WITH_ME_MARKETPLACE_NAME,
        crate::remote::REMOTE_WORKSPACE_SHARED_WITH_ME_PRIVATE_MARKETPLACE_NAME,
        crate::remote::REMOTE_WORKSPACE_SHARED_WITH_ME_UNLISTED_MARKETPLACE_NAME,
    ] {
        assert!(
            validate_marketplace_install(codex_home.path(), &repository_manifest, marketplace)
                .is_err()
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_marketplaces_cannot_borrow_managed_provenance() {
    let codex_home = TempDir::new().expect("create Codex home");
    let official_root = curated_plugins_repo_path(codex_home.path());
    let manifest = official_root.join(".agents/plugins/marketplace.json");
    fs::create_dir_all(manifest.parent().expect("manifest directory"))
        .expect("create manifest directory");
    fs::write(&manifest, "{}").expect("write official manifest");

    let root_alias = codex_home.path().join("repository");
    std::os::unix::fs::symlink(&official_root, &root_alias).expect("symlink official root");
    let parent_alias = codex_home.path().join("parent-alias");
    std::os::unix::fs::symlink(
        official_root.parent().expect("official root parent"),
        &parent_alias,
    )
    .expect("symlink official root parent");
    let manifest_alias = codex_home
        .path()
        .join("manifest-alias/.agents/plugins/api_marketplace.json");
    fs::create_dir_all(manifest_alias.parent().expect("manifest alias parent"))
        .expect("create manifest alias directory");
    std::os::unix::fs::symlink(&manifest, &manifest_alias).expect("symlink official manifest");

    for (path, marketplace) in [
        (
            root_alias.join(".agents/plugins/marketplace.json"),
            OPENAI_CURATED_MARKETPLACE_NAME,
        ),
        (
            parent_alias.join("plugins/.agents/plugins/marketplace.json"),
            OPENAI_CURATED_MARKETPLACE_NAME,
        ),
        (manifest_alias, OPENAI_API_CURATED_MARKETPLACE_NAME),
    ] {
        let manifest = AbsolutePathBuf::try_from(path).expect("absolute repository manifest");
        assert!(validate_marketplace_install(codex_home.path(), &manifest, marketplace).is_err());
    }
}

#[test]
fn managed_bundled_source_is_bound_to_its_expected_name() {
    let codex_home = TempDir::new().expect("create Codex home");
    let bundled_root = codex_home
        .path()
        .join(".tmp/bundled-marketplaces")
        .join(crate::OPENAI_BUNDLED_MARKETPLACE_NAME);
    fs::create_dir_all(&bundled_root).expect("create bundled marketplace root");
    let source = parse_source(
        bundled_root.to_string_lossy().as_ref(),
        /*ref_name*/ None,
    );

    let expected_name = managed_marketplace_name_for_add(codex_home.path(), &source);
    assert_eq!(expected_name, Some(crate::OPENAI_BUNDLED_MARKETPLACE_NAME));
    assert_eq!(
        validate_marketplace_name_for_add(expected_name, crate::OPENAI_BUNDLED_MARKETPLACE_NAME,),
        Ok(())
    );
    assert!(validate_marketplace_name_for_add(expected_name, "other").is_err());
}

#[test]
fn managed_bundled_config_is_retained_only_at_its_owned_path() {
    let codex_home = TempDir::new().expect("create Codex home");
    let bundled_root = codex_home
        .path()
        .join(".tmp/bundled-marketplaces")
        .join(crate::OPENAI_BUNDLED_MARKETPLACE_NAME);
    let config_file = AbsolutePathBuf::try_from(codex_home.path().join("config.toml"))
        .expect("absolute config path");
    let stack = config_layer_stack_with_user_config(
        &format!(
            r#"
[marketplaces.openai-bundled]
source_type = "local"
source = {bundled_root:?}

[marketplaces.openai-bundled-alpha]
source_type = "local"
source = "/tmp/not-managed"

[marketplaces.evil]
source_type = "local"
source = {bundled_root:?}

[plugins."sample@openai-bundled"]
enabled = true

[plugins."sample@openai-bundled-alpha"]
enabled = true

[plugins."sample@evil"]
enabled = true
"#
        ),
        config_file,
    );

    let projected =
        filtered_plugin_config(&stack, codex_home.path()).expect("project plugin config");

    assert_eq!(
        projected["marketplaces"]
            .as_table()
            .expect("projected marketplaces")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![crate::OPENAI_BUNDLED_MARKETPLACE_NAME.to_string()]
    );
    assert_eq!(
        projected["plugins"]
            .as_table()
            .expect("projected plugins")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![format!("sample@{}", crate::OPENAI_BUNDLED_MARKETPLACE_NAME)]
    );
}

#[test]
fn configured_marketplaces_cannot_claim_reserved_names() {
    let codex_home = TempDir::new().expect("create Codex home");
    let source_root = TempDir::new().expect("create marketplace root");
    let source_root = source_root
        .path()
        .canonicalize()
        .expect("canonical marketplace root");
    let config_file = AbsolutePathBuf::try_from(codex_home.path().join("config.toml"))
        .expect("absolute config path");
    let stack = config_layer_stack_with_user_config(
        &format!(
            r#"
[marketplaces.openai-bundled]
source_type = "local"
source = {source_root:?}

[marketplaces.openai-curated]
source_type = "local"
source = {source_root:?}

[plugins."sample@openai-bundled"]
enabled = true

[plugins."sample@openai-curated"]
enabled = true
"#
        ),
        config_file,
    );

    let projected =
        filtered_plugin_config(&stack, codex_home.path()).expect("project plugin config");
    assert_eq!(
        projected["marketplaces"]
            .as_table()
            .expect("projected marketplaces")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );
    assert_eq!(
        projected["plugins"]
            .as_table()
            .expect("projected plugins")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );
}

#[test]
fn reserved_upgrade_is_rejected_before_marketplace_installation() {
    let reload_config: crate::ConfigLayerReload =
        std::sync::Arc::new(|| panic!("reserved marketplace must not reload config"));
    let codex_home = TempDir::new().expect("create Codex home");
    let config_file = AbsolutePathBuf::try_from(codex_home.path().join("config.toml"))
        .expect("absolute config path");
    let stack = config_layer_stack_with_user_config(
        "[marketplaces.openai-bundled]\nsource_type = \"git\"\nsource = \"https://github.com/example/blocked.git\"\n",
        config_file,
    );
    let outcome = upgrade_configured_git_marketplaces(
        codex_home.path(),
        &stack,
        Some(crate::OPENAI_BUNDLED_MARKETPLACE_NAME),
        &reload_config,
    );
    assert!(outcome.errors[0].message.contains("is reserved"));
    assert!(!marketplace_install_root(codex_home.path()).exists());
}
