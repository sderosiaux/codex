use crate::context::world_state::test_support::FragmentSectionTestExt as _;
use crate::shell::ShellType;

use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::test_support::PathBufExt;
use core_test_support::test_path_buf;
use pretty_assertions::assert_eq;
use std::path::PathBuf;

fn fake_shell_name() -> String {
    let shell = crate::shell::Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };
    shell.name().to_string()
}

fn test_abs_path(unix_path: &str) -> AbsolutePathBuf {
    test_path_buf(unix_path).abs()
}

fn environment(id: &str, cwd: PathUri, shell: impl Into<String>) -> (String, EnvironmentState) {
    (
        id.to_string(),
        EnvironmentState {
            cwd,
            status: EnvironmentStatus::Available,
            error: None,
            shell: Some(shell.into()),
            is_primary: false,
        },
    )
}

fn environment_state(
    environments: impl IntoIterator<Item = (String, EnvironmentState)>,
    current_date: Option<String>,
    timezone: Option<String>,
    subagents: Option<String>,
) -> EnvironmentsState {
    let environments = environments
        .into_iter()
        .enumerate()
        .map(|(index, (id, mut environment))| {
            environment.is_primary = index == 0;
            (id, environment)
        })
        .collect();
    EnvironmentsState {
        environments,
        shell_version: None,
        current_date,
        timezone,
        filesystem: None,
        subagents,
    }
}

#[test]
fn serialize_workspace_write_environment_context() {
    let cwd = test_path_buf("/repo");
    let context = environment_state(
        [environment(
            "local",
            PathUri::from_abs_path(&cwd.abs()),
            fake_shell_name(),
        )],
        Some("2026-02-26".to_string()),
        Some("America/Los_Angeles".to_string()),
        /*subagents*/ None,
    );

    let expected = format!(
        r#"<environment_context>
  <cwd>{cwd}</cwd>
  <shell>bash</shell>
  <current_date>2026-02-26</current_date>
  <timezone>America/Los_Angeles</timezone>
</environment_context>"#,
        cwd = cwd.display(),
    );

    assert_eq!(context.render(), expected);
}

#[test]
fn serialize_environment_context_with_foreign_windows_cwd() {
    let mut context = environment_state(
        [environment(
            "remote",
            PathUri::parse("file:///C:/windows").expect("Windows cwd URI"),
            "powershell",
        )],
        /*current_date*/ None,
        /*timezone*/ None,
        /*subagents*/ None,
    );
    context.filesystem =
        FileSystemContext::from_workspace_roots(&[
            PathUri::parse("file:///D:/workspace").expect("Windows workspace root URI")
        ]);

    assert_eq!(
        context.render(),
        r#"<environment_context>
  <cwd>C:\windows</cwd>
  <shell>powershell</shell>
  <filesystem><workspace_roots><root>D:\workspace</root></workspace_roots></filesystem>
</environment_context>"#
    );
}

#[test]
fn serialize_environment_context_with_workspace_roots() {
    let repo = test_abs_path("/repo");
    let other_repo = test_abs_path("/other-repo");
    let mut context = environment_state(
        [environment(
            "local",
            PathUri::from_abs_path(&repo),
            fake_shell_name(),
        )],
        /*current_date*/ None,
        /*timezone*/ None,
        /*subagents*/ None,
    );
    context.filesystem = FileSystemContext::from_workspace_roots(&[
        PathUri::from_abs_path(&repo),
        PathUri::from_abs_path(&other_repo),
    ]);

    let expected = format!(
        r#"<environment_context>
  <cwd>{repo}</cwd>
  <shell>bash</shell>
  <filesystem><workspace_roots><root>{repo}</root><root>{other_repo}</root></workspace_roots></filesystem>
</environment_context>"#,
        repo = repo.to_string_lossy(),
        other_repo = other_repo.to_string_lossy(),
    );

    assert_eq!(context.render(), expected);
}

#[test]
fn serialize_read_only_environment_context() {
    let context = environment_state(
        Vec::new(),
        Some("2026-02-26".to_string()),
        Some("America/Los_Angeles".to_string()),
        /*subagents*/ None,
    );

    let expected = r#"<environment_context>
  <current_date>2026-02-26</current_date>
  <timezone>America/Los_Angeles</timezone>
</environment_context>"#;

    assert_eq!(context.render(), expected);
}

#[test]
fn serialize_environment_context_with_subagents() {
    let context = environment_state(
        [environment(
            "local",
            PathUri::from_abs_path(&test_abs_path("/repo")),
            fake_shell_name(),
        )],
        Some("2026-02-26".to_string()),
        Some("America/Los_Angeles".to_string()),
        Some("- agent-1: atlas\n- agent-2".to_string()),
    );

    let expected = format!(
        r#"<environment_context>
  <cwd>{}</cwd>
  <shell>bash</shell>
  <current_date>2026-02-26</current_date>
  <timezone>America/Los_Angeles</timezone>
  <subagents>
    - agent-1: atlas
    - agent-2
  </subagents>
</environment_context>"#,
        test_path_buf("/repo").display()
    );

    assert_eq!(context.render(), expected);
}

#[test]
fn serialize_environment_context_with_multiple_selected_environments() {
    let local_cwd = test_path_buf("/repo/local");
    let remote_cwd = test_path_buf("/repo/remote");
    let context = environment_state(
        [
            environment("local", PathUri::from_abs_path(&local_cwd.abs()), "bash"),
            environment("remote", PathUri::from_abs_path(&remote_cwd.abs()), "bash"),
        ],
        Some("2026-02-26".to_string()),
        Some("America/Los_Angeles".to_string()),
        /*subagents*/ None,
    );

    let expected = format!(
        r#"<environment_context>
  <environments>
    <environment id="local" primary="true">
      <cwd>{}</cwd>
      <shell>bash</shell>
    </environment>
    <environment id="remote" primary="false">
      <cwd>{}</cwd>
      <shell>bash</shell>
    </environment>
  </environments>
  <current_date>2026-02-26</current_date>
  <timezone>America/Los_Angeles</timezone>
</environment_context>"#,
        local_cwd.display(),
        remote_cwd.display()
    );

    assert_eq!(context.render(), expected);
}

#[test]
fn serialize_environment_context_prefers_environment_shell_when_present() {
    let local_cwd = test_path_buf("/repo/local");
    let remote_cwd = test_path_buf("/repo/remote");
    let context = environment_state(
        [
            environment(
                "local",
                PathUri::from_abs_path(&local_cwd.abs()),
                "powershell",
            ),
            environment("remote", PathUri::from_abs_path(&remote_cwd.abs()), "cmd"),
        ],
        /*current_date*/ None,
        /*timezone*/ None,
        /*subagents*/ None,
    );

    let expected = format!(
        r#"<environment_context>
  <environments>
    <environment id="local" primary="true">
      <cwd>{}</cwd>
      <shell>powershell</shell>
    </environment>
    <environment id="remote" primary="false">
      <cwd>{}</cwd>
      <shell>cmd</shell>
    </environment>
  </environments>
</environment_context>"#,
        local_cwd.display(),
        remote_cwd.display()
    );

    assert_eq!(context.render(), expected);
}

fn powershell_environment() -> EnvironmentsState {
    let cwd = PathUri::from_abs_path(&test_abs_path("/repo"));
    EnvironmentsState {
        environments: [environment("local", cwd, "powershell")].into(),
        shell_version: Some("5.1".to_string()),
        ..Default::default()
    }
}

#[test]
fn shell_version_diff_restates_shell_from_legacy_snapshot() {
    let current = powershell_environment();
    let mut previous = current
        .render_fragment_diff(PreviousSectionState::Absent)
        .0
        .unwrap();
    previous.shell_version = None;
    previous.environments.get_mut("local").expect("local").shell = None;
    let rendered = current
        .render_fragment_diff(PreviousSectionState::Known(&previous))
        .1
        .expect("shell version update")
        .render();
    assert!(
        rendered.contains("<shell>powershell</shell>\n  <shell_version>5.1</shell_version>"),
        "{rendered}"
    );
}

#[test]
fn shell_version_diff_clears_previously_visible_version() {
    let previous = powershell_environment();
    let current = EnvironmentsState {
        shell_version: None,
        ..previous.clone()
    };
    assert_eq!(
        current
            .render_fragment_diff(PreviousSectionState::Known(
                &previous
                    .render_fragment_diff(PreviousSectionState::Absent)
                    .0
                    .unwrap()
            ))
            .1
            .expect("removed shell version")
            .render(),
        "<environment_context>\n  <shell_version status=\"unavailable\" />\n</environment_context>"
    );
}

#[test]
fn current_date_diff_clears_once_and_recovers() {
    let available = EnvironmentsState {
        current_date: Some("2026-06-17".to_string()),
        ..Default::default()
    };
    let unavailable = EnvironmentsState::default();
    assert_eq!(
        unavailable
            .render_fragment_diff(PreviousSectionState::Known(
                &available
                    .render_fragment_diff(PreviousSectionState::Absent)
                    .0
                    .unwrap()
            ))
            .1
            .expect("removed current date")
            .render(),
        "<environment_context>\n  <current_date status=\"unavailable\" />\n</environment_context>"
    );
    assert!(
        unavailable
            .render_fragment_diff(PreviousSectionState::Known(
                &unavailable
                    .render_fragment_diff(PreviousSectionState::Absent)
                    .0
                    .unwrap()
            ))
            .1
            .is_none()
    );
    assert_eq!(
        available
            .render_fragment_diff(PreviousSectionState::Known(
                &unavailable
                    .render_fragment_diff(PreviousSectionState::Absent)
                    .0
                    .unwrap()
            ))
            .1
            .expect("restored current date")
            .render(),
        "<environment_context>\n  <current_date>2026-06-17</current_date>\n</environment_context>"
    );
}
