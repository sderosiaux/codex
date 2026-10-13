use codex_file_system::MAX_WALK_DEPTH;
use codex_file_system::MAX_WALK_DIRECTORIES;
use codex_file_system::MAX_WALK_ENTRIES;
use codex_file_system::MAX_WALK_RESPONSE_BYTES;
use codex_file_system::WALK_RESPONSE_ITEM_OVERHEAD_BYTES;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::CopyOptions;
use crate::CreateDirectoryOptions;
use crate::ExecutorFileSystem;
use crate::ExecutorFileSystemFuture;
use crate::FILE_READ_CHUNK_SIZE;
use crate::FileMetadata;
use crate::FileSystemReadStream;
use crate::FileSystemResult;
use crate::GetMetadataOptions;
use crate::ReadDirectoryEntry;
use crate::ReadFileOptions;
use crate::RemoveOptions;
use crate::WalkEntry;
use crate::WalkEntryKind;
use crate::WalkError;
use crate::WalkOptions;
use crate::WalkOutcome;
use crate::WriteFileOptions;
use crate::no_follow;
use crate::protocol::FsOpenMode;
use crate::regular_file;

pub static LOCAL_FS: LazyLock<Arc<dyn ExecutorFileSystem>> =
    LazyLock::new(|| -> Arc<dyn ExecutorFileSystem> { Arc::new(LocalFileSystem::new()) });

/// Local filesystem access with the user's own permissions.
#[derive(Clone, Default)]
pub struct LocalFileSystem;

impl LocalFileSystem {
    pub fn new() -> Self {
        Self
    }

    pub(crate) async fn open_file(
        &self,
        path: &PathUri,
        mode: FsOpenMode,
    ) -> FileSystemResult<tokio::fs::File> {
        regular_file::open(path.to_abs_path()?.as_path(), mode).await
    }
}

impl LocalFileSystem {
    async fn open_for_read(&self, path: &PathUri) -> FileSystemResult<tokio::fs::File> {
        let path = path.to_abs_path()?;
        regular_file::open(path.as_path(), FsOpenMode::Read).await
    }

    async fn canonicalize(&self, path: &PathUri) -> FileSystemResult<PathUri> {
        let path = path.to_abs_path()?;
        let canonicalized =
            AbsolutePathBuf::from_absolute_path(tokio::fs::canonicalize(path.as_path()).await?)?;
        Ok(PathUri::from_abs_path(&canonicalized))
    }

    async fn read_file(
        &self,
        path: &PathUri,
        options: ReadFileOptions,
    ) -> FileSystemResult<Vec<u8>> {
        let path = path.to_abs_path()?;
        crate::local_file_system_read::read_file(path, options).await
    }

    async fn read_file_stream(&self, path: &PathUri) -> FileSystemResult<FileSystemReadStream> {
        let file = self.open_for_read(path).await?;
        Ok(FileSystemReadStream::new(ReaderStream::with_capacity(
            file,
            FILE_READ_CHUNK_SIZE,
        )))
    }

    async fn write_file(
        &self,
        path: &PathUri,
        contents: Vec<u8>,
        options: WriteFileOptions,
    ) -> FileSystemResult<()> {
        let path = path.to_abs_path()?;
        if options.follow_symlinks {
            tokio::fs::write(path.as_path(), contents).await
        } else {
            no_follow::write_file(path.as_path(), contents).await
        }
    }

    async fn create_directory(
        &self,
        path: &PathUri,
        options: CreateDirectoryOptions,
    ) -> FileSystemResult<()> {
        let path = path.to_abs_path()?;
        if !options.follow_symlinks {
            return no_follow::create_directory(path.as_path(), options.recursive).await;
        }
        if options.recursive {
            tokio::fs::create_dir_all(path.as_path()).await?;
        } else {
            tokio::fs::create_dir(path.as_path()).await?;
        }
        Ok(())
    }

    async fn get_metadata(
        &self,
        path: &PathUri,
        options: GetMetadataOptions,
    ) -> FileSystemResult<FileMetadata> {
        let path = path.to_abs_path()?;
        if !options.follow_symlinks {
            return no_follow::metadata(path.as_path()).await;
        }
        let symlink_metadata = tokio::fs::symlink_metadata(path.as_path()).await?;
        let is_symlink = symlink_metadata.is_symlink();
        let metadata = if is_symlink {
            tokio::fs::metadata(path.as_path()).await?
        } else {
            symlink_metadata
        };
        Ok(file_metadata(metadata, is_symlink))
    }

    async fn read_directory(&self, path: &PathUri) -> FileSystemResult<Vec<ReadDirectoryEntry>> {
        let path = path.to_abs_path()?;
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(path.as_path()).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let Ok(mut file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_symlink() {
                let Ok(metadata) = tokio::fs::metadata(entry.path()).await else {
                    continue;
                };
                file_type = metadata.file_type();
            }
            entries.push(ReadDirectoryEntry {
                file_name: entry.file_name().to_string_lossy().into_owned(),
                is_directory: file_type.is_dir(),
                is_file: file_type.is_file(),
            });
        }
        Ok(entries)
    }

    fn sync_walk(
        root: &PathUri,
        options: WalkOptions,
        cancelled: &CancellationToken,
    ) -> io::Result<WalkOutcome> {
        if options.max_directories == 0 || options.max_entries == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "filesystem walk limits must be greater than zero",
            ));
        }
        if options.max_depth > MAX_WALK_DEPTH
            || options.max_directories > MAX_WALK_DIRECTORIES
            || options.max_entries > MAX_WALK_ENTRIES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "filesystem walk limits exceed maximums: depth={MAX_WALK_DEPTH}, directories={MAX_WALK_DIRECTORIES}, entries={MAX_WALK_ENTRIES}"
                ),
            ));
        }

        check_walk_cancelled(cancelled)?;
        let (root_metadata, root_is_symlink) = walk_metadata(root)?;
        if !root_metadata.is_dir() || (root_is_symlink && !options.follow_directory_symlinks) {
            return Ok(WalkOutcome::default());
        }

        let root_identity = if options.follow_directory_symlinks {
            check_walk_cancelled(cancelled)?;
            walk_canonicalize(root)?
        } else {
            root.clone()
        };
        let mut outcome = WalkOutcome::default();
        let mut queue = VecDeque::from([(root.clone(), 0usize)]);
        let mut visited_directories = HashSet::from([root_identity]);
        let mut directory_count = 1usize;
        let mut entry_count = 0usize;
        let mut response_bytes = 0usize;

        while let Some((directory, depth)) = queue.pop_front() {
            let entries = walk_read_directory(&directory, cancelled);
            check_walk_cancelled(cancelled)?;
            let mut entries = match entries {
                Ok(entries) => entries,
                Err(error) => {
                    if !push_walk_error(
                        &mut outcome,
                        &mut response_bytes,
                        directory,
                        error.to_string(),
                    ) {
                        return Ok(outcome);
                    }
                    continue;
                }
            };
            entries.sort();

            for file_name in entries {
                check_walk_cancelled(cancelled)?;
                if entry_count == options.max_entries {
                    outcome.truncated = true;
                    return Ok(outcome);
                }
                entry_count += 1;

                let path = match directory.join(&file_name) {
                    Ok(path) => path,
                    Err(error) => {
                        if !push_walk_error(
                            &mut outcome,
                            &mut response_bytes,
                            directory.clone(),
                            error.to_string(),
                        ) {
                            return Ok(outcome);
                        }
                        continue;
                    }
                };
                let (metadata, is_symlink) = match walk_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        if !push_walk_error(
                            &mut outcome,
                            &mut response_bytes,
                            path,
                            error.to_string(),
                        ) {
                            return Ok(outcome);
                        }
                        continue;
                    }
                };
                if is_symlink && (!options.follow_directory_symlinks || !metadata.is_dir()) {
                    continue;
                }

                let kind = if metadata.is_dir() {
                    WalkEntryKind::Directory
                } else if metadata.is_file() {
                    WalkEntryKind::File
                } else {
                    continue;
                };
                if !reserve_walk_response_bytes(
                    &mut outcome,
                    &mut response_bytes,
                    path.to_string().len(),
                ) {
                    return Ok(outcome);
                }
                outcome.entries.push(WalkEntry {
                    path: path.clone(),
                    kind,
                });

                if kind == WalkEntryKind::Directory && depth < options.max_depth {
                    if options.prune_hidden_directories && file_name.starts_with('.') {
                        continue;
                    }
                    let directory_identity = if options.follow_directory_symlinks {
                        check_walk_cancelled(cancelled)?;
                        match walk_canonicalize(&path) {
                            Ok(path) => path,
                            Err(error) => {
                                if !push_walk_error(
                                    &mut outcome,
                                    &mut response_bytes,
                                    path,
                                    error.to_string(),
                                ) {
                                    return Ok(outcome);
                                }
                                continue;
                            }
                        }
                    } else {
                        path.clone()
                    };
                    if !visited_directories.insert(directory_identity) {
                        continue;
                    }
                    if directory_count == options.max_directories {
                        outcome.truncated = true;
                    } else {
                        directory_count += 1;
                        queue.push_back((path, depth + 1));
                    }
                }
            }
        }

        Ok(outcome)
    }

    async fn remove(&self, path: &PathUri, options: RemoveOptions) -> FileSystemResult<()> {
        let path = path.to_abs_path()?;
        if !options.follow_symlinks {
            return no_follow::remove(path.as_path(), options.recursive, options.force).await;
        }
        match tokio::fs::symlink_metadata(path.as_path()).await {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_dir() {
                    if options.recursive {
                        tokio::fs::remove_dir_all(path.as_path()).await?;
                    } else {
                        tokio::fs::remove_dir(path.as_path()).await?;
                    }
                } else {
                    tokio::fs::remove_file(path.as_path()).await?;
                }
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound && options.force => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn copy(
        &self,
        source_path: &PathUri,
        destination_path: &PathUri,
        options: CopyOptions,
    ) -> FileSystemResult<()> {
        let source_path = source_path.to_abs_path()?.into_path_buf();
        let destination_path = destination_path.to_abs_path()?.into_path_buf();
        tokio::task::spawn_blocking(move || -> FileSystemResult<()> {
            let metadata = std::fs::symlink_metadata(source_path.as_path())?;
            let file_type = metadata.file_type();

            if file_type.is_dir() {
                if !options.recursive {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "fs/copy requires recursive: true when sourcePath is a directory",
                    ));
                }
                if destination_is_same_or_descendant_of_source(
                    source_path.as_path(),
                    destination_path.as_path(),
                )? {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "fs/copy cannot copy a directory to itself or one of its descendants",
                    ));
                }
                copy_dir_recursive(source_path.as_path(), destination_path.as_path())?;
                return Ok(());
            }

            if file_type.is_symlink() {
                copy_symlink(source_path.as_path(), destination_path.as_path())?;
                return Ok(());
            }

            if file_type.is_file() {
                std::fs::copy(source_path.as_path(), destination_path.as_path())?;
                return Ok(());
            }

            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fs/copy only supports regular files, directories, and symlinks",
            ))
        })
        .await
        .map_err(|err| io::Error::other(format!("filesystem task failed: {err}")))?
    }
}

impl ExecutorFileSystem for LocalFileSystem {
    fn canonicalize<'a>(&'a self, path: &'a PathUri) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(LocalFileSystem::canonicalize(self, path))
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        options: ReadFileOptions,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(LocalFileSystem::read_file(self, path, options))
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(LocalFileSystem::read_file_stream(self, path))
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        options: WriteFileOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::write_file(self, path, contents, options))
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::create_directory(self, path, options))
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        options: GetMetadataOptions,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(LocalFileSystem::get_metadata(self, path, options))
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(LocalFileSystem::read_directory(self, path))
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Box::pin(async move {
            let path = path.clone();
            let cancelled = CancellationToken::new();
            let _cancel_on_drop = cancelled.clone().drop_guard();
            tokio::task::spawn_blocking(move || Self::sync_walk(&path, options, &cancelled))
                .await
                .map_err(|err| io::Error::other(format!("filesystem task failed: {err}")))?
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::remove(self, path, options))
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(LocalFileSystem::copy(
            self,
            source_path,
            destination_path,
            options,
        ))
    }
}

fn check_walk_cancelled(cancelled: &CancellationToken) -> io::Result<()> {
    if cancelled.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "filesystem walk cancelled",
        ));
    }
    Ok(())
}

fn walk_metadata(path: &PathUri) -> io::Result<(std::fs::Metadata, bool)> {
    let path = path.to_abs_path()?;
    let metadata = std::fs::symlink_metadata(path.as_path())?;
    let is_symlink = metadata.is_symlink();
    let metadata = if is_symlink {
        std::fs::metadata(path.as_path())?
    } else {
        metadata
    };
    Ok((metadata, is_symlink))
}

fn walk_canonicalize(path: &PathUri) -> io::Result<PathUri> {
    let path = path.to_abs_path()?;
    let canonicalized =
        AbsolutePathBuf::from_absolute_path(std::fs::canonicalize(path.as_path())?)?;
    Ok(PathUri::from_abs_path(&canonicalized))
}

fn walk_read_directory(path: &PathUri, cancelled: &CancellationToken) -> io::Result<Vec<String>> {
    check_walk_cancelled(cancelled)?;
    let path = path.to_abs_path()?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path.as_path())? {
        check_walk_cancelled(cancelled)?;
        let entry = entry?;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Match LocalFileSystem::read_directory: omit broken or inaccessible links.
        if file_type.is_symlink() {
            check_walk_cancelled(cancelled)?;
            if std::fs::metadata(entry.path()).is_err() {
                continue;
            }
        }
        entries.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(entries)
}

fn push_walk_error(
    outcome: &mut WalkOutcome,
    response_bytes: &mut usize,
    path: PathUri,
    message: String,
) -> bool {
    let item_bytes = path.to_string().len().saturating_add(message.len());
    if !reserve_walk_response_bytes(outcome, response_bytes, item_bytes) {
        return false;
    }
    outcome.errors.push(WalkError { path, message });
    true
}

fn reserve_walk_response_bytes(
    outcome: &mut WalkOutcome,
    response_bytes: &mut usize,
    content_bytes: usize,
) -> bool {
    let item_bytes = content_bytes.saturating_add(WALK_RESPONSE_ITEM_OVERHEAD_BYTES);
    let Some(total_bytes) = response_bytes.checked_add(item_bytes) else {
        outcome.truncated = true;
        return false;
    };
    if total_bytes > MAX_WALK_RESPONSE_BYTES {
        outcome.truncated = true;
        return false;
    }
    *response_bytes = total_bytes;
    true
}

fn file_metadata(metadata: std::fs::Metadata, is_symlink: bool) -> FileMetadata {
    FileMetadata {
        is_directory: metadata.is_dir(),
        is_file: metadata.is_file(),
        is_symlink,
        size: metadata.len(),
        created_at_ms: metadata.created().ok().map_or(0, system_time_to_unix_ms),
        modified_at_ms: metadata.modified().ok().map_or(0, system_time_to_unix_ms),
    }
}

fn copy_dir_recursive(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            copy_symlink(&source_path, &target_path)?;
        }
    }
    Ok(())
}

fn destination_is_same_or_descendant_of_source(
    source: &Path,
    destination: &Path,
) -> io::Result<bool> {
    let source = std::fs::canonicalize(source)?;
    let destination = resolve_existing_path(destination)?;
    Ok(destination.starts_with(&source))
}

pub(crate) fn resolve_existing_path(path: &Path) -> io::Result<PathBuf> {
    let mut unresolved_suffix = Vec::new();
    let mut existing_path = path;
    while !existing_path.exists() {
        let Some(file_name) = existing_path.file_name() else {
            break;
        };
        unresolved_suffix.push(file_name.to_os_string());
        let Some(parent) = existing_path.parent() else {
            break;
        };
        existing_path = parent;
    }

    let mut resolved = std::fs::canonicalize(existing_path)?;
    for file_name in unresolved_suffix.iter().rev() {
        resolved.push(file_name);
    }
    Ok(resolved)
}

fn copy_symlink(source: &Path, target: &Path) -> io::Result<()> {
    let link_target = std::fs::read_link(source)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&link_target, target)
    }
    #[cfg(windows)]
    {
        if symlink_points_to_directory(source)? {
            std::os::windows::fs::symlink_dir(&link_target, target)
        } else {
            std::os::windows::fs::symlink_file(&link_target, target)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = link_target;
        let _ = target;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "copying symlinks is unsupported on this platform",
        ))
    }
}

#[cfg(windows)]
fn symlink_points_to_directory(source: &Path) -> io::Result<bool> {
    use std::os::windows::fs::FileTypeExt;

    Ok(std::fs::symlink_metadata(source)?
        .file_type()
        .is_symlink_dir())
}

fn system_time_to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(all(test, any(unix, windows)))]
#[path = "local_file_system_path_uri_tests.rs"]
mod path_uri_tests;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::os::unix::fs::symlink;

    #[test]
    fn resolve_existing_path_handles_symlink_parent_dotdot_escape() -> io::Result<()> {
        let temp_dir = tempfile::TempDir::new()?;
        let allowed_dir = temp_dir.path().join("allowed");
        let outside_dir = temp_dir.path().join("outside");
        std::fs::create_dir_all(&allowed_dir)?;
        std::fs::create_dir_all(&outside_dir)?;
        symlink(&outside_dir, allowed_dir.join("link"))?;

        let resolved = resolve_existing_path(
            allowed_dir
                .join("link")
                .join("..")
                .join("secret.txt")
                .as_path(),
        )?;

        assert_eq!(
            resolved,
            resolve_existing_path(temp_dir.path())?.join("secret.txt")
        );
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn symlink_points_to_directory_handles_dangling_directory_symlinks() -> io::Result<()> {
        use std::os::windows::fs::symlink_dir;

        let temp_dir = tempfile::TempDir::new()?;
        let source_dir = temp_dir.path().join("source");
        let link_path = temp_dir.path().join("source-link");
        std::fs::create_dir(&source_dir)?;

        if symlink_dir(&source_dir, &link_path).is_err() {
            return Ok(());
        }

        std::fs::remove_dir(&source_dir)?;

        assert_eq!(symlink_points_to_directory(&link_path)?, true);
        Ok(())
    }
}

#[cfg(test)]
mod walk_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn sync_walk_cancellation_stops_before_io() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let missing = PathUri::from_host_native_path(temp.path().join("missing"))?;
        let options = WalkOptions {
            max_depth: 1,
            max_directories: 1,
            max_entries: 1,
            follow_directory_symlinks: true,
            prune_hidden_directories: false,
        };
        let cancelled = CancellationToken::new();
        let cancel_on_drop = cancelled.clone().drop_guard();
        drop(cancel_on_drop);

        for result in [
            LocalFileSystem::sync_walk(&missing, options, &cancelled).map(|_| ()),
            walk_read_directory(&missing, &cancelled).map(|_| ()),
        ] {
            assert_eq!(
                result
                    .expect_err("cancelled walks must stop before I/O")
                    .kind(),
                io::ErrorKind::Interrupted,
            );
        }
        Ok(())
    }

    #[test]
    fn sync_walk_response_budget_counts_entries_and_errors() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = PathUri::from_host_native_path(temp.path())?;
        let mut outcome = WalkOutcome::default();
        let mut response_bytes =
            MAX_WALK_RESPONSE_BYTES - WALK_RESPONSE_ITEM_OVERHEAD_BYTES - root.to_string().len();
        assert!(push_walk_error(
            &mut outcome,
            &mut response_bytes,
            root.clone(),
            String::new()
        ));
        assert!(!reserve_walk_response_bytes(
            &mut outcome,
            &mut response_bytes,
            /*content_bytes*/ 0
        ));
        assert_eq!(
            outcome,
            WalkOutcome {
                entries: Vec::new(),
                errors: vec![WalkError {
                    path: root,
                    message: String::new()
                }],
                truncated: true,
            },
        );
        Ok(())
    }
}
