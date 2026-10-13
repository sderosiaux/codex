#![cfg(unix)]
#![allow(clippy::expect_used)]

mod common;

#[path = "file_system/shared.rs"]
mod shared;
#[path = "file_system/support.rs"]
mod support;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
#[cfg(target_os = "linux")]
use codex_exec_server::Environment;
use codex_exec_server::FileMetadata;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::ReadFileOptions;
use codex_exec_server::RemoveOptions;
use codex_exec_server::WalkEntry;
use codex_exec_server::WalkEntryKind;
use codex_exec_server::WalkOptions;
use codex_exec_server::WalkOutcome;
use codex_exec_server::WriteFileOptions;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

#[cfg(target_os = "linux")]
use crate::common::exec_server::exec_server_with_env;

use crate::support::FileSystemImplementation;
use crate::support::create_file_system_context;

fn create_directory_symlink(target: &Path, alias: &Path) -> Result<()> {
    symlink(target, alias)?;
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_canonicalize_resolves_directory_symlink(
    implementation: FileSystemImplementation,
) -> Result<()> {
    shared::assert_canonicalize_resolves_directory_alias(implementation, create_directory_symlink)
        .await
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_operations_can_reject_symlinks_in_any_path_component(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let tmp = TempDir::new()?;
    let tmp_path = tmp.path().canonicalize()?;
    let real = tmp_path.join("real");
    std::fs::create_dir(&real)?;
    let existing = real.join("existing.txt");
    std::fs::write(&existing, "unchanged")?;
    let removable = real.join("removable.txt");
    std::fs::write(&removable, "keep")?;
    let directory_link = tmp_path.join("directory-link");
    symlink(&real, &directory_link)?;
    let file_link = tmp_path.join("file-link");
    symlink(&existing, &file_link)?;

    let no_follow_read = ReadFileOptions {
        follow_symlinks: false,
    };
    let no_follow_write = WriteFileOptions {
        follow_symlinks: false,
    };
    let no_follow_metadata = GetMetadataOptions {
        follow_symlinks: false,
    };
    let no_follow_create = CreateDirectoryOptions {
        recursive: true,
        follow_symlinks: false,
    };
    let no_follow_remove = RemoveOptions {
        recursive: false,
        force: false,
        follow_symlinks: false,
    };
    let uri = |path: &Path| PathUri::from_host_native_path(path);
    assert!(
        context
            .file_system
            .read_file(&uri(&file_link)?, no_follow_read)
            .await
            .is_err()
    );
    assert!(
        context
            .file_system
            .read_file(&uri(&directory_link.join("existing.txt"))?, no_follow_read)
            .await
            .is_err()
    );
    assert!(
        context
            .file_system
            .write_file(&uri(&file_link)?, b"changed".to_vec(), no_follow_write)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&existing)?, "unchanged");
    assert!(
        context
            .file_system
            .write_file(
                &uri(&directory_link.join("existing.txt"))?,
                b"changed".to_vec(),
                no_follow_write
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&existing)?, "unchanged");
    assert!(
        context
            .file_system
            .get_metadata(&uri(&file_link)?, no_follow_metadata)
            .await
            .is_err()
    );
    let directory_metadata = context
        .file_system
        .get_metadata(&uri(&real)?, no_follow_metadata)
        .await?;
    assert!(directory_metadata.is_directory);
    assert!(
        context
            .file_system
            .create_directory(&uri(&directory_link.join("created"))?, no_follow_create)
            .await
            .is_err()
    );
    assert!(!real.join("created").exists());
    assert!(
        context
            .file_system
            .remove(
                &uri(&directory_link.join("removable.txt"))?,
                no_follow_remove
            )
            .await
            .is_err()
    );
    assert!(removable.exists());
    assert!(
        context
            .file_system
            .remove(&uri(&file_link)?, no_follow_remove)
            .await
            .is_err()
    );
    assert!(file_link.symlink_metadata()?.file_type().is_symlink());

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test]
async fn file_system_no_follow_non_recursive_root_creation_fails(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let result = context
        .file_system
        .create_directory(
            &PathUri::from_host_native_path(Path::new("/"))?,
            CreateDirectoryOptions {
                recursive: false,
                follow_symlinks: false,
            },
        )
        .await;

    assert!(result.is_err());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test]
async fn file_system_no_follow_metadata_preserves_linux_birthtime(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let tmp = TempDir::new()?;
    let file = tmp.path().join("created.txt");
    std::fs::write(&file, "created")?;
    let expected = match std::fs::metadata(&file)?.created() {
        Ok(created) => Some(i64::try_from(
            created.duration_since(UNIX_EPOCH)?.as_millis(),
        )?),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => None,
        Err(error) => return Err(error.into()),
    };

    let metadata = context
        .file_system
        .get_metadata(
            &PathUri::from_host_native_path(&file)?,
            GetMetadataOptions {
                follow_symlinks: false,
            },
        )
        .await?;

    if let Some(expected) = expected {
        assert!(expected > 0);
        assert_eq!(metadata.created_at_ms, expected);
    } else {
        assert_eq!(metadata.created_at_ms, 0);
    }
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_no_follow_operations_support_search_only_ancestors(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let tmp = TempDir::new()?;
    let root = tmp.path().canonicalize()?;
    let search_only = root.join("search-only");
    std::fs::create_dir(&search_only)?;
    let existing = search_only.join("existing.txt");
    std::fs::write(&existing, "before")?;
    let unreadable = search_only.join("unreadable.txt");
    std::fs::write(&unreadable, "metadata only")?;
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))?;
    let socket_path = search_only.join("socket");
    let _socket = UnixListener::bind(&socket_path)?;
    let removable = search_only.join("removable.txt");
    std::fs::write(&removable, "remove")?;
    std::fs::set_permissions(&search_only, std::fs::Permissions::from_mode(0o300))?;

    let uri = |path: &Path| PathUri::from_host_native_path(path);
    let result: Result<()> = async {
        let root_metadata = context
            .file_system
            .get_metadata(
                &uri(Path::new("/"))?,
                GetMetadataOptions {
                    follow_symlinks: false,
                },
            )
            .await?;
        assert!(root_metadata.is_directory);

        assert_eq!(
            context
                .file_system
                .read_file(
                    &uri(&existing)?,
                    ReadFileOptions {
                        follow_symlinks: false,
                    }
                )
                .await?,
            b"before"
        );
        context
            .file_system
            .write_file(
                &uri(&existing)?,
                b"after".to_vec(),
                WriteFileOptions {
                    follow_symlinks: false,
                },
            )
            .await?;
        assert_eq!(std::fs::read_to_string(&existing)?, "after");

        for metadata_path in [&unreadable, &socket_path] {
            context
                .file_system
                .get_metadata(
                    &uri(metadata_path)?,
                    GetMetadataOptions {
                        follow_symlinks: false,
                    },
                )
                .await?;
        }

        let nested = search_only.join("created").join("nested");
        context
            .file_system
            .create_directory(
                &uri(&nested)?,
                CreateDirectoryOptions {
                    recursive: true,
                    follow_symlinks: false,
                },
            )
            .await?;
        assert!(nested.is_dir());

        context
            .file_system
            .remove(
                &uri(&removable)?,
                RemoveOptions {
                    recursive: false,
                    force: false,
                    follow_symlinks: false,
                },
            )
            .await?;
        assert!(!removable.exists());
        Ok(())
    }
    .await;

    std::fs::set_permissions(&search_only, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600))?;
    result
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_no_follow_write_rejects_fifo_without_blocking(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let tmp = TempDir::new()?;
    let fifo = tmp.path().canonicalize()?.join("fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let result = timeout(
        Duration::from_secs(1),
        context.file_system.write_file(
            &PathUri::from_host_native_path(&fifo)?,
            b"must not be written".to_vec(),
            WriteFileOptions {
                follow_symlinks: false,
            },
        ),
    )
    .await
    .context("strict FIFO write must not block")?;
    assert!(result.is_err());
    assert!(fifo.symlink_metadata()?.file_type().is_fifo());
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_system_no_follow_recursive_mkdir_handles_concurrent_creators(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let tmp = TempDir::new()?;
    let path = tmp.path().canonicalize()?.join("shared").join("nested");
    let path_uri = PathUri::from_host_native_path(&path)?;
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let file_system = Arc::clone(&context.file_system);
        let path_uri = path_uri.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            file_system
                .create_directory(
                    &path_uri,
                    CreateDirectoryOptions {
                        recursive: true,
                        follow_symlinks: false,
                    },
                )
                .await
        }));
    }
    for task in tasks {
        task.await??;
    }
    assert!(path.is_dir());
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_metadata_and_directory_listing_follow_symlinks(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let file_path = tmp.path().join("note.txt");
    std::fs::write(&file_path, "hello")?;
    let symlink_path = tmp.path().join("note-link.txt");
    symlink(&file_path, &symlink_path)?;
    let symlink_metadata = file_system
        .get_metadata(
            &PathUri::from_host_native_path(&symlink_path)?,
            Default::default(),
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        symlink_metadata,
        FileMetadata {
            is_directory: false,
            is_file: true,
            is_symlink: true,
            size: 5,
            created_at_ms: symlink_metadata.created_at_ms,
            modified_at_ms: symlink_metadata.modified_at_ms,
        }
    );
    assert!(symlink_metadata.modified_at_ms > 0);

    let dir_path = tmp.path().join("notes");
    std::fs::create_dir(&dir_path)?;
    let dir_symlink_path = tmp.path().join("notes-link");
    symlink(&dir_path, &dir_symlink_path)?;
    let dir_symlink_metadata = file_system
        .get_metadata(
            &PathUri::from_host_native_path(&dir_symlink_path)?,
            Default::default(),
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        dir_symlink_metadata,
        FileMetadata {
            is_directory: true,
            is_file: false,
            is_symlink: true,
            size: std::fs::metadata(&dir_path)?.len(),
            created_at_ms: dir_symlink_metadata.created_at_ms,
            modified_at_ms: dir_symlink_metadata.modified_at_ms,
        }
    );

    let dangling_symlink_path = tmp.path().join("dangling-link");
    symlink(tmp.path().join("missing"), &dangling_symlink_path)?;
    let error = file_system
        .get_metadata(
            &PathUri::from_host_native_path(&dangling_symlink_path)?,
            Default::default(),
        )
        .await
        .expect_err("dangling symlink should not resolve");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

    let mut entries = file_system
        .read_directory(&PathUri::from_host_native_path(tmp.path())?)
        .await
        .with_context(|| format!("mode={implementation}"))?;
    entries.retain(|entry| entry.file_name.contains("link"));
    entries.sort_by(|left, right| left.file_name.cmp(&right.file_name));

    assert_eq!(
        entries,
        vec![
            ReadDirectoryEntry {
                file_name: "note-link.txt".to_string(),
                is_directory: false,
                is_file: true,
            },
            ReadDirectoryEntry {
                file_name: "notes-link".to_string(),
                is_directory: true,
                is_file: false,
            },
        ]
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_walk_handles_directory_symlinks(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let root = tmp.path().join("root");
    let target = tmp.path().join("target");
    let target_file = target.join("note.txt");
    let target_link = root.join("target-link");
    let root_link = target.join("root-link");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&target)?;
    std::fs::write(&target_file, "target")?;
    symlink(&target, &target_link)?;
    symlink(&root, &root_link)?;
    symlink(&target_file, root.join("file-link"))?;
    symlink(root.join("missing"), root.join("broken-link"))?;

    for root in [&root, &root_link] {
        let target_link = root.join("target-link");

        let outcome = file_system
            .walk(
                &PathUri::from_host_native_path(root)?,
                WalkOptions {
                    max_depth: 2,
                    max_directories: 4,
                    max_entries: 8,
                    follow_directory_symlinks: false,
                    prune_hidden_directories: false,
                },
            )
            .await
            .with_context(|| format!("mode={implementation}"))?;
        assert_eq!(
            outcome,
            WalkOutcome {
                entries: Vec::new(),
                errors: Vec::new(),
                truncated: false,
            }
        );

        let outcome = file_system
            .walk(
                &PathUri::from_host_native_path(root)?,
                WalkOptions {
                    max_depth: 2,
                    max_directories: 4,
                    max_entries: 8,
                    follow_directory_symlinks: true,
                    prune_hidden_directories: false,
                },
            )
            .await
            .with_context(|| format!("mode={implementation}"))?;
        assert_eq!(
            outcome,
            WalkOutcome {
                entries: vec![
                    WalkEntry {
                        path: PathUri::from_host_native_path(&target_link)?,
                        kind: WalkEntryKind::Directory,
                    },
                    WalkEntry {
                        path: PathUri::from_host_native_path(target_link.join("note.txt"))?,
                        kind: WalkEntryKind::File,
                    },
                    WalkEntry {
                        path: PathUri::from_host_native_path(target_link.join("root-link"))?,
                        kind: WalkEntryKind::Directory,
                    },
                ],
                errors: Vec::new(),
                truncated: false,
            }
        );
    }

    Ok(())
}

#[cfg(target_os = "linux")]
#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_walk_reports_non_utf8_names(
    implementation: FileSystemImplementation,
) -> Result<()> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use codex_exec_server::WalkError;

    let context = create_file_system_context(implementation).await?;
    let tmp = TempDir::new()?;
    std::fs::write(
        tmp.path()
            .join(OsString::from_vec(b"invalid-\xff".to_vec())),
        "contents",
    )?;
    let lossy_path = tmp.path().join("invalid-\u{fffd}");
    let error =
        std::fs::symlink_metadata(&lossy_path).expect_err("the lossy filename must not exist");
    let outcome = context
        .file_system
        .walk(
            &PathUri::from_host_native_path(tmp.path())?,
            WalkOptions {
                max_depth: 0,
                max_directories: 1,
                max_entries: 1,
                follow_directory_symlinks: false,
                prune_hidden_directories: false,
            },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        outcome,
        WalkOutcome {
            entries: Vec::new(),
            errors: vec![WalkError {
                path: PathUri::from_host_native_path(lossy_path)?,
                message: error.to_string(),
            }],
            truncated: false,
        }
    );
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_walk_prunes_hidden_directories_without_claiming_visible_aliases(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let root = tmp.path().join("root");
    let hidden = root.join(".hidden");
    let hidden_nested = hidden.join("nested");
    let visible = root.join("visible");
    std::fs::create_dir_all(&hidden_nested)?;
    std::fs::write(hidden_nested.join("note.txt"), "visible through alias")?;
    symlink(&hidden, &visible)?;

    let outcome = file_system
        .walk(
            &PathUri::from_host_native_path(&root)?,
            WalkOptions {
                max_depth: 3,
                max_directories: 3,
                max_entries: 6,
                follow_directory_symlinks: true,
                prune_hidden_directories: true,
            },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;

    assert_eq!(
        outcome,
        WalkOutcome {
            entries: vec![
                WalkEntry {
                    path: PathUri::from_host_native_path(hidden)?,
                    kind: WalkEntryKind::Directory,
                },
                WalkEntry {
                    path: PathUri::from_host_native_path(&visible)?,
                    kind: WalkEntryKind::Directory,
                },
                WalkEntry {
                    path: PathUri::from_host_native_path(visible.join("nested"))?,
                    kind: WalkEntryKind::Directory,
                },
                WalkEntry {
                    path: PathUri::from_host_native_path(visible.join("nested/note.txt"))?,
                    kind: WalkEntryKind::File,
                },
            ],
            errors: Vec::new(),
            truncated: false,
        }
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_remove_removes_symlink_not_target(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let allowed_dir = tmp.path().join("allowed");
    let outside_dir = tmp.path().join("outside");
    let outside_file = outside_dir.join("keep.txt");
    std::fs::create_dir_all(&allowed_dir)?;
    std::fs::create_dir_all(&outside_dir)?;
    std::fs::write(&outside_file, "outside")?;
    let symlink_path = allowed_dir.join("link");
    symlink(&outside_file, &symlink_path)?;

    file_system
        .remove(
            &PathUri::from_host_native_path(&symlink_path)?,
            RemoveOptions {
                recursive: false,
                force: false,
                follow_symlinks: true,
            },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;

    assert!(!symlink_path.exists());
    assert!(outside_file.exists());
    assert_eq!(std::fs::read_to_string(outside_file)?, "outside");

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_preserves_symlink_source(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let allowed_dir = tmp.path().join("allowed");
    let outside_dir = tmp.path().join("outside");
    let outside_file = outside_dir.join("outside.txt");
    let source_symlink = allowed_dir.join("link");
    let copied_symlink = allowed_dir.join("copied-link");
    std::fs::create_dir_all(&allowed_dir)?;
    std::fs::create_dir_all(&outside_dir)?;
    std::fs::write(&outside_file, "outside")?;
    symlink(&outside_file, &source_symlink)?;

    file_system
        .copy(
            &PathUri::from_host_native_path(&source_symlink)?,
            &PathUri::from_host_native_path(&copied_symlink)?,
            CopyOptions { recursive: false },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;

    let copied_metadata = std::fs::symlink_metadata(&copied_symlink)?;
    assert!(copied_metadata.file_type().is_symlink());
    assert_eq!(std::fs::read_link(copied_symlink)?, outside_file);

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_preserves_symlinks_in_recursive_copy(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    let nested_dir = source_dir.join("nested");
    let copied_dir = tmp.path().join("copied");
    std::fs::create_dir_all(&nested_dir)?;
    symlink("nested", source_dir.join("nested-link"))?;

    file_system
        .copy(
            &PathUri::from_host_native_path(&source_dir)?,
            &PathUri::from_host_native_path(&copied_dir)?,
            CopyOptions { recursive: true },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;

    let copied_link = copied_dir.join("nested-link");
    let metadata = std::fs::symlink_metadata(&copied_link)?;
    assert!(metadata.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(copied_link)?,
        std::path::PathBuf::from("nested")
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_ignores_unknown_special_files_in_recursive_copy(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    let copied_dir = tmp.path().join("copied");
    std::fs::create_dir_all(&source_dir)?;
    std::fs::write(source_dir.join("note.txt"), "hello")?;

    let fifo_path = source_dir.join("named-pipe");
    let output = Command::new("mkfifo").arg(&fifo_path).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "mkfifo failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    file_system
        .copy(
            &PathUri::from_host_native_path(&source_dir)?,
            &PathUri::from_host_native_path(&copied_dir)?,
            CopyOptions { recursive: true },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;

    assert_eq!(
        std::fs::read_to_string(copied_dir.join("note.txt"))?,
        "hello"
    );
    assert!(!copied_dir.join("named-pipe").exists());

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_rejects_standalone_fifo_source(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let fifo_path = tmp.path().join("named-pipe");
    let output = Command::new("mkfifo").arg(&fifo_path).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "mkfifo failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let error = file_system
        .copy(
            &PathUri::from_host_native_path(&fifo_path)?,
            &PathUri::from_host_native_path(tmp.path().join("copied"))?,
            CopyOptions { recursive: false },
        )
        .await;
    let error = error.expect_err("copying a FIFO should fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "fs/copy only supports regular files, directories, and symlinks"
    );

    Ok(())
}
