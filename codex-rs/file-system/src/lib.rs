mod environment_accessor;
mod find_up;

use bytes::Bytes;
use codex_utils_path_uri::PathUri;
pub use environment_accessor::EnvironmentAccess;
pub use environment_accessor::EnvironmentAccessExt;
pub use environment_accessor::EnvironmentAccessKey;
pub use environment_accessor::FileSystemEnvironmentAccessor;
pub use find_up::FindUpErrorPolicy;
pub use find_up::find_nearest_ancestor_with_markers;
pub use find_up::find_nearest_native_ancestor_with_markers;
use futures::Stream;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// Maximum chunk size returned by [`ExecutorFileSystem::read_file_stream`].
pub const FILE_READ_CHUNK_SIZE: usize = 1024 * 1024;
/// Maximum decoded chunk size accepted by a streamed filesystem write.
pub const FILE_WRITE_CHUNK_SIZE: usize = 1024 * 1024;
/// Maximum accepted directory depth for a filesystem walk.
pub const MAX_WALK_DEPTH: usize = 64;
/// Maximum accepted directory count, including the walk root.
pub const MAX_WALK_DIRECTORIES: usize = 10_000;
/// Maximum accepted number of directory entries to examine.
pub const MAX_WALK_ENTRIES: usize = 50_000;
/// Maximum estimated size of a walk response.
pub const MAX_WALK_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Per-entry or per-error overhead charged to the walk response budget.
pub const WALK_RESPONSE_ITEM_OVERHEAD_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadFileOptions {
    pub follow_symlinks: bool,
}

impl Default for ReadFileOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteFileOptions {
    pub follow_symlinks: bool,
}

impl Default for WriteFileOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GetMetadataOptions {
    pub follow_symlinks: bool,
}

impl Default for GetMetadataOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateDirectoryOptions {
    pub recursive: bool,
    pub follow_symlinks: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoveOptions {
    pub recursive: bool,
    pub force: bool,
    pub follow_symlinks: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyOptions {
    pub recursive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    pub is_directory: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    /// Size in bytes.
    pub size: u64,
    pub created_at_ms: i64,
    pub modified_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadDirectoryEntry {
    pub file_name: String,
    pub is_directory: bool,
    pub is_file: bool,
}

/// Bounds for a recursive filesystem walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkOptions {
    /// Maximum directory depth below the root that may be traversed.
    pub max_depth: usize,
    /// Maximum number of directories that may be traversed, including the root.
    pub max_directories: usize,
    /// Maximum number of directory entries that may be examined.
    pub max_entries: usize,
    /// Whether directory symlinks should be followed.
    pub follow_directory_symlinks: bool,
    /// Whether directories whose names start with `.` should be returned but not traversed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prune_hidden_directories: bool,
}

/// Type of a filesystem entry returned by a walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WalkEntryKind {
    Directory,
    File,
}

/// One entry returned by a walk.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkEntry {
    pub path: PathUri,
    pub kind: WalkEntryKind,
}

/// A descendant that could not be inspected during a walk.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkError {
    pub path: PathUri,
    pub message: String,
}

/// Entries and recoverable errors collected by a bounded walk.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkOutcome {
    pub entries: Vec<WalkEntry>,
    pub errors: Vec<WalkError>,
    pub truncated: bool,
}

pub type FileSystemResult<T> = io::Result<T>;

/// Future returned by [`ExecutorFileSystem`] operations.
pub type ExecutorFileSystemFuture<'a, T> =
    Pin<Box<dyn Future<Output = FileSystemResult<T>> + Send + 'a>>;

/// Stream of immutable chunks read from an [`ExecutorFileSystem`].
pub struct FileSystemReadStream {
    inner: Pin<Box<dyn Stream<Item = FileSystemResult<Bytes>> + Send + 'static>>,
}

impl FileSystemReadStream {
    /// Wraps a filesystem byte stream.
    pub fn new(stream: impl Stream<Item = FileSystemResult<Bytes>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }
}

impl Stream for FileSystemReadStream {
    type Item = FileSystemResult<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Filesystem access used by tools, skills, and AGENTS.md discovery.
pub trait ExecutorFileSystem: Send + Sync {
    /// Resolves a path within this filesystem.
    fn canonicalize<'a>(&'a self, path: &'a PathUri) -> ExecutorFileSystemFuture<'a, PathUri>;

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        options: ReadFileOptions,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>>;

    /// Reads a file as a stream of chunks no larger than [`FILE_READ_CHUNK_SIZE`].
    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream>;

    /// Reads a file and decodes it as UTF-8 text.
    fn read_file_text<'a>(
        &'a self,
        path: &'a PathUri,
        options: ReadFileOptions,
    ) -> ExecutorFileSystemFuture<'a, String> {
        Box::pin(async move {
            let bytes = self.read_file(path, options).await?;
            String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        options: WriteFileOptions,
    ) -> ExecutorFileSystemFuture<'a, ()>;

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        create_directory_options: CreateDirectoryOptions,
    ) -> ExecutorFileSystemFuture<'a, ()>;

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        options: GetMetadataOptions,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata>;

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>>;

    /// Recursively lists descendants, optionally following directory symlinks.
    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome>;

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        remove_options: RemoveOptions,
    ) -> ExecutorFileSystemFuture<'a, ()>;

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        copy_options: CopyOptions,
    ) -> ExecutorFileSystemFuture<'a, ()>;
}
