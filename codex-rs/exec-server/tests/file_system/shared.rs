use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::EnvironmentAccess;
use codex_exec_server::EnvironmentAccessExt;
#[cfg(unix)]
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FILE_READ_CHUNK_SIZE;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemEnvironmentAccessor;
use codex_exec_server::LocalFileSystem;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::RemoveOptions;
use codex_exec_server::WalkEntry;
use codex_exec_server::WalkEntryKind;
use codex_exec_server::WalkOptions;
use codex_exec_server::WalkOutcome;
use codex_exec_server::WriteFileOptions;
use codex_file_system::MAX_WALK_DEPTH;
use codex_file_system::MAX_WALK_DIRECTORIES;
use codex_file_system::MAX_WALK_ENTRIES;
use codex_utils_path_uri::PathUri;
use futures::TryStreamExt;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use test_case::test_case;

use super::support::FileSystemImplementation;
use super::support::create_file_system_context;

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_get_metadata_reports_files_and_directories(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let file_path = tmp.path().join("note.txt");
    let directory_path = tmp.path().join("notes");
    std::fs::write(&file_path, "hello")?;
    std::fs::create_dir(&directory_path)?;

    let file_metadata = file_system
        .get_metadata(
            &PathUri::from_host_native_path(&file_path)?,
            Default::default(),
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        file_metadata,
        FileMetadata {
            is_directory: false,
            is_file: true,
            is_symlink: false,
            size: 5,
            created_at_ms: file_metadata.created_at_ms,
            modified_at_ms: file_metadata.modified_at_ms,
        }
    );
    assert!(file_metadata.modified_at_ms > 0);

    let directory_metadata = file_system
        .get_metadata(
            &PathUri::from_host_native_path(&directory_path)?,
            Default::default(),
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        directory_metadata,
        FileMetadata {
            is_directory: true,
            is_file: false,
            is_symlink: false,
            size: std::fs::metadata(&directory_path)?.len(),
            created_at_ms: directory_metadata.created_at_ms,
            modified_at_ms: directory_metadata.modified_at_ms,
        }
    );
    assert!(directory_metadata.modified_at_ms > 0);

    Ok(())
}

#[test_case(FileSystemImplementation::Local, true ; "local_follow")]
#[test_case(FileSystemImplementation::Local, false ; "local_no_follow")]
#[test_case(FileSystemImplementation::Remote, true ; "remote_follow")]
#[test_case(FileSystemImplementation::Remote, false ; "remote_no_follow")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_create_directory_creates_nested_directories(
    implementation: FileSystemImplementation,
    follow_symlinks: bool,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let root = tmp.path().canonicalize()?;
    let nested_dir = root.join("source").join("nested");

    let result = file_system
        .create_directory(
            &PathUri::from_host_native_path(&nested_dir)?,
            CreateDirectoryOptions {
                recursive: true,
                follow_symlinks,
            },
        )
        .await;
    result.with_context(|| format!("mode={implementation}"))?;
    assert!(nested_dir.is_dir());

    Ok(())
}

#[test_case(FileSystemImplementation::Local, true ; "local_follow")]
#[test_case(FileSystemImplementation::Local, false ; "local_no_follow")]
#[test_case(FileSystemImplementation::Remote, true ; "remote_follow")]
#[test_case(FileSystemImplementation::Remote, false ; "remote_no_follow")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_write_file_writes_bytes(
    implementation: FileSystemImplementation,
    follow_symlinks: bool,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let root = tmp.path().canonicalize()?;
    let file_path = root.join("note.txt");
    let result = file_system
        .write_file(
            &PathUri::from_host_native_path(&file_path)?,
            b"hello from trait".to_vec(),
            WriteFileOptions { follow_symlinks },
        )
        .await;
    result.with_context(|| format!("mode={implementation}"))?;
    assert_eq!(std::fs::read(file_path)?, b"hello from trait");

    let file_path = root.join("existing.txt");
    std::fs::write(&file_path, b"before")?;
    file_system
        .write_file(
            &PathUri::from_host_native_path(&file_path)?,
            b"after".to_vec(),
            WriteFileOptions { follow_symlinks },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(std::fs::read(file_path)?, b"after");

    Ok(())
}

#[test]
fn path_uri_join_and_parent_preserve_lexical_paths() -> Result<()> {
    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    let source_dir_uri = PathUri::from_host_native_path(&source_dir)?;
    let joined_nested = source_dir_uri.join("nested/note.txt")?;
    assert_eq!(
        joined_nested,
        PathUri::from_host_native_path(source_dir.join("nested").join("note.txt"))?
    );
    let joined_parent = joined_nested.parent();
    assert_eq!(
        joined_parent,
        Some(PathUri::from_host_native_path(source_dir.join("nested"))?)
    );
    let joined_parent_traversal = source_dir_uri.join("../outside")?;
    assert_eq!(
        joined_parent_traversal,
        PathUri::from_host_native_path(source_dir.join("../outside"))?
    );
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_read_file_returns_bytes(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let file_path = tmp.path().join("note.txt");
    std::fs::write(&file_path, "hello from trait")?;

    let contents = file_system
        .read_file(
            &PathUri::from_host_native_path(&file_path)?,
            Default::default(),
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(contents, b"hello from trait");

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_read_file_stream_returns_bounded_chunks(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let file_path = tmp.path().join("blocks.bin");
    let contents = (0..FILE_READ_CHUNK_SIZE * 2 + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    std::fs::write(&file_path, &contents)?;

    let path = PathUri::from_host_native_path(file_path)?;
    let chunks = file_system
        .read_file_stream(&path)
        .await
        .with_context(|| format!("mode={implementation}"))?
        .try_collect::<Vec<_>>()
        .await?;

    assert!(
        chunks
            .iter()
            .all(|chunk| !chunk.is_empty() && chunk.len() <= FILE_READ_CHUNK_SIZE)
    );
    assert_eq!(
        chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>(),
        contents
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_read_file_text_returns_string(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let file_path = tmp.path().join("note.txt");
    std::fs::write(&file_path, "hello from trait")?;
    let file_path = PathUri::from_host_native_path(file_path)?;

    let contents = file_system
        .read_file_text(&file_path, Default::default())
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(contents, "hello from trait");

    let accessor = FileSystemEnvironmentAccessor::new(&file_system);
    let access: &dyn EnvironmentAccess = &accessor;
    assert_eq!(
        access
            .read_file_text(&file_path, Default::default())
            .await?,
        contents
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_copies_file(implementation: FileSystemImplementation) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_file = tmp.path().join("source.txt");
    let copied_file = tmp.path().join("copy.txt");
    std::fs::write(&source_file, "hello from trait")?;

    file_system
        .copy(
            &PathUri::from_host_native_path(&source_file)?,
            &PathUri::from_host_native_path(&copied_file)?,
            CopyOptions { recursive: false },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(std::fs::read_to_string(copied_file)?, "hello from trait");

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_copies_directory_recursively(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    let nested_dir = source_dir.join("nested");
    let nested_file = nested_dir.join("note.txt");
    let copied_dir = tmp.path().join("copied");
    std::fs::create_dir_all(&nested_dir)?;
    std::fs::write(&nested_file, "hello from trait")?;

    file_system
        .copy(
            &PathUri::from_host_native_path(&source_dir)?,
            &PathUri::from_host_native_path(&copied_dir)?,
            CopyOptions { recursive: true },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        std::fs::read_to_string(copied_dir.join("nested").join("note.txt"))?,
        "hello from trait"
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_read_directory_lists_entries(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    std::fs::create_dir_all(source_dir.join("nested"))?;
    std::fs::write(source_dir.join("root.txt"), "hello")?;

    let mut entries = file_system
        .read_directory(&PathUri::from_host_native_path(&source_dir)?)
        .await
        .with_context(|| format!("mode={implementation}"))?;
    entries.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    assert_eq!(
        entries,
        vec![
            ReadDirectoryEntry {
                file_name: "nested".to_string(),
                is_directory: true,
                is_file: false,
            },
            ReadDirectoryEntry {
                file_name: "root.txt".to_string(),
                is_directory: false,
                is_file: true,
            },
        ]
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_walk_returns_a_bounded_tree(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    let nested_dir = source_dir.join("nested");
    std::fs::create_dir_all(&nested_dir)?;
    std::fs::write(source_dir.join("root.txt"), "root")?;
    std::fs::write(nested_dir.join("note.txt"), "nested")?;

    let source_uri = PathUri::from_host_native_path(&source_dir)?;
    let outcome = file_system
        .walk(
            &source_uri,
            WalkOptions {
                max_depth: 4,
                max_directories: 10,
                max_entries: 10,
                follow_directory_symlinks: false,
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
                    path: PathUri::from_host_native_path(&nested_dir)?,
                    kind: WalkEntryKind::Directory,
                },
                WalkEntry {
                    path: PathUri::from_host_native_path(source_dir.join("root.txt"))?,
                    kind: WalkEntryKind::File,
                },
                WalkEntry {
                    path: PathUri::from_host_native_path(nested_dir.join("note.txt"))?,
                    kind: WalkEntryKind::File,
                },
            ],
            errors: Vec::new(),
            truncated: false,
        }
    );

    let root_entries = vec![
        WalkEntry {
            path: PathUri::from_host_native_path(&nested_dir)?,
            kind: WalkEntryKind::Directory,
        },
        WalkEntry {
            path: PathUri::from_host_native_path(source_dir.join("root.txt"))?,
            kind: WalkEntryKind::File,
        },
    ];
    let shallow = file_system
        .walk(
            &source_uri,
            WalkOptions {
                max_depth: 0,
                max_directories: 10,
                max_entries: 10,
                follow_directory_symlinks: false,
                prune_hidden_directories: false,
            },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        shallow,
        WalkOutcome {
            entries: root_entries.clone(),
            errors: Vec::new(),
            truncated: false,
        }
    );

    let directory_bounded = file_system
        .walk(
            &source_uri,
            WalkOptions {
                max_depth: 4,
                max_directories: 1,
                max_entries: 10,
                follow_directory_symlinks: false,
                prune_hidden_directories: false,
            },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        directory_bounded,
        WalkOutcome {
            entries: root_entries,
            errors: Vec::new(),
            truncated: true,
        }
    );

    let bounded = file_system
        .walk(
            &source_uri,
            WalkOptions {
                max_depth: 4,
                max_directories: 10,
                max_entries: 1,
                follow_directory_symlinks: false,
                prune_hidden_directories: false,
            },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(
        bounded,
        WalkOutcome {
            entries: vec![WalkEntry {
                path: PathUri::from_host_native_path(&nested_dir)?,
                kind: WalkEntryKind::Directory,
            }],
            errors: Vec::new(),
            truncated: true,
        }
    );

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_walk_handles_invalid_roots_and_limits(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;
    let tmp = TempDir::new()?;
    let file_path = tmp.path().join("file.txt");
    std::fs::write(&file_path, "contents")?;
    let missing = PathUri::from_host_native_path(tmp.path().join("missing"))?;
    let options = WalkOptions {
        max_depth: 8,
        max_directories: 100,
        max_entries: 100,
        follow_directory_symlinks: false,
        prune_hidden_directories: false,
    };

    let outcome = file_system
        .walk(&PathUri::from_host_native_path(file_path)?, options)
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(outcome, WalkOutcome::default());

    let error = file_system
        .walk(&missing, options)
        .await
        .expect_err("a missing root must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

    let zero_limit_message = "filesystem walk limits must be greater than zero";
    let excessive_limit_message = format!(
        "filesystem walk limits exceed maximums: depth={MAX_WALK_DEPTH}, directories={MAX_WALK_DIRECTORIES}, entries={MAX_WALK_ENTRIES}"
    );
    for (max_depth, max_directories, max_entries, message) in [
        (8, 0, 100, zero_limit_message),
        (8, 100, 0, zero_limit_message),
        (
            MAX_WALK_DEPTH + 1,
            100,
            100,
            excessive_limit_message.as_str(),
        ),
        (
            8,
            MAX_WALK_DIRECTORIES + 1,
            100,
            excessive_limit_message.as_str(),
        ),
        (
            8,
            100,
            MAX_WALK_ENTRIES + 1,
            excessive_limit_message.as_str(),
        ),
    ] {
        let options = WalkOptions {
            max_depth,
            max_directories,
            max_entries,
            ..options
        };
        // Invalid limits take precedence over the missing root.
        let error = file_system
            .walk(&missing, options)
            .await
            .expect_err("invalid walk limits must fail");
        assert_eq!(
            (error.kind(), error.to_string()),
            (std::io::ErrorKind::InvalidInput, message.to_owned()),
            "mode={implementation}, options={options:?}",
        );
    }

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_remove_removes_directory(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let directory_path = tmp.path().join("remove-me");
    std::fs::create_dir_all(directory_path.join("nested"))?;

    file_system
        .remove(
            &PathUri::from_host_native_path(&directory_path)?,
            RemoveOptions {
                recursive: true,
                force: true,
                follow_symlinks: true,
            },
        )
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert!(!directory_path.exists());

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_remove_no_follow_removes_file_and_empty_directory(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;
    let tmp = TempDir::new()?;
    let root = tmp.path().canonicalize()?;
    let options = RemoveOptions {
        recursive: false,
        force: false,
        follow_symlinks: false,
    };

    let file_path = root.join("remove-me.txt");
    std::fs::write(&file_path, b"remove")?;
    let result = file_system
        .remove(&PathUri::from_host_native_path(&file_path)?, options)
        .await;
    result.with_context(|| format!("mode={implementation}"))?;
    assert!(!file_path.exists());

    let directory_path = root.join("remove-me");
    std::fs::create_dir(&directory_path)?;
    file_system
        .remove(&PathUri::from_host_native_path(&directory_path)?, options)
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert!(!directory_path.exists());

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_write_file_reports_missing_parent(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let missing_parent_path = tmp.path().join("missing").join("note.txt");

    let error = match file_system
        .write_file(
            &PathUri::from_host_native_path(&missing_parent_path)?,
            b"hello from trait".to_vec(),
            Default::default(),
        )
        .await
    {
        Ok(()) => anyhow::bail!("write should fail when parent directory is absent"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::NotFound,
        "mode={implementation}"
    );
    assert!(!missing_parent_path.exists(), "mode={implementation}");

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_rejects_directory_without_recursive(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    std::fs::create_dir_all(&source_dir)?;

    let error = file_system
        .copy(
            &PathUri::from_host_native_path(&source_dir)?,
            &PathUri::from_host_native_path(tmp.path().join("dest"))?,
            CopyOptions { recursive: false },
        )
        .await;
    let error = error.expect_err("copying a directory without recursion should fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "fs/copy requires recursive: true when sourcePath is a directory"
    );

    Ok(())
}

/// A stream already opened under one environment remains usable after its accessor is dropped;
/// cache keys still distinguish a new filesystem or changed captured permissions.
#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_accessor_stream_outlives_accessor(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let tmp = TempDir::new()?;
    let file = tmp.path().join("note.txt");
    std::fs::write(&file, b"opened in the previous turn")?;
    let file = PathUri::from_host_native_path(file)?;

    let (stream, key) = {
        let access = FileSystemEnvironmentAccessor::new(&context.file_system);
        (access.read_file_stream(&file).await?, access.cache_key())
    };
    let same = FileSystemEnvironmentAccessor::new(&context.file_system);
    assert_eq!(key, same.cache_key());
    let other_file_system: Arc<dyn ExecutorFileSystem> = Arc::new(LocalFileSystem::new());
    let replacement = FileSystemEnvironmentAccessor::new(&other_file_system);
    assert_ne!(key, replacement.cache_key());
    assert_eq!(
        stream.try_collect::<Vec<_>>().await?.concat(),
        b"opened in the previous turn"
    );
    Ok(())
}

pub(crate) async fn assert_canonicalize_resolves_directory_alias(
    implementation: FileSystemImplementation,
    create_directory_alias: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    let nested_dir = source_dir.join("nested");
    let file_path = nested_dir.join("note.txt");
    let alias_dir = tmp.path().join("source-alias");
    std::fs::create_dir_all(&nested_dir)?;
    std::fs::write(&file_path, "canonical hello")?;
    create_directory_alias(&source_dir, &alias_dir)?;

    let requested_path = PathUri::from_host_native_path(alias_dir.join("nested").join("note.txt"))?;
    let expected_path = PathUri::from_host_native_path(std::fs::canonicalize(&file_path)?)?;
    assert_ne!(requested_path, expected_path);

    let canonical_path = file_system
        .canonicalize(&requested_path)
        .await
        .with_context(|| format!("mode={implementation}"))?;
    assert_eq!(canonical_path, expected_path);

    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_copy_rejects_copying_directory_into_descendant(
    implementation: FileSystemImplementation,
) -> Result<()> {
    let context = create_file_system_context(implementation).await?;
    let file_system = context.file_system;

    let tmp = TempDir::new()?;
    let source_dir = tmp.path().join("source");
    std::fs::create_dir_all(source_dir.join("nested"))?;

    let error = file_system
        .copy(
            &PathUri::from_host_native_path(&source_dir)?,
            &PathUri::from_host_native_path(source_dir.join("nested").join("copy"))?,
            CopyOptions { recursive: true },
        )
        .await;
    let error = error.expect_err("copying a directory into itself should fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "fs/copy cannot copy a directory to itself or one of its descendants"
    );

    Ok(())
}
