use super::*;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn test_user_config_path(temp_dir: &TempDir, file_name: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(temp_dir.path().join(file_name))
        .expect("test user config path should be absolute")
}

#[test]
fn with_user_config_rejects_malformed_shell_policy_filter_fields() {
    let temp_dir = TempDir::new().expect("tempdir");
    let config_file = test_user_config_path(&temp_dir, "config.toml");
    let cases = [
        r#"
[shell_environment_policy]
exclude = ["SECRET_*", 17]
"#,
        r#"
[shell_environment_policy.filters]
"SECRET_*" = "keep"
"#,
        r#"
[shell_environment_policy]
exclude = ["SECRET_*"]

[shell_environment_policy.filters]
"PATH" = "include"
"#,
        r#"
[shell_environment_policy.filters]
"SECRET_*" = "exclude"
"secret_*" = "include"
"#,
    ];

    for contents in cases {
        let error = ConfigLayerStack::default()
            .with_user_config(&config_file, toml::from_str(contents).expect("user config"))
            .expect_err("malformed shell policy filter fields should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
