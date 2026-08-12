//! Capability-based filesystem shared across sandbox backends.
//!
//! Design and permission model (`DirPerms`, `FilePerms`, `Dir` wrapper)
//! inspired by [wasmtime's WASI filesystem implementation][wt].
//!
//! [wt]: https://github.com/bytecodealliance/wasmtime/blob/main/crates/wasi/src/filesystem.rs
//!
//! * **Input** — host-provided directory, exposed to the guest as a read-only
//!   WASI preopen (`DirPerms::READ`, `FilePerms::READ`).  The host populates
//!   the directory before creating the sandbox; the guest can only read.
//!
//! * **Output** — default temp directory or host-provided with explicit
//!   permissions, exposed as a writable WASI preopen.  Wiped clean before
//!   each run.
//!
//! Snapshots only capture runtime state — input is immutable and output is
//! ephemeral, so no filesystem state needs to be saved or restored.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use cap_std::ambient_authority;
use cap_std::fs::Dir as CapDir;

/// First preopen fd (after 0–2 stdio).
const FIRST_PREOPEN_FD: u32 = 3;

// ---------------------------------------------------------------------------
// Filesystem errors
// ---------------------------------------------------------------------------

/// Errors returned by [`CapFs`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    Access,
    BadDescriptor,
    Busy,
    CrossDevice,
    Deadlock,
    Exist,
    FileTooLarge,
    InsufficientMemory,
    InsufficientSpace,
    Interrupted,
    NotPermitted,
    NoEntry,
    NotDirectory,
    IsDirectory,
    NotEmpty,
    NameTooLong,
    Overflow,
    Quota,
    ReadOnly,
    InvalidSeek,
    TextFileBusy,
    TooManyLinks,
    Unsupported,
    WouldBlock,
    InvalidPath,
    SymlinkLoop,
    Io(String),
}

impl FsError {
    /// Preserve the portable part of an operating-system I/O failure so the
    /// WASI adapter can return a specific `error-code` instead of generic I/O.
    fn from_io(error: std::io::Error) -> Self {
        use std::io::ErrorKind;

        match error.kind() {
            ErrorKind::NotFound => Self::NoEntry,
            ErrorKind::PermissionDenied => Self::Access,
            ErrorKind::AlreadyExists => Self::Exist,
            ErrorKind::WouldBlock => Self::WouldBlock,
            ErrorKind::NotADirectory => Self::NotDirectory,
            ErrorKind::IsADirectory => Self::IsDirectory,
            ErrorKind::DirectoryNotEmpty => Self::NotEmpty,
            ErrorKind::ReadOnlyFilesystem => Self::ReadOnly,
            ErrorKind::InvalidInput | ErrorKind::InvalidData => Self::InvalidPath,
            ErrorKind::StorageFull => Self::InsufficientSpace,
            ErrorKind::NotSeekable => Self::InvalidSeek,
            ErrorKind::QuotaExceeded => Self::Quota,
            ErrorKind::FileTooLarge => Self::FileTooLarge,
            ErrorKind::ResourceBusy => Self::Busy,
            ErrorKind::ExecutableFileBusy => Self::TextFileBusy,
            ErrorKind::Deadlock => Self::Deadlock,
            ErrorKind::CrossesDevices => Self::CrossDevice,
            ErrorKind::TooManyLinks => Self::TooManyLinks,
            ErrorKind::InvalidFilename | ErrorKind::ArgumentListTooLong => Self::NameTooLong,
            ErrorKind::Interrupted => Self::Interrupted,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::OutOfMemory => Self::InsufficientMemory,
            _ => Self::Io(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountLifetime {
    Persistent,
    ClearBeforeRun,
}

// ---------------------------------------------------------------------------
// Metadata types
// ---------------------------------------------------------------------------

/// Type of a filesystem descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorType {
    Directory,
    RegularFile,
    SymbolicLink,
}

/// Metadata about a file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorStat {
    pub descriptor_type: DescriptorType,
    pub size: u64,
}

/// Effective permissions reported for a descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorFlags {
    pub read: bool,
    pub write: bool,
    pub mutate_directory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkDirAccess {
    ReadOnly,
    ReadWrite,
}

/// Resource limits enforced by [`CapFs`].
///
/// Live descriptor and stream limits apply for the lifetime of the sandbox.
/// Byte, creation, and directory-entry budgets apply to one `run()` and are
/// reset by [`CapFs::prepare_for_run`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesystemLimits {
    /// Maximum number of live descriptors, including preopens.
    pub max_open_descriptors: usize,
    /// Maximum number of live file and directory streams combined.
    pub max_open_streams: usize,
    /// Maximum allocation made by one filesystem read.
    pub max_single_read_bytes: u64,
    /// Maximum number of bytes returned by filesystem reads in one run.
    pub max_read_bytes_per_run: u64,
    /// Maximum number of bytes accepted by filesystem writes in one run.
    pub max_written_bytes_per_run: u64,
    /// Maximum number of files and directories created in one run.
    pub max_creations_per_run: u64,
    /// Maximum entries materialized by one directory listing.
    pub max_directory_entries_per_listing: usize,
    /// Maximum entries materialized across directory listings in one run.
    pub max_directory_entries_per_run: u64,
    /// Maximum entries inspected while recursively cleaning one mount.
    pub max_cleanup_entries: u64,
    /// Maximum directory depth traversed by recursive cleanup.
    pub max_recursive_depth: usize,
}

impl Default for FilesystemLimits {
    fn default() -> Self {
        Self {
            max_open_descriptors: 1_024,
            max_open_streams: 1_024,
            max_single_read_bytes: 16 * 1024 * 1024,
            max_read_bytes_per_run: 256 * 1024 * 1024,
            max_written_bytes_per_run: 64 * 1024 * 1024,
            max_creations_per_run: 10_000,
            max_directory_entries_per_listing: 10_000,
            max_directory_entries_per_run: 100_000,
            max_cleanup_entries: 100_000,
            max_recursive_depth: 64,
        }
    }
}

// ---------------------------------------------------------------------------
// Permission types (following wasmtime's cap-std pattern)
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DirPerms: u8 {
        const READ = 0b01;
        const MUTATE = 0b10;
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct FilePerms: u8 {
        const READ = 0b01;
        const WRITE = 0b10;
    }
}

bitflags::bitflags! {
    /// Flags controlling how the final component of a guest path is resolved.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PathFlags: u8 {
        /// Follow a symbolic link in the final path component.
        const SYMLINK_FOLLOW = 0b01;
    }
}

bitflags::bitflags! {
    /// Flags controlling how [`CapFs::open_at`] opens a file.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OpenFlags: u8 {
        /// Open an existing file without modification.
        const OPEN_EXISTING = 0;
        /// Create the file if it doesn't exist.
        const CREATE = 0b01;
        /// Truncate the file to zero length.
        const TRUNCATE = 0b10;
        /// Require the path to resolve to a directory.
        const DIRECTORY = 0b100;
    }
}

// ---------------------------------------------------------------------------
// Capability-wrapped directory
// ---------------------------------------------------------------------------

/// A capability-wrapped directory handle with explicit permissions.
#[derive(Clone)]
pub struct Dir {
    dir: Arc<CapDir>,
    perms: DirPerms,
    file_perms: FilePerms,
}

impl Dir {
    pub fn new(dir: CapDir, perms: DirPerms, file_perms: FilePerms) -> Self {
        Self {
            dir: Arc::new(dir),
            perms,
            file_perms,
        }
    }

    pub fn cap_std(&self) -> &CapDir {
        &self.dir
    }

    pub fn perms(&self) -> DirPerms {
        self.perms
    }

    pub fn file_perms(&self) -> FilePerms {
        self.file_perms
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Maximum accepted UTF-8 byte length for a guest-relative path.
const MAX_GUEST_PATH_BYTES: usize = 4096;

/// Maximum number of normalized components in a guest-relative path.
const MAX_GUEST_PATH_COMPONENTS: usize = 256;

/// A validated, normalized path relative to a filesystem capability.
///
/// Values can only be constructed by the lexical policy below or as the
/// internal empty path representing a preopen root. Authorization is still
/// performed by `cap-std`; this type prevents unchecked guest strings from
/// reaching filesystem operations.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GuestRelativePath {
    normalized: String,
    component_count: usize,
    requires_directory: bool,
}

impl GuestRelativePath {
    fn root() -> Self {
        Self {
            normalized: String::new(),
            component_count: 0,
            requires_directory: true,
        }
    }

    fn parse(path: &str) -> Result<Self, FsError> {
        if path.is_empty()
            || path.len() > MAX_GUEST_PATH_BYTES
            || path.starts_with('/')
            || path.contains('\\')
            || path.contains('\0')
        {
            return Err(FsError::InvalidPath);
        }

        let requires_directory = path.ends_with('/')
            || path.rsplit('/').find(|component| !component.is_empty()) == Some(".");
        let mut components = Vec::new();

        for component in path.split('/') {
            match component {
                "" | "." => continue,
                ".." => return Err(FsError::InvalidPath),
                _ => {}
            }

            // Reject Windows drive prefixes even when compiling on Unix so
            // the guest path language has identical cross-platform meaning.
            if components.is_empty() && Self::has_windows_drive_prefix(component) {
                return Err(FsError::InvalidPath);
            }

            components.push(component);
            if components.len() > MAX_GUEST_PATH_COMPONENTS {
                return Err(FsError::InvalidPath);
            }
        }

        if components.is_empty() {
            return Err(FsError::InvalidPath);
        }

        let normalized = components.join("/");
        if normalized.len() > MAX_GUEST_PATH_BYTES {
            return Err(FsError::InvalidPath);
        }

        Ok(Self {
            normalized,
            component_count: components.len(),
            requires_directory,
        })
    }

    fn has_windows_drive_prefix(component: &str) -> bool {
        let bytes = component.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    }

    fn join(&self, child: &Self) -> Result<Self, FsError> {
        let component_count = self
            .component_count
            .checked_add(child.component_count)
            .ok_or(FsError::InvalidPath)?;
        if component_count > MAX_GUEST_PATH_COMPONENTS {
            return Err(FsError::InvalidPath);
        }

        let normalized = if self.normalized.is_empty() {
            child.normalized.clone()
        } else {
            format!("{}/{}", self.normalized, child.normalized)
        };
        if normalized.len() > MAX_GUEST_PATH_BYTES {
            return Err(FsError::InvalidPath);
        }

        Ok(Self {
            normalized,
            component_count,
            requires_directory: child.requires_directory,
        })
    }

    fn as_path(&self) -> &Path {
        Path::new(&self.normalized)
    }

    fn is_within(&self, ancestor: &Self) -> bool {
        self.as_path().strip_prefix(ancestor.as_path()).is_ok()
    }
}

struct DescriptorEntry {
    root_fd: u32,
    relative_path: GuestRelativePath,
    descriptor_type: DescriptorType,
    is_preopen: bool,
    parent_fd: Option<u32>,
    directory: Option<Dir>,
}

struct ResolvedGuestPath {
    root_fd: u32,
    dir_relative: GuestRelativePath,
    root_relative: GuestRelativePath,
}

#[derive(Clone)]
struct StreamState {
    file_fd: u32,
    offset: u64,
    is_write: bool,
}

#[derive(Clone)]
struct DirStreamState {
    dir_fd: u32,
    entries: Vec<(String, bool)>,
    cursor: usize,
}

/// Counters that are reset before each guest invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RunBudget {
    bytes_read: u64,
    bytes_written: u64,
    creations: u64,
    directory_entries: u64,
}

#[derive(Debug, Default)]
struct CleanupBudget {
    entries: u64,
}

// ---------------------------------------------------------------------------
// CapFs
// ---------------------------------------------------------------------------

/// A preopened directory registered with the filesystem.
#[derive(Clone)]
struct PreopenEntry {
    dir: Dir,
    guest_path: String,
    lifetime: MountLifetime,
}

/// Capability-based virtual filesystem.
///
/// Input is read-only and shared across snapshots; output is ephemeral.
pub struct CapFs {
    // Preopened directories keyed by fd.
    preopen_dirs: HashMap<u32, PreopenEntry>,
    // The fd assigned to the output preopen (if configured).
    output_fd: Option<u32>,

    // All filesystem descriptors, including preopens and opened descendants.
    descriptors: HashMap<u32, DescriptorEntry>,
    streams: HashMap<u32, StreamState>,
    dir_streams: HashMap<u32, DirStreamState>,
    next_handle: u32,
    limits: FilesystemLimits,
    run_budget: RunBudget,

    // Host filesystem path to the output directory.
    output_path: Option<PathBuf>,
    // Owns the output temp dir when using the default.
    _output_tmp: Option<tempfile::TempDir>,

    temp_paths: Option<PathBuf>,
    temp_dirs: Option<Vec<tempfile::TempDir>>,
}

impl Default for CapFs {
    fn default() -> Self {
        Self::new()
    }
}

impl CapFs {
    /// Create an empty filesystem with no preopens.
    ///
    /// Use [`with_input`], [`with_temp_output`], or [`with_output_dir`] to add
    /// directories. Without any preopens the guest sees no filesystem.
    pub fn new() -> Self {
        Self {
            preopen_dirs: HashMap::new(),
            output_fd: None,
            descriptors: HashMap::new(),
            streams: HashMap::new(),
            dir_streams: HashMap::new(),
            next_handle: FIRST_PREOPEN_FD,
            limits: FilesystemLimits::default(),
            run_budget: RunBudget::default(),
            output_path: None,
            _output_tmp: None,
            temp_paths: None,
            temp_dirs: None,
        }
    }

    /// Replace the filesystem limits used by this sandbox.
    ///
    /// Existing resources are not evicted when a limit is lowered. New
    /// operations fail with [`FsError::Quota`] until usage is below the limit.
    pub fn with_limits(mut self, limits: FilesystemLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> &FilesystemLimits {
        &self.limits
    }

    /// Add a read-only input directory preopen (`/input`).
    pub fn with_input(mut self, input_path: impl AsRef<Path>) -> Result<Self> {
        let input_cap = CapDir::open_ambient_dir(input_path.as_ref(), ambient_authority())
            .with_context(|| {
                format!(
                    "failed to open input dir: {}",
                    input_path.as_ref().display()
                )
            })?;
        self.register_preopen(
            Dir::new(input_cap, DirPerms::READ, FilePerms::READ),
            "/input",
            MountLifetime::Persistent,
        )
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;
        Ok(self)
    }

    /// Add a writable output directory preopen (`/output`) backed by a temp dir.
    pub fn with_temp_output(mut self) -> Result<Self> {
        let output_tmp = tempfile::tempdir().context("failed to create output temp dir")?;
        let output_cap = CapDir::open_ambient_dir(output_tmp.path(), ambient_authority())
            .context("failed to open output temp dir")?;
        let fd = self
            .register_preopen(
                Dir::new(
                    output_cap,
                    DirPerms::READ | DirPerms::MUTATE,
                    FilePerms::READ | FilePerms::WRITE,
                ),
                "/output",
                MountLifetime::ClearBeforeRun,
            )
            .map_err(|err| anyhow::anyhow!("{err:?}"))?;
        self.output_fd = Some(fd);
        self.output_path = Some(output_tmp.path().to_path_buf());
        self._output_tmp = Some(output_tmp);
        Ok(self)
    }

    /// Add a writable output directory preopen (`/output`) at a caller-provided path.
    pub fn with_output_dir(
        mut self,
        output_path: impl AsRef<Path>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
    ) -> Result<Self> {
        let output_cap = CapDir::open_ambient_dir(output_path.as_ref(), ambient_authority())
            .with_context(|| {
                format!(
                    "failed to open output dir: {}",
                    output_path.as_ref().display()
                )
            })?;
        let fd = self
            .register_preopen(
                Dir::new(output_cap, dir_perms, file_perms),
                "/output",
                MountLifetime::ClearBeforeRun,
            )
            .map_err(|err| anyhow::anyhow!("{err:?}"))?;
        self.output_fd = Some(fd);
        self.output_path = Some(output_path.as_ref().to_path_buf());
        Ok(self)
    }

    pub fn with_work(mut self, work_path: impl AsRef<Path>, access: WorkDirAccess) -> Result<Self> {
        let work_cap = CapDir::open_ambient_dir(work_path.as_ref(), ambient_authority())
            .context("failed to open work dir")?;

        let (dir_perms, file_perms) = match access {
            WorkDirAccess::ReadOnly => (DirPerms::READ, FilePerms::READ),
            WorkDirAccess::ReadWrite => (
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::READ | FilePerms::WRITE,
            ),
        };

        self.register_preopen(
            Dir::new(work_cap, dir_perms, file_perms),
            "/work",
            MountLifetime::Persistent,
        )
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;

        Ok(self)
    }

    pub fn with_temp_dir(mut self) -> Result<Self> {
        let temp_dir = tempfile::tempdir().context("failed to create temp dir")?;

        let temp_cap = CapDir::open_ambient_dir(temp_dir.path(), ambient_authority())
            .context("failed to open temp dir")?;

        self.register_preopen(
            Dir::new(
                temp_cap,
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::READ | FilePerms::WRITE,
            ),
            "/tmp",
            MountLifetime::ClearBeforeRun,
        )
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;
        self.temp_paths = Some(temp_dir.path().to_path_buf());
        self.temp_dirs = Some(vec![temp_dir]);
        Ok(self)
    }

    // -----------------------------------------------------------------------
    // Host-side output API
    // -----------------------------------------------------------------------

    pub fn write_output_path(&mut self, path: &str, data: Vec<u8>) -> Result<()> {
        let key = Self::parse_rooted_guest_path(path, "output")?;
        if key.requires_directory {
            anyhow::bail!("output path must name a file: {path}");
        }
        let output_fd = self
            .output_fd
            .ok_or_else(|| anyhow::anyhow!("no output directory configured"))?;
        let output = self.preopen_dirs[&output_fd].dir.clone();
        if !output.perms().contains(DirPerms::MUTATE) {
            anyhow::bail!("write permission denied on output directory");
        }
        if !output.file_perms().contains(FilePerms::WRITE) {
            anyhow::bail!("write permission denied on output files");
        }
        let creates_file = match output.cap_std().symlink_metadata(key.as_path()) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to stat output file: {path}"));
            }
        };
        if creates_file {
            self.charge_creation().map_err(|error| {
                anyhow::anyhow!("filesystem creation quota exceeded: {error:?}")
            })?;
        }
        self.charge_written_bytes(data.len() as u64)
            .map_err(|error| anyhow::anyhow!("filesystem write quota exceeded: {error:?}"))?;
        let mut file = output
            .cap_std()
            .create(key.as_path())
            .with_context(|| format!("failed to create output file: {path}"))?;
        file.write_all(&data)
            .with_context(|| format!("failed to write output file: {path}"))?;
        Ok(())
    }

    /// Read a file from a preopened directory by guest path (e.g. "/input/data.txt").
    pub fn read_guest_file(&mut self, guest_path: &str) -> Result<Vec<u8>> {
        // Determine which preopen and validated relative path from the path.
        let without_leading = guest_path
            .strip_prefix('/')
            .ok_or_else(|| anyhow::anyhow!("guest path must be absolute: {guest_path}"))?;
        let (dir_name, relative) = without_leading.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("path must include a directory and filename: {guest_path}")
        })?;
        let file_name = GuestRelativePath::parse(relative)
            .map_err(|_| anyhow::anyhow!("invalid guest path: {guest_path}"))?;
        if file_name.requires_directory {
            anyhow::bail!("guest path must name a file: {guest_path}");
        }
        let preopen_path = format!("/{dir_name}");
        let dir = self
            .dir_by_guest_path(&preopen_path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no preopen for {preopen_path}"))?;
        if !dir.perms().contains(DirPerms::READ) {
            anyhow::bail!("read permission denied on {preopen_path}");
        }
        if !dir.file_perms().contains(FilePerms::READ) {
            anyhow::bail!("read permission denied on files in {preopen_path}");
        }
        let file = dir
            .cap_std()
            .open(file_name.as_path())
            .map_err(|_| anyhow::anyhow!("file not found: {guest_path}"))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to stat: {guest_path}"))?;
        let single_read_limit = self.limits.max_single_read_bytes.min(usize::MAX as u64);
        if metadata.len() > single_read_limit {
            anyhow::bail!(
                "file too large ({} bytes, max {} bytes): {guest_path}",
                metadata.len(),
                single_read_limit,
            );
        }
        self.charge_read_bytes(metadata.len())
            .map_err(|error| anyhow::anyhow!("filesystem read quota exceeded: {error:?}"))?;
        let mut data = Vec::new();
        // Bound the actual read to the size that was charged. If a host
        // process grows the file concurrently, the extra bytes belong to a
        // later read/run rather than bypassing the per-run byte budget.
        file.take(metadata.len())
            .read_to_end(&mut data)
            .with_context(|| format!("failed to read: {guest_path}"))?;
        Ok(data)
    }

    /// Return the host filesystem path of the output directory, if configured.
    pub fn output_path(&self) -> Option<&Path> {
        self.output_path.as_deref()
    }

    /// Maximum number of output files returned by `get_output_files()`.
    const MAX_OUTPUT_FILE_COUNT: usize = 10_000;

    /// List filenames in the output directory (without reading file contents).
    pub fn get_output_files(&self) -> Vec<String> {
        let mut result = Vec::new();
        let Some(output_fd) = self.output_fd else {
            return result;
        };
        let Some(output) = self.preopen_dirs.get(&output_fd) else {
            return result;
        };
        let dir = output.dir.cap_std();
        if let Ok(entries) = dir.entries() {
            for entry in entries.flatten() {
                if result.len() >= Self::MAX_OUTPUT_FILE_COUNT {
                    log::warn!(
                        "get_output_files: truncated at {} entries",
                        Self::MAX_OUTPUT_FILE_COUNT
                    );
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if meta.is_file() {
                    result.push(name);
                }
            }
        }
        result
    }

    /// Recursively clear the ephemeral output mount.
    ///
    /// The preopen root is preserved. Nested directories, regular files, and
    /// symlinks are removed without following directory symlinks. Failures are
    /// returned so callers never execute a new run against dirty output state.
    pub fn clear_ephemeral_mounts(&mut self) -> Result<(), FsError> {
        let mount_fds: Vec<u32> = self
            .preopen_dirs
            .iter()
            .filter_map(|(&fd, entry)| {
                (entry.lifetime == MountLifetime::ClearBeforeRun).then_some(fd)
            })
            .collect();
        for fd in mount_fds {
            self.clear_mount(fd).map_err(|error| {
                log::error!("failed to clear ephemeral mount {fd}: {error:?}");
                error
            })?
        }
        Ok(())
    }

    /// Recursively clear an ephemeral mount while preserving its preopen root.
    ///
    /// At present only `/output` is ephemeral. `/input` and any other
    /// persistent preopen are rejected even if they grant mutation rights.
    pub fn clear_mount(&mut self, mount_fd: u32) -> Result<(), FsError> {
        // if self.output_fd != Some(mount_fd) {
        //     return Err(FsError::NotPermitted);
        // }
        //get the preopen dirs
        let entry = self
            .preopen_dirs
            .get(&mount_fd)
            .ok_or(FsError::BadDescriptor)?;
        if entry.lifetime == MountLifetime::Persistent {
            return Err(FsError::NotPermitted);
        }
        let root = entry.dir.clone();

        let mut cleanup_budget = CleanupBudget::default();
        let cleanup_result =
            Self::clear_directory_recursive(root.cap_std(), 0, &self.limits, &mut cleanup_budget);

        // Cleanup may have partially changed the namespace before an I/O or
        // bound failure. Invalidate every path-backed handle under the mount
        // in either case so none can silently retarget a different object.
        let invalidated = self
            .descriptors
            .iter()
            .filter_map(|(&fd, entry)| {
                (!entry.is_preopen && entry.root_fd == mount_fd).then_some(fd)
            })
            .collect::<HashSet<_>>();
        self.invalidate_descriptors(&invalidated);
        self.recalculate_next_handle()?;

        cleanup_result
    }

    /// Establish clean ephemeral state and reset per-run quota accounting.
    pub fn prepare_for_run(&mut self) -> Result<(), FsError> {
        self.clear_ephemeral_mounts()?;
        self.run_budget = RunBudget::default();
        Ok(())
    }

    /// Clear output directory. Input is host-managed and left untouched.
    pub fn clear(&mut self) -> Result<(), FsError> {
        self.clear_ephemeral_mounts()
    }

    // -----------------------------------------------------------------------
    // Descriptor queries
    // -----------------------------------------------------------------------

    pub fn get_dir(&self, fd: u32) -> Option<&Dir> {
        self.descriptors.get(&fd)?.directory.as_ref()
    }

    pub fn is_directory(&self, fd: u32) -> bool {
        self.descriptors
            .get(&fd)
            .is_some_and(|e| e.descriptor_type == DescriptorType::Directory)
    }

    /// Find a preopened directory by its guest-visible path.
    pub fn dir_by_guest_path(&self, guest_path: &str) -> Option<&Dir> {
        self.preopen_dirs
            .values()
            .find(|e| e.guest_path == guest_path)
            .map(|e| &e.dir)
    }

    pub fn is_file(&self, fd: u32) -> bool {
        self.descriptors
            .get(&fd)
            .is_some_and(|e| e.descriptor_type == DescriptorType::RegularFile)
    }

    pub fn file_size(&self, fd: u32) -> Option<u64> {
        let entry = self.descriptors.get(&fd)?;
        if entry.descriptor_type != DescriptorType::RegularFile {
            return None;
        }
        let dir = self.get_dir(entry.root_fd)?;
        Some(
            dir.cap_std()
                .metadata(entry.relative_path.as_path())
                .ok()?
                .len(),
        )
    }

    pub fn find_file_in_dir(&self, dir_fd: u32, name: &str) -> Option<u32> {
        for (&fd, entry) in &self.descriptors {
            if entry.descriptor_type != DescriptorType::RegularFile {
                continue;
            }
            if entry.parent_fd == Some(dir_fd)
                && entry
                    .relative_path
                    .as_path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    == Some(name)
            {
                return Some(fd);
            }
        }
        None
    }

    /// Return the parent directory fd for an open file.
    pub fn file_dir_fd(&self, fd: u32) -> Option<u32> {
        self.descriptors
            .get(&fd)
            .filter(|e| e.descriptor_type == DescriptorType::RegularFile)
            .and_then(|e| e.parent_fd)
    }

    /// Check if a file's parent directory grants the given file permissions.
    pub fn file_has_perms(&self, fd: u32, perms: FilePerms) -> bool {
        self.descriptors
            .get(&fd)
            .filter(|e| e.descriptor_type == DescriptorType::RegularFile)
            .and_then(|e| self.get_dir(e.root_fd))
            .is_some_and(|dir| dir.file_perms().contains(perms))
    }

    /// Get the type of a descriptor (directory or file).
    pub fn get_type(&self, fd: u32) -> Result<DescriptorType, FsError> {
        if self.is_directory(fd) {
            Ok(DescriptorType::Directory)
        } else if self.is_file(fd) {
            Ok(DescriptorType::RegularFile)
        } else {
            Err(FsError::BadDescriptor)
        }
    }

    /// Get metadata for an open descriptor.
    pub fn stat(&self, fd: u32) -> Result<DescriptorStat, FsError> {
        if self.is_directory(fd) {
            return Ok(DescriptorStat {
                descriptor_type: DescriptorType::Directory,
                size: 0,
            });
        }
        if let Some(size) = self.file_size(fd) {
            return Ok(DescriptorStat {
                descriptor_type: DescriptorType::RegularFile,
                size,
            });
        }
        Err(FsError::BadDescriptor)
    }

    /// Get metadata for a path relative to a directory descriptor.
    pub fn stat_at(&self, dir_fd: u32, path: &str) -> Result<DescriptorStat, FsError> {
        self.stat_at_with_path_flags(dir_fd, path, PathFlags::SYMLINK_FOLLOW)
    }

    /// Get metadata for a validated path relative to a directory descriptor.
    pub fn stat_at_with_path_flags(
        &self,
        dir_fd: u32,
        path: &str,
        path_flags: PathFlags,
    ) -> Result<DescriptorStat, FsError> {
        let resolved = self.resolve_at(dir_fd, path)?;
        let dir = self.get_dir(dir_fd).ok_or(FsError::BadDescriptor)?;
        if !dir.perms().contains(DirPerms::READ) {
            return Err(FsError::NotPermitted);
        }
        let metadata = if path_flags.contains(PathFlags::SYMLINK_FOLLOW) {
            dir.cap_std().metadata(resolved.dir_relative.as_path())
        } else {
            dir.cap_std()
                .symlink_metadata(resolved.dir_relative.as_path())
        }
        .map_err(FsError::from_io)?;

        if resolved.dir_relative.requires_directory && !metadata.is_dir() {
            return Err(FsError::InvalidPath);
        }

        let descriptor_type = if metadata.is_dir() {
            DescriptorType::Directory
        } else if metadata.file_type().is_symlink() {
            DescriptorType::SymbolicLink
        } else {
            DescriptorType::RegularFile
        };
        Ok(DescriptorStat {
            descriptor_type,
            size: metadata.len(),
        })
    }

    /// Get the effective permission flags for a descriptor.
    pub fn get_flags(&self, fd: u32) -> Result<DescriptorFlags, FsError> {
        if self.is_directory(fd) {
            let dir = self.get_dir(fd).ok_or(FsError::BadDescriptor)?;
            Ok(DescriptorFlags {
                read: dir.perms().contains(DirPerms::READ),
                write: dir.perms().contains(DirPerms::MUTATE),
                mutate_directory: dir.perms().contains(DirPerms::MUTATE),
            })
        } else if self.is_file(fd) {
            let entry = self.descriptors.get(&fd).ok_or(FsError::BadDescriptor)?;
            let dir = self.root_dir(entry.root_fd).ok_or(FsError::BadDescriptor)?;
            Ok(DescriptorFlags {
                read: dir.file_perms().contains(FilePerms::READ),
                write: dir.file_perms().contains(FilePerms::WRITE),
                mutate_directory: false,
            })
        } else {
            Err(FsError::BadDescriptor)
        }
    }

    // -----------------------------------------------------------------------
    // File operations (cap-std backed)
    // -----------------------------------------------------------------------

    pub fn open_at(&mut self, dir_fd: u32, path: &str, flags: OpenFlags) -> Result<u32, FsError> {
        self.open_at_with_path_flags(dir_fd, path, PathFlags::SYMLINK_FOLLOW, flags)
    }

    /// Open a validated path relative to a directory descriptor.
    pub fn open_at_with_path_flags(
        &mut self,
        dir_fd: u32,
        path: &str,
        path_flags: PathFlags,
        flags: OpenFlags,
    ) -> Result<u32, FsError> {
        let resolved = self.resolve_at(dir_fd, path)?;
        let dir = self
            .get_dir(dir_fd)
            .cloned()
            .ok_or(FsError::BadDescriptor)?;
        let dir_relative = resolved.dir_relative.as_path();

        let create = flags.contains(OpenFlags::CREATE);
        let truncate = flags.contains(OpenFlags::TRUNCATE);
        let require_directory =
            flags.contains(OpenFlags::DIRECTORY) || resolved.dir_relative.requires_directory;

        if (create || truncate) && !dir.perms().contains(DirPerms::MUTATE) {
            return Err(FsError::NotPermitted);
        }

        let follow_symlinks = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        let metadata_result = if follow_symlinks {
            dir.cap_std().metadata(dir_relative)
        } else {
            dir.cap_std().symlink_metadata(dir_relative)
        };
        let metadata = match metadata_result {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(FsError::from_io(error)),
        };
        let exists = metadata.is_some();
        if !exists && !create {
            return Err(FsError::NoEntry);
        }
        if !follow_symlinks
            && metadata
                .as_ref()
                .is_some_and(|meta| meta.file_type().is_symlink())
        {
            return Err(FsError::SymlinkLoop);
        }
        if require_directory && !metadata.as_ref().is_some_and(|meta| meta.is_dir()) {
            return Err(FsError::NoEntry);
        }
        if truncate && metadata.as_ref().is_some_and(|meta| meta.is_dir()) {
            return Err(FsError::InvalidPath);
        }
        self.ensure_descriptor_capacity()?;
        if !exists || truncate {
            if !exists {
                self.charge_creation()?;
            }
            dir.cap_std()
                .create(dir_relative)
                .map_err(FsError::from_io)?;
        }

        let metadata = if follow_symlinks {
            dir.cap_std().metadata(dir_relative)
        } else {
            dir.cap_std().symlink_metadata(dir_relative)
        }
        .map_err(FsError::from_io)?;
        if !follow_symlinks && metadata.file_type().is_symlink() {
            return Err(FsError::SymlinkLoop);
        }
        let descriptor_type = if metadata.is_dir() {
            DescriptorType::Directory
        } else {
            DescriptorType::RegularFile
        };
        let directory = if descriptor_type == DescriptorType::Directory {
            let opened = dir
                .cap_std()
                .open_dir(dir_relative)
                .map_err(FsError::from_io)?;
            Some(Dir::new(opened, dir.perms(), dir.file_perms()))
        } else {
            None
        };

        let fd = self.alloc_handle()?;
        self.descriptors.insert(
            fd,
            DescriptorEntry {
                root_fd: resolved.root_fd,
                relative_path: resolved.root_relative,
                descriptor_type,
                is_preopen: false,
                parent_fd: Some(dir_fd),
                directory,
            },
        );
        Ok(fd)
    }

    /// Create one directory relative to an opened directory descriptor.
    ///
    /// This is intentionally non-recursive: every parent component must
    /// already exist, matching WASI `create-directory-at` semantics.
    pub fn create_directory_at(&mut self, dir_fd: u32, path: &str) -> Result<(), FsError> {
        let resolved = self.resolve_at(dir_fd, path)?;
        let dir = self.mutating_dir(dir_fd)?;
        self.charge_creation()?;
        dir.cap_std()
            .create_dir(resolved.dir_relative.as_path())
            .map_err(FsError::from_io)
    }

    /// Remove an empty directory relative to an opened directory descriptor.
    pub fn remove_directory_at(&mut self, dir_fd: u32, path: &str) -> Result<(), FsError> {
        let resolved = self.resolve_at(dir_fd, path)?;
        let dir = self.mutating_dir(dir_fd)?;
        dir.cap_std()
            .remove_dir(resolved.dir_relative.as_path())
            .map_err(FsError::from_io)?;

        let invalidated = self.descriptors_in_subtree(resolved.root_fd, &resolved.root_relative);
        self.invalidate_descriptors(&invalidated);
        Ok(())
    }

    /// Unlink a non-directory entry without following the final symlink.
    pub fn unlink_file_at(&mut self, dir_fd: u32, path: &str) -> Result<(), FsError> {
        let resolved = self.resolve_at(dir_fd, path)?;
        let dir = self.mutating_dir(dir_fd)?;

        // Normalization removes a trailing slash, so retain its directory
        // requirement explicitly instead of accidentally unlinking `file/`.
        if resolved.dir_relative.requires_directory {
            let metadata = dir
                .cap_std()
                .symlink_metadata(resolved.dir_relative.as_path())
                .map_err(FsError::from_io)?;
            return if metadata.is_dir() {
                Err(FsError::IsDirectory)
            } else {
                Err(FsError::NotDirectory)
            };
        }

        dir.cap_std()
            .remove_file(resolved.dir_relative.as_path())
            .map_err(FsError::from_io)?;

        let invalidated = self.descriptors_in_subtree(resolved.root_fd, &resolved.root_relative);
        self.invalidate_descriptors(&invalidated);
        Ok(())
    }

    /// Rename a file, symlink, or directory between two opened directories.
    ///
    /// Both descriptors must grant directory mutation rights. Renames may
    /// cross opened directory descriptors within one preopen, but never cross
    /// preopen capability roots. This avoids transferring path-backed open
    /// descriptors into a capability with different rights.
    pub fn rename_at(
        &mut self,
        old_dir_fd: u32,
        old_path: &str,
        new_dir_fd: u32,
        new_path: &str,
    ) -> Result<(), FsError> {
        let old_resolved = self.resolve_at(old_dir_fd, old_path)?;
        let new_resolved = self.resolve_at(new_dir_fd, new_path)?;
        let old_dir = self.mutating_dir(old_dir_fd)?;
        let new_dir = self.mutating_dir(new_dir_fd)?;

        if old_resolved.root_fd != new_resolved.root_fd {
            return Err(FsError::CrossDevice);
        }

        let old_metadata = old_dir
            .cap_std()
            .symlink_metadata(old_resolved.dir_relative.as_path())
            .map_err(FsError::from_io)?;
        if old_resolved.dir_relative.requires_directory && !old_metadata.is_dir() {
            return Err(FsError::NotDirectory);
        }
        if new_resolved.dir_relative.requires_directory {
            let new_metadata = new_dir
                .cap_std()
                .symlink_metadata(new_resolved.dir_relative.as_path())
                .map_err(FsError::from_io)?;
            if !new_metadata.is_dir() {
                return Err(FsError::NotDirectory);
            }
        }

        let same_path =
            old_resolved.root_relative.as_path() == new_resolved.root_relative.as_path();
        let moved = self.descriptors_in_subtree(old_resolved.root_fd, &old_resolved.root_relative);
        let replaced =
            self.descriptors_in_subtree(new_resolved.root_fd, &new_resolved.root_relative);

        old_dir
            .cap_std()
            .rename(
                old_resolved.dir_relative.as_path(),
                new_dir.cap_std(),
                new_resolved.dir_relative.as_path(),
            )
            .map_err(FsError::from_io)?;

        if same_path {
            return Ok(());
        }

        // CapFs file descriptors are path-backed rather than open host file
        // handles. Keeping them alive after a namespace mutation could make a
        // descriptor refer to a different object, particularly when the tree
        // contains relative symlinks or pre-existing hard links. Use one
        // simple documented policy: a successful rename invalidates every
        // non-preopen descriptor and stream rooted at either the old or the
        // replaced destination path. The guest must reopen the new path.
        let invalidated = moved.union(&replaced).copied().collect::<HashSet<_>>();
        self.invalidate_descriptors(&invalidated);
        Ok(())
    }

    /// Close an opened descendant descriptor and any streams derived from it.
    ///
    /// Preopen descriptors are owned by the filesystem and cannot be closed
    /// through this method. Descendants of a closed directory remain valid:
    /// each descriptor retains its own root-relative identity/capability.
    pub fn close_descriptor(&mut self, fd: u32) -> Result<(), FsError> {
        let entry = self.descriptors.get(&fd).ok_or(FsError::BadDescriptor)?;
        if entry.is_preopen {
            return Err(FsError::NotPermitted);
        }
        self.descriptors.remove(&fd);
        // When closing a descriptor (fd), also remove any associated file streams and directory streams.
        // This ensures that any open file or directory streams that reference the closed descriptor
        // are also closed and removed from their respective maps.
        // Retains only the elements specified by the predicate.
        self.streams.retain(|_, stream| stream.file_fd != fd);
        self.dir_streams.retain(|_, stream| stream.dir_fd != fd);
        Ok(())
    }

    /// Close an open file or directory handle, freeing the descriptor.
    pub fn close_file(&mut self, fd: u32) {
        if self.is_file(fd) {
            let _ = self.close_descriptor(fd);
        }
    }

    /// Close a stream handle, freeing the descriptor.
    pub fn close_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
    }

    /// Close a directory stream handle, freeing the descriptor.
    pub fn close_dir_stream(&mut self, stream_id: u32) {
        self.dir_streams.remove(&stream_id);
    }

    pub fn read_file(
        &mut self,
        fd: u32,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<u8>, bool), FsError> {
        let (root, path) = {
            let (root, path) = self.file_parts(fd)?;
            (root.clone(), path.to_path_buf())
        };
        if !root.file_perms().contains(FilePerms::READ) {
            return Err(FsError::NotPermitted);
        }
        let mut file = root.cap_std().open(&path).map_err(FsError::from_io)?;
        let file_size = file.metadata().map_err(FsError::from_io)?.len();

        let start = offset.min(file_size);
        let remaining = file_size - start;
        let to_read = len
            .min(remaining)
            .min(self.limits.max_single_read_bytes)
            .min(usize::MAX as u64) as usize;
        if to_read == 0 {
            return Ok((Vec::new(), true));
        }
        self.charge_read_bytes(to_read as u64)?;
        file.seek(SeekFrom::Start(start))
            .map_err(FsError::from_io)?;
        let mut buf = vec![0u8; to_read];
        let n = file.read(&mut buf).map_err(FsError::from_io)?;
        buf.truncate(n);
        let eof = start + n as u64 >= file_size;
        Ok((buf, eof))
    }

    pub fn write_file(&mut self, fd: u32, offset: u64, buffer: &[u8]) -> Result<u64, FsError> {
        let (root, path) = {
            let (root, path) = self.file_parts(fd)?;
            (root.clone(), path.to_path_buf())
        };
        if !root.file_perms().contains(FilePerms::WRITE) {
            return Err(FsError::NotPermitted);
        }
        self.charge_written_bytes(buffer.len() as u64)?;
        let mut opts = cap_std::fs::OpenOptions::new();
        opts.read(true).write(true);
        let mut file = root
            .cap_std()
            .open_with(&path, &opts)
            .map_err(FsError::from_io)?;

        let file_size = file.metadata().map_err(FsError::from_io)?.len();
        let new_end = offset
            .checked_add(buffer.len() as u64)
            .ok_or(FsError::Overflow)?;
        if new_end > file_size {
            file.set_len(new_end).map_err(FsError::from_io)?;
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(FsError::from_io)?;
        file.write_all(buffer).map_err(FsError::from_io)?;
        Ok(buffer.len() as u64)
    }

    // -----------------------------------------------------------------------
    // Streams
    // -----------------------------------------------------------------------

    pub fn create_read_stream(&mut self, file_fd: u32, offset: u64) -> Result<u32, FsError> {
        if !self.is_file(file_fd) {
            return Err(FsError::BadDescriptor);
        }
        if !self.file_has_perms(file_fd, FilePerms::READ) {
            return Err(FsError::NotPermitted);
        }
        self.ensure_stream_capacity()?;
        let id = self.alloc_handle()?;
        self.streams.insert(
            id,
            StreamState {
                file_fd,
                offset,
                is_write: false,
            },
        );
        Ok(id)
    }

    pub fn create_write_stream(&mut self, file_fd: u32, offset: u64) -> Result<u32, FsError> {
        if !self.is_file(file_fd) {
            return Err(FsError::BadDescriptor);
        }
        if !self.file_has_perms(file_fd, FilePerms::WRITE) {
            return Err(FsError::NotPermitted);
        }
        self.ensure_stream_capacity()?;
        let id = self.alloc_handle()?;
        self.streams.insert(
            id,
            StreamState {
                file_fd,
                offset,
                is_write: true,
            },
        );
        Ok(id)
    }

    pub fn create_append_stream(&mut self, file_fd: u32) -> Result<u32, FsError> {
        let size = self.file_size(file_fd).ok_or(FsError::BadDescriptor)?;
        self.create_write_stream(file_fd, size)
    }

    pub fn stream_read(&mut self, stream_id: u32, len: u64) -> Result<Vec<u8>, FsError> {
        let stream = self.streams.get(&stream_id).ok_or(FsError::BadDescriptor)?;
        let file_fd = stream.file_fd;
        let offset = stream.offset;

        let (root, path) = {
            let (root, path) = self.file_parts(file_fd)?;
            (root.clone(), path.to_path_buf())
        };
        let mut file = root.cap_std().open(&path).map_err(FsError::from_io)?;
        let file_size = file.metadata().map_err(FsError::from_io)?.len();
        if offset >= file_size {
            return Err(FsError::Io("stream read past end of file".into()));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(FsError::from_io)?;
        let remaining = file_size - offset;
        let to_read = len
            .min(remaining)
            .min(self.limits.max_single_read_bytes)
            .min(usize::MAX as u64) as usize;
        self.charge_read_bytes(to_read as u64)?;
        let mut buf = vec![0u8; to_read];
        let n = file.read(&mut buf).map_err(FsError::from_io)?;
        buf.truncate(n);

        if buf.is_empty() {
            return Err(FsError::Io("stream read returned no data".into()));
        }
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(FsError::BadDescriptor)?;
        stream.offset = stream.offset.saturating_add(buf.len() as u64);
        Ok(buf)
    }

    pub fn stream_write(&mut self, stream_id: u32, buffer: &[u8]) -> Result<u64, FsError> {
        let stream = self.streams.get(&stream_id).ok_or(FsError::BadDescriptor)?;
        let file_fd = stream.file_fd;
        let offset = stream.offset;
        let written = self.write_file(file_fd, offset, buffer)?;
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(FsError::BadDescriptor)?;
        stream.offset = stream.offset.saturating_add(written);
        Ok(written)
    }

    pub fn has_stream(&self, id: u32) -> bool {
        self.streams.contains_key(&id)
    }
    pub fn is_write_stream(&self, id: u32) -> bool {
        self.streams.get(&id).is_some_and(|s| s.is_write)
    }

    // -----------------------------------------------------------------------
    // Directory streams
    // -----------------------------------------------------------------------

    pub fn create_dir_stream(&mut self, dir_fd: u32) -> Result<u32, FsError> {
        let dir = self
            .get_dir(dir_fd)
            .cloned()
            .ok_or(FsError::BadDescriptor)?;
        if !dir.perms().contains(DirPerms::READ) {
            return Err(FsError::NotPermitted);
        }
        self.ensure_stream_capacity()?;
        let mut file_entries = Vec::new();
        let entries = dir.cap_std().entries().map_err(FsError::from_io)?;
        for entry in entries {
            let entry = entry.map_err(FsError::from_io)?;
            if file_entries.len() >= self.limits.max_directory_entries_per_listing {
                return Err(FsError::Quota);
            }
            self.charge_directory_entries(1)?;
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map_err(FsError::from_io)?.is_dir();
            file_entries.push((name, is_dir));
        }
        let id = self.alloc_handle()?;
        self.dir_streams.insert(
            id,
            DirStreamState {
                dir_fd,
                entries: file_entries,
                cursor: 0,
            },
        );
        Ok(id)
    }

    pub fn read_dir_entry(&mut self, stream_id: u32) -> Option<Option<(String, bool)>> {
        let stream = self.dir_streams.get_mut(&stream_id)?;
        if stream.cursor >= stream.entries.len() {
            return Some(None);
        }
        let entry = stream.entries[stream.cursor].clone();
        stream.cursor += 1;
        Some(Some(entry))
    }

    pub fn has_dir_stream(&self, id: u32) -> bool {
        self.dir_streams.contains_key(&id)
    }

    /// Return the WASI preopens — derived from the registered directories.
    pub fn preopens(&self) -> Vec<(u32, &str)> {
        self.preopen_dirs
            .iter()
            .map(|(&fd, e)| (fd, e.guest_path.as_str()))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    fn checked_charge(current: &mut u64, amount: u64, maximum: u64) -> Result<(), FsError> {
        let updated = current.checked_add(amount).ok_or(FsError::Quota)?;
        if updated > maximum {
            return Err(FsError::Quota);
        }
        *current = updated;
        Ok(())
    }

    fn charge_read_bytes(&mut self, amount: u64) -> Result<(), FsError> {
        Self::checked_charge(
            &mut self.run_budget.bytes_read,
            amount,
            self.limits.max_read_bytes_per_run,
        )
    }

    fn charge_written_bytes(&mut self, amount: u64) -> Result<(), FsError> {
        Self::checked_charge(
            &mut self.run_budget.bytes_written,
            amount,
            self.limits.max_written_bytes_per_run,
        )
    }

    fn charge_creation(&mut self) -> Result<(), FsError> {
        Self::checked_charge(
            &mut self.run_budget.creations,
            1,
            self.limits.max_creations_per_run,
        )
    }

    fn charge_directory_entries(&mut self, amount: u64) -> Result<(), FsError> {
        Self::checked_charge(
            &mut self.run_budget.directory_entries,
            amount,
            self.limits.max_directory_entries_per_run,
        )
    }

    fn ensure_descriptor_capacity(&self) -> Result<(), FsError> {
        if self.descriptors.len() >= self.limits.max_open_descriptors {
            return Err(FsError::Quota);
        }
        Ok(())
    }

    fn ensure_stream_capacity(&self) -> Result<(), FsError> {
        let live_streams = self
            .streams
            .len()
            .checked_add(self.dir_streams.len())
            .ok_or(FsError::Quota)?;
        if live_streams >= self.limits.max_open_streams {
            return Err(FsError::Quota);
        }
        Ok(())
    }

    fn clear_directory_recursive(
        dir: &CapDir,
        depth: usize,
        limits: &FilesystemLimits,
        budget: &mut CleanupBudget,
    ) -> Result<(), FsError> {
        // Materialize names, not handles, so the directory iterator is closed
        // before entries are removed (required by some host filesystems).
        let mut names = Vec::new();
        let entries = dir.entries().map_err(FsError::from_io)?;
        for entry in entries {
            let entry = entry.map_err(FsError::from_io)?;
            budget.entries = budget.entries.checked_add(1).ok_or(FsError::Quota)?;
            if budget.entries > limits.max_cleanup_entries {
                return Err(FsError::Quota);
            }
            names.push(entry.file_name());
        }

        for name in names {
            let metadata = match dir.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                // A concurrent host cleanup may already have removed it.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(FsError::from_io(error)),
            };

            if metadata.is_dir() {
                if depth >= limits.max_recursive_depth {
                    return Err(FsError::Quota);
                }
                let child = dir.open_dir(&name).map_err(FsError::from_io)?;
                Self::clear_directory_recursive(&child, depth + 1, limits, budget)?;
                dir.remove_dir(&name).map_err(FsError::from_io)?;
            } else {
                // `symlink_metadata` does not follow the final component, so
                // directory symlinks are unlinked here rather than traversed.
                dir.remove_file(&name).map_err(FsError::from_io)?;
            }
        }
        Ok(())
    }

    fn recalculate_next_handle(&mut self) -> Result<(), FsError> {
        let maximum_live_handle = self
            .descriptors
            .keys()
            .chain(self.streams.keys())
            .chain(self.dir_streams.keys())
            .copied()
            .max();
        self.next_handle = match maximum_live_handle {
            Some(handle) => handle.checked_add(1).ok_or(FsError::Quota)?,
            None => FIRST_PREOPEN_FD,
        };
        Ok(())
    }

    fn mutating_dir(&self, fd: u32) -> Result<Dir, FsError> {
        let dir = self.get_dir(fd).cloned().ok_or(FsError::BadDescriptor)?;
        if !dir.perms().contains(DirPerms::MUTATE) {
            return Err(FsError::NotPermitted);
        }
        Ok(dir)
    }

    fn descriptors_in_subtree(&self, root_fd: u32, path: &GuestRelativePath) -> HashSet<u32> {
        self.descriptors
            .iter()
            .filter_map(|(&fd, entry)| {
                (!entry.is_preopen
                    && entry.root_fd == root_fd
                    && entry.relative_path.is_within(path))
                .then_some(fd)
            })
            .collect()
    }

    fn invalidate_descriptors(&mut self, descriptors: &HashSet<u32>) {
        if descriptors.is_empty() {
            return;
        }
        self.descriptors.retain(|fd, _| !descriptors.contains(fd));
        self.streams
            .retain(|_, stream| !descriptors.contains(&stream.file_fd));
        self.dir_streams
            .retain(|_, stream| !descriptors.contains(&stream.dir_fd));
    }

    fn register_preopen(
        &mut self,
        dir: Dir,
        guest_path: &str,
        lifetime: MountLifetime,
    ) -> Result<u32, FsError> {
        self.ensure_descriptor_capacity()?;
        let fd = self.alloc_handle()?;
        self.preopen_dirs.insert(
            fd,
            PreopenEntry {
                dir: dir.clone(),
                guest_path: guest_path.to_string(),
                lifetime,
            },
        );
        self.descriptors.insert(
            fd,
            DescriptorEntry {
                root_fd: fd,
                relative_path: GuestRelativePath::root(),
                descriptor_type: DescriptorType::Directory,
                is_preopen: true,
                parent_fd: None,
                directory: Some(dir),
            },
        );
        Ok(fd)
    }

    /// Resolve a validated nested path relative to an opened directory while
    /// retaining the descriptor's preopen-relative identity.
    fn resolve_at(&self, dir_fd: u32, path: &str) -> Result<ResolvedGuestPath, FsError> {
        let dir_relative = GuestRelativePath::parse(path)?;
        let parent = self
            .descriptors
            .get(&dir_fd)
            .ok_or(FsError::BadDescriptor)?;
        if parent.descriptor_type != DescriptorType::Directory || parent.directory.is_none() {
            return Err(FsError::BadDescriptor);
        }
        let root_relative = parent.relative_path.join(&dir_relative)?;
        Ok(ResolvedGuestPath {
            root_fd: parent.root_fd,
            dir_relative,
            root_relative,
        })
    }

    fn root_dir(&self, root_fd: u32) -> Option<&Dir> {
        self.preopen_dirs.get(&root_fd).map(|e| &e.dir)
    }

    fn file_parts(&self, fd: u32) -> Result<(&Dir, &Path), FsError> {
        let entry = self.descriptors.get(&fd).ok_or(FsError::BadDescriptor)?;
        if entry.descriptor_type != DescriptorType::RegularFile {
            return Err(FsError::BadDescriptor);
        }
        let root = self.root_dir(entry.root_fd).ok_or(FsError::BadDescriptor)?;
        Ok((root, entry.relative_path.as_path()))
    }

    fn parse_rooted_guest_path(path: &str, root: &str) -> Result<GuestRelativePath> {
        let prefix = format!("/{root}/");
        let relative = path
            .strip_prefix(&prefix)
            .ok_or_else(|| anyhow::anyhow!("path must be rooted at /{root}: {path}"))?;
        GuestRelativePath::parse(relative)
            .map_err(|_| anyhow::anyhow!("invalid path under /{root}: {path}"))
    }

    fn alloc_handle(&mut self) -> Result<u32, FsError> {
        let id = self.next_handle;
        self.next_handle = self
            .next_handle
            .checked_add(1)
            .ok_or_else(|| FsError::Io("file descriptor handle space exhausted".into()))?;
        Ok(id)
    }
}

// No Clone — snapshots don't need filesystem state. Input is immutable
// (shared via Arc) and output is ephemeral (wiped each run).
//TODO: include filesystem state in snapshots

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn test_fs() -> (CapFs, tempfile::TempDir, tempfile::TempDir) {
        test_fs_with_limits(FilesystemLimits::default())
    }

    fn test_fs_with_limits(
        limits: FilesystemLimits,
    ) -> (CapFs, tempfile::TempDir, tempfile::TempDir) {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let fs = CapFs::new()
            .with_limits(limits)
            .with_input(input.path())
            .unwrap()
            .with_output_dir(
                output.path(),
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::READ | FilePerms::WRITE,
            )
            .unwrap();
        (fs, input, output)
    }

    /// Helper: write a file directly into the input dir (simulates host setup).
    fn host_write_input(input: &tempfile::TempDir, name: &str, data: &[u8]) {
        std::fs::write(input.path().join(name), data).unwrap();
    }

    fn host_write_output(output: &tempfile::TempDir, name: &str, data: &[u8]) {
        std::fs::write(output.path().join(name), data).unwrap();
    }

    fn host_create_dir(root: &tempfile::TempDir, name: &str) {
        std::fs::create_dir_all(root.path().join(name)).unwrap();
    }

    /// Look up the fd for a preopen by guest path.
    fn preopen_fd(fs: &CapFs, path: &str) -> u32 {
        fs.preopens()
            .into_iter()
            .find(|(_, p)| *p == path)
            .unwrap()
            .0
    }

    #[test]
    fn guest_relative_path_accepts_and_normalizes_safe_paths() {
        let cases = [
            ("src/lib.rs", "src/lib.rs", false),
            ("./src/lib.rs", "src/lib.rs", false),
            ("src//./nested///lib.rs", "src/nested/lib.rs", false),
            (
                "dir with spaces/file.txt",
                "dir with spaces/file.txt",
                false,
            ),
            (".git/config", ".git/config", false),
            ("unicode/olá.txt", "unicode/olá.txt", false),
            ("src/", "src", true),
            ("src/.", "src", true),
        ];

        for (input, normalized, requires_directory) in cases {
            let parsed = GuestRelativePath::parse(input).unwrap();
            assert_eq!(parsed.as_path(), Path::new(normalized));
            assert_eq!(parsed.requires_directory, requires_directory);
        }
    }

    #[test]
    fn guest_relative_path_rejects_unsafe_and_ambiguous_paths() {
        for path in [
            "",
            ".",
            "./",
            "..",
            "../secret",
            "a/../../secret",
            "/absolute/path",
            "a\\b",
            "C:\\secret",
            "C:/secret",
            "C:secret",
            "file\0name",
        ] {
            assert_eq!(
                GuestRelativePath::parse(path),
                Err(FsError::InvalidPath),
                "unexpected result for {path:?}"
            );
        }
    }

    #[test]
    fn guest_relative_path_enforces_length_and_depth_limits() {
        let overlong = "a".repeat(MAX_GUEST_PATH_BYTES + 1);
        assert_eq!(
            GuestRelativePath::parse(&overlong),
            Err(FsError::InvalidPath)
        );

        let too_deep = std::iter::repeat_n("a", MAX_GUEST_PATH_COMPONENTS + 1)
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(
            GuestRelativePath::parse(&too_deep),
            Err(FsError::InvalidPath)
        );
    }

    proptest! {
        #[test]
        fn guest_relative_path_never_accepts_parent_components(
            prefix in "[a-z]{0,16}",
            suffix in "[a-z]{0,16}",
        ) {
            let path = format!("{prefix}/../{suffix}");
            prop_assert_eq!(
                GuestRelativePath::parse(&path),
                Err(FsError::InvalidPath)
            );
        }

        #[test]
        fn guest_relative_path_never_accepts_backslashes(
            prefix in "[a-z]{0,16}",
            suffix in "[a-z]{0,16}",
        ) {
            let path = format!("{prefix}\\{suffix}");
            prop_assert_eq!(
                GuestRelativePath::parse(&path),
                Err(FsError::InvalidPath)
            );
        }
    }

    #[test]
    fn input_is_readonly_preopen() {
        let (fs, _i, _o) = test_fs();
        let dir = fs.dir_by_guest_path("/input").unwrap();
        assert!(dir.perms().contains(DirPerms::READ));
        assert!(!dir.perms().contains(DirPerms::MUTATE));
        assert!(dir.file_perms().contains(FilePerms::READ));
        assert!(!dir.file_perms().contains(FilePerms::WRITE));
    }

    #[test]
    fn guest_reads_host_provided_input() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "test.txt", b"hello world");

        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "test.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let (data, _) = fs.read_file(fd, 0, 100).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn opened_directory_is_typed_and_lists_its_own_entries() {
        let (mut fs, input, _o) = test_fs();
        host_create_dir(&input, "src/nested");
        host_write_input(&input, "root.txt", b"root");
        host_write_input(&input, "src/lib.rs", b"pub fn library() {}");

        let input_fd = preopen_fd(&fs, "/input");
        let src_fd = fs.open_at(input_fd, "src", OpenFlags::DIRECTORY).unwrap();

        assert!(fs.is_directory(src_fd));
        assert_eq!(fs.get_type(src_fd), Ok(DescriptorType::Directory));
        assert_eq!(
            fs.stat(src_fd),
            Ok(DescriptorStat {
                descriptor_type: DescriptorType::Directory,
                size: 0,
            })
        );
        assert_eq!(
            fs.stat_at(src_fd, "lib.rs"),
            Ok(DescriptorStat {
                descriptor_type: DescriptorType::RegularFile,
                size: 19,
            })
        );

        let stream = fs.create_dir_stream(src_fd).unwrap();
        let mut entries = Vec::new();
        while let Some(Some(entry)) = fs.read_dir_entry(stream) {
            entries.push(entry);
        }
        entries.sort();
        assert_eq!(
            entries,
            vec![("lib.rs".to_string(), false), ("nested".to_string(), true)]
        );
    }

    #[test]
    fn open_at_and_stat_at_accept_safe_nested_paths() {
        let (mut fs, input, _o) = test_fs();
        host_create_dir(&input, "src/nested");
        host_create_dir(&input, "dir with spaces");
        host_create_dir(&input, ".git");
        host_create_dir(&input, "unicode");
        host_write_input(&input, "src/nested/lib.rs", b"nested");
        host_write_input(&input, "dir with spaces/file.txt", b"spaces");
        host_write_input(&input, ".git/config", b"git");
        host_write_input(&input, "unicode/olá.txt", b"unicode");

        let input_fd = preopen_fd(&fs, "/input");
        for (path, contents) in [
            ("./src/nested/lib.rs", b"nested".as_slice()),
            ("dir with spaces/file.txt", b"spaces".as_slice()),
            (".git/config", b"git".as_slice()),
            ("unicode/olá.txt", b"unicode".as_slice()),
        ] {
            let fd = fs
                .open_at(input_fd, path, OpenFlags::OPEN_EXISTING)
                .unwrap();
            assert_eq!(fs.read_file(fd, 0, 100).unwrap().0, contents);
            assert_eq!(
                fs.stat_at(input_fd, path).unwrap().descriptor_type,
                DescriptorType::RegularFile
            );
        }
    }

    #[test]
    fn nested_path_is_composed_with_an_opened_directory_identity() {
        let (mut fs, input, _o) = test_fs();
        host_create_dir(&input, "src/nested");
        host_write_input(&input, "src/nested/lib.rs", b"nested");

        let input_fd = preopen_fd(&fs, "/input");
        let src_fd = fs.open_at(input_fd, "src", OpenFlags::DIRECTORY).unwrap();
        let file_fd = fs
            .open_at(src_fd, "./nested/lib.rs", OpenFlags::OPEN_EXISTING)
            .unwrap();

        assert_eq!(fs.read_file(file_fd, 0, 100).unwrap().0, b"nested");
        assert_eq!(
            fs.descriptors[&file_fd].relative_path.as_path(),
            Path::new("src/nested/lib.rs")
        );
    }

    #[test]
    fn nested_path_composition_enforces_total_depth_limit() {
        let (mut fs, input, _o) = test_fs();
        let parent_path = std::iter::repeat_n("a", 200).collect::<Vec<_>>().join("/");
        host_create_dir(&input, &parent_path);

        let input_fd = preopen_fd(&fs, "/input");
        let parent_fd = fs
            .open_at(input_fd, &parent_path, OpenFlags::DIRECTORY)
            .unwrap();
        let child_path = std::iter::repeat_n("b", 57).collect::<Vec<_>>().join("/");

        assert_eq!(
            fs.open_at(parent_fd, &child_path, OpenFlags::OPEN_EXISTING),
            Err(FsError::InvalidPath)
        );
    }

    #[test]
    fn file_opened_relative_to_directory_survives_parent_close() {
        let (mut fs, input, _o) = test_fs();
        host_create_dir(&input, "src");
        host_write_input(&input, "src/lib.rs", b"nested file");

        let input_fd = preopen_fd(&fs, "/input");
        let src_fd = fs.open_at(input_fd, "src", OpenFlags::DIRECTORY).unwrap();
        let file_fd = fs
            .open_at(src_fd, "lib.rs", OpenFlags::OPEN_EXISTING)
            .unwrap();

        assert_eq!(fs.file_dir_fd(file_fd), Some(src_fd));
        assert_eq!(fs.get_type(file_fd), Ok(DescriptorType::RegularFile));
        assert_eq!(fs.read_file(file_fd, 0, 100).unwrap().0, b"nested file");

        fs.close_descriptor(src_fd).unwrap();
        assert_eq!(fs.get_type(src_fd), Err(FsError::BadDescriptor));
        assert_eq!(fs.read_file(file_fd, 0, 100).unwrap().0, b"nested file");
        assert_eq!(
            fs.get_flags(file_fd),
            Ok(DescriptorFlags {
                read: true,
                write: false,
                mutate_directory: false,
            })
        );
    }

    #[test]
    fn file_created_through_nested_directory_descriptors_uses_root_relative_path() {
        let (mut fs, _i, output) = test_fs();
        host_create_dir(&output, "build/cache");

        let output_fd = preopen_fd(&fs, "/output");
        let build_fd = fs
            .open_at(output_fd, "build", OpenFlags::DIRECTORY)
            .unwrap();
        let cache_fd = fs.open_at(build_fd, "cache", OpenFlags::DIRECTORY).unwrap();
        let file_fd = fs
            .open_at(cache_fd, "result.txt", OpenFlags::CREATE)
            .unwrap();

        fs.write_file(file_fd, 0, b"nested output").unwrap();
        assert_eq!(
            std::fs::read(output.path().join("build/cache/result.txt")).unwrap(),
            b"nested output"
        );
        assert_eq!(fs.read_file(file_fd, 0, 100).unwrap().0, b"nested output");
    }

    #[test]
    fn opening_same_path_returns_independent_descriptors() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "same.txt", b"same contents");

        let input_fd = preopen_fd(&fs, "/input");
        let first = fs
            .open_at(input_fd, "same.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let second = fs
            .open_at(input_fd, "same.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        assert_ne!(first, second);
        fs.close_descriptor(first).unwrap();
        assert_eq!(fs.get_type(first), Err(FsError::BadDescriptor));
        assert_eq!(fs.read_file(second, 0, 100).unwrap().0, b"same contents");
    }

    #[test]
    fn output_write_and_collect() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs.open_at(output_fd, "out.txt", OpenFlags::CREATE).unwrap();
        fs.write_file(fd, 0, b"result").unwrap();
        let files = fs.get_output_files();
        assert!(files.contains(&"out.txt".to_string()));
        let output_dir = fs.output_path().unwrap();
        assert_eq!(
            std::fs::read(output_dir.join("out.txt")).unwrap(),
            b"result"
        );
    }

    #[test]
    fn clear_output_preserves_input() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "keep.txt", b"input");
        fs.write_output_path("/output/gone.txt", b"output".to_vec())
            .unwrap();

        fs.clear_ephemeral_mounts().unwrap();

        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "keep.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let (data, _) = fs.read_file(fd, 0, 100).unwrap();
        assert_eq!(data, b"input");
        assert!(fs.get_output_files().is_empty());
    }

    #[test]
    fn clear_output_invalidates_only_output_descriptors() {
        let (mut fs, input, output) = test_fs();
        host_create_dir(&input, "src");
        host_write_input(&input, "src/lib.rs", b"input");
        host_create_dir(&output, "build");
        host_write_output(&output, "build/result.txt", b"output");

        let input_fd = preopen_fd(&fs, "/input");
        let output_fd = preopen_fd(&fs, "/output");
        let input_dir = fs.open_at(input_fd, "src", OpenFlags::DIRECTORY).unwrap();
        let input_file = fs
            .open_at(input_dir, "lib.rs", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let output_dir = fs
            .open_at(output_fd, "build", OpenFlags::DIRECTORY)
            .unwrap();
        let output_file = fs
            .open_at(output_dir, "result.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        fs.clear_ephemeral_mounts().unwrap();

        assert_eq!(fs.get_type(input_dir), Ok(DescriptorType::Directory));
        assert_eq!(fs.read_file(input_file, 0, 100).unwrap().0, b"input");
        assert_eq!(fs.get_type(output_dir), Err(FsError::BadDescriptor));
        assert_eq!(fs.get_type(output_file), Err(FsError::BadDescriptor));
        assert_eq!(fs.get_type(output_fd), Ok(DescriptorType::Directory));
    }

    #[test]
    fn prepare_for_run_recursively_clears_output_and_preserves_the_root() {
        let (mut fs, input, output) = test_fs();
        host_write_input(&input, "keep.txt", b"input");
        host_create_dir(&output, "build/cache/deep");
        host_write_output(&output, "build/cache/deep/result.txt", b"output");
        host_write_output(&output, "top.txt", b"output");

        fs.prepare_for_run().unwrap();

        assert!(output.path().is_dir());
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(input.path().join("keep.txt")).unwrap(),
            b"input"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recursive_cleanup_unlinks_directory_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let (mut fs, _input, output) = test_fs();
        let outside = tempfile::tempdir().unwrap();
        host_write_output(&outside, "keep.txt", b"outside");
        symlink(outside.path(), output.path().join("outside-link")).unwrap();

        fs.prepare_for_run().unwrap();

        assert!(!output.path().join("outside-link").exists());
        assert_eq!(
            std::fs::read(outside.path().join("keep.txt")).unwrap(),
            b"outside"
        );
    }

    #[test]
    fn cleanup_bound_failure_is_returned_and_invalidates_mount_handles() {
        let limits = FilesystemLimits {
            max_cleanup_entries: 1,
            ..FilesystemLimits::default()
        };
        let (mut fs, _input, output) = test_fs_with_limits(limits);
        host_write_output(&output, "one.txt", b"one");
        host_write_output(&output, "two.txt", b"two");

        let output_fd = preopen_fd(&fs, "/output");
        let file_fd = fs
            .open_at(output_fd, "one.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        assert_eq!(fs.prepare_for_run(), Err(FsError::Quota));
        assert_eq!(fs.get_type(file_fd), Err(FsError::BadDescriptor));
        assert!(!fs.get_output_files().is_empty());
    }

    #[test]
    fn failed_cleanup_does_not_reset_the_previous_run_budget() {
        let limits = FilesystemLimits {
            max_creations_per_run: 1,
            max_cleanup_entries: 1,
            ..FilesystemLimits::default()
        };
        let (mut fs, _input, output) = test_fs_with_limits(limits);
        host_write_output(&output, "one.txt", b"one");
        host_write_output(&output, "two.txt", b"two");
        let output_fd = preopen_fd(&fs, "/output");
        fs.open_at(output_fd, "created.txt", OpenFlags::CREATE)
            .unwrap();

        assert_eq!(fs.prepare_for_run(), Err(FsError::Quota));
        assert_eq!(
            fs.create_directory_at(output_fd, "still-blocked"),
            Err(FsError::Quota)
        );
    }

    #[test]
    fn cleanup_handle_recalculation_preserves_live_input_stream_ids() {
        let (mut fs, input, output) = test_fs();
        host_write_input(&input, "first.txt", b"first");
        host_write_input(&input, "second.txt", b"second");
        host_write_output(&output, "gone.txt", b"gone");
        let input_fd = preopen_fd(&fs, "/input");
        let output_fd = preopen_fd(&fs, "/output");
        let first_fd = fs
            .open_at(input_fd, "first.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let input_stream = fs.create_read_stream(first_fd, 0).unwrap();
        fs.open_at(output_fd, "gone.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        fs.clear_ephemeral_mounts().unwrap();
        let second_fd = fs
            .open_at(input_fd, "second.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        assert_ne!(second_fd, input_stream);
        assert!(fs.has_stream(input_stream));
    }

    #[test]
    fn cleanup_depth_failure_is_returned() {
        let limits = FilesystemLimits {
            max_recursive_depth: 1,
            ..FilesystemLimits::default()
        };
        let (mut fs, _input, output) = test_fs_with_limits(limits);
        host_create_dir(&output, "one/two");
        host_write_output(&output, "one/two/file.txt", b"data");

        assert_eq!(fs.prepare_for_run(), Err(FsError::Quota));
        assert!(output.path().join("one/two/file.txt").is_file());
    }

    #[test]
    fn clear_mount_rejects_persistent_input() {
        let (mut fs, input, _output) = test_fs();
        host_write_input(&input, "keep.txt", b"input");
        let input_fd = preopen_fd(&fs, "/input");

        assert_eq!(fs.clear_mount(input_fd), Err(FsError::NotPermitted));
        assert!(input.path().join("keep.txt").is_file());
    }

    #[test]
    fn default_temp_output() {
        let input = tempfile::tempdir().unwrap();
        let mut fs = CapFs::new()
            .with_input(input.path())
            .unwrap()
            .with_temp_output()
            .unwrap();

        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs
            .open_at(output_fd, "test.txt", OpenFlags::CREATE)
            .unwrap();
        fs.write_file(fd, 0, b"works").unwrap();
        let files = fs.get_output_files();
        assert!(files.contains(&"test.txt".to_string()));
        let output_dir = fs.output_path().unwrap();
        assert_eq!(
            std::fs::read(output_dir.join("test.txt")).unwrap(),
            b"works"
        );
    }

    #[test]
    fn no_input_dir() {
        let fs = CapFs::new();
        assert!(fs.dir_by_guest_path("/input").is_none());
        assert!(fs.dir_by_guest_path("/output").is_none());
        assert_eq!(fs.preopens().len(), 0);
    }

    #[test]
    fn stream_file_backed() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs.open_at(output_fd, "s.txt", OpenFlags::CREATE).unwrap();
        let ws = fs.create_write_stream(fd, 0).unwrap();
        fs.stream_write(ws, b"hello ").unwrap();
        fs.stream_write(ws, b"world").unwrap();
        let rs = fs.create_read_stream(fd, 0).unwrap();
        assert_eq!(fs.stream_read(rs, 100).unwrap(), b"hello world");
    }

    #[test]
    fn open_at_rejects_bad_paths() {
        let (mut fs, _i, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_create_dir(&output, "a");
        host_create_dir(&output, "trailing");

        // Path traversal
        assert_eq!(
            fs.open_at(output_fd, "../x", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "../../etc/passwd", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // Parent components are rejected even when a lexical normalization
        // would appear to remain under the preopen.
        assert_eq!(
            fs.open_at(output_fd, "a/../../x", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // Backslash separators
        assert_eq!(
            fs.open_at(output_fd, "a\\b", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "..\\x", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "C:/secret", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // Special names
        assert_eq!(
            fs.open_at(output_fd, ".", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "..", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // Null bytes and whitespace tricks
        assert_eq!(
            fs.open_at(output_fd, "\0", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "file\0.txt", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // Absolute paths are never accepted.
        assert_eq!(
            fs.open_at(output_fd, "/absolute", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // A trailing slash is operation-sensitive: it works for a directory
        // but cannot be used to create or open a regular file.
        assert!(
            fs.open_at(output_fd, "trailing/", OpenFlags::DIRECTORY)
                .is_ok()
        );
        assert_eq!(
            fs.open_at(output_fd, "file.txt/", OpenFlags::CREATE),
            Err(FsError::NoEntry)
        );

        // Invalid descriptor
        assert_eq!(
            fs.open_at(999, "file.txt", OpenFlags::CREATE),
            Err(FsError::BadDescriptor)
        );

        // Valid names should work
        assert!(fs.open_at(output_fd, "file.txt", OpenFlags::CREATE).is_ok());
        assert!(fs.open_at(output_fd, "a/b.txt", OpenFlags::CREATE).is_ok());
        assert!(
            fs.open_at(output_fd, "./a//./c.txt", OpenFlags::CREATE)
                .is_ok()
        );
        assert!(fs.open_at(output_fd, ".hidden", OpenFlags::CREATE).is_ok());
        assert!(
            fs.open_at(output_fd, "file-name_v2.tar.gz", OpenFlags::CREATE)
                .is_ok()
        );
    }

    #[test]
    fn directory_flag_rejects_regular_file() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "file.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");

        assert_eq!(
            fs.open_at(input_fd, "file.txt", OpenFlags::DIRECTORY),
            Err(FsError::NoEntry)
        );
    }

    #[test]
    fn close_descriptor_invalidates_derived_streams() {
        let (mut fs, _i, output) = test_fs();
        host_create_dir(&output, "dir");
        let output_fd = preopen_fd(&fs, "/output");

        let file_fd = fs
            .open_at(output_fd, "file.txt", OpenFlags::CREATE)
            .unwrap();
        fs.write_file(file_fd, 0, b"data").unwrap();
        let file_stream = fs.create_read_stream(file_fd, 0).unwrap();

        let dir_fd = fs.open_at(output_fd, "dir", OpenFlags::DIRECTORY).unwrap();
        let dir_stream = fs.create_dir_stream(dir_fd).unwrap();

        fs.close_descriptor(file_fd).unwrap();
        assert!(!fs.has_stream(file_stream));
        assert_eq!(fs.stream_read(file_stream, 1), Err(FsError::BadDescriptor));

        fs.close_descriptor(dir_fd).unwrap();
        assert!(!fs.has_dir_stream(dir_stream));
        assert_eq!(fs.read_dir_entry(dir_stream), None);
    }

    #[test]
    fn preopen_descriptor_cannot_be_closed() {
        let (mut fs, _i, _o) = test_fs();
        let input_fd = preopen_fd(&fs, "/input");

        assert_eq!(fs.close_descriptor(input_fd), Err(FsError::NotPermitted));
        assert_eq!(fs.get_type(input_fd), Ok(DescriptorType::Directory));
    }

    #[test]
    fn input_is_readonly_no_create() {
        let (mut fs, _i, _o) = test_fs();
        let input_fd = preopen_fd(&fs, "/input");
        assert_eq!(
            fs.open_at(input_fd, "new.txt", OpenFlags::CREATE),
            Err(FsError::NotPermitted)
        );
    }

    #[test]
    fn input_is_readonly_no_truncate() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "existing.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "existing.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert!(fs.read_file(fd, 0, 100).is_ok());
        assert_eq!(
            fs.open_at(input_fd, "existing.txt", OpenFlags::TRUNCATE),
            Err(FsError::NotPermitted)
        );
    }

    #[test]
    fn input_file_perms_are_read_only() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "readonly.txt", b"original");
        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "readonly.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert!(fs.file_has_perms(fd, FilePerms::READ));
        assert!(!fs.file_has_perms(fd, FilePerms::WRITE));
    }

    #[test]
    fn output_allows_read_and_write() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs.open_at(output_fd, "rw.txt", OpenFlags::CREATE).unwrap();
        assert!(fs.file_has_perms(fd, FilePerms::READ));
        assert!(fs.file_has_perms(fd, FilePerms::WRITE));
        fs.write_file(fd, 0, b"hello").unwrap();
        let (data, _) = fs.read_file(fd, 0, 100).unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn dir_perms_correct() {
        let (fs, _i, _o) = test_fs();
        let input = fs.get_dir(preopen_fd(&fs, "/input")).unwrap();
        assert!(input.perms().contains(DirPerms::READ));
        assert!(!input.perms().contains(DirPerms::MUTATE));

        let output = fs.get_dir(preopen_fd(&fs, "/output")).unwrap();
        assert!(output.perms().contains(DirPerms::READ));
        assert!(output.perms().contains(DirPerms::MUTATE));
    }

    #[test]
    fn custom_output_perms_respected() {
        let output = tempfile::tempdir().unwrap();
        let fs = CapFs::new()
            .with_output_dir(output.path(), DirPerms::MUTATE, FilePerms::WRITE)
            .unwrap();
        let output_fd = preopen_fd(&fs, "/output");
        let dir = fs.get_dir(output_fd).unwrap();
        assert!(!dir.perms().contains(DirPerms::READ));
        assert!(dir.perms().contains(DirPerms::MUTATE));
        assert!(!dir.file_perms().contains(FilePerms::READ));
        assert!(dir.file_perms().contains(FilePerms::WRITE));
    }

    // ── Metadata tests ─────────────────────────────────────────────

    #[test]
    fn get_type_directory() {
        let (fs, _i, _o) = test_fs();
        let input_fd = preopen_fd(&fs, "/input");
        let output_fd = preopen_fd(&fs, "/output");
        assert_eq!(fs.get_type(input_fd).unwrap(), DescriptorType::Directory);
        assert_eq!(fs.get_type(output_fd).unwrap(), DescriptorType::Directory);
    }

    #[test]
    fn get_type_file() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "test.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "test.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(fs.get_type(fd).unwrap(), DescriptorType::RegularFile);
    }

    #[test]
    fn get_type_invalid_fd() {
        let (fs, _i, _o) = test_fs();
        assert_eq!(fs.get_type(999), Err(FsError::BadDescriptor));
    }

    #[test]
    fn stat_directory() {
        let (fs, _i, _o) = test_fs();
        let input_fd = preopen_fd(&fs, "/input");
        let s = fs.stat(input_fd).unwrap();
        assert_eq!(s.descriptor_type, DescriptorType::Directory);
        assert_eq!(s.size, 0);
    }

    #[test]
    fn stat_file_reports_size() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs
            .open_at(output_fd, "data.bin", OpenFlags::CREATE)
            .unwrap();
        fs.write_file(fd, 0, b"hello").unwrap();
        let s = fs.stat(fd).unwrap();
        assert_eq!(s.descriptor_type, DescriptorType::RegularFile);
        assert_eq!(s.size, 5);
    }

    #[test]
    fn stat_invalid_fd() {
        let (fs, _i, _o) = test_fs();
        assert_eq!(fs.stat(999), Err(FsError::BadDescriptor));
    }

    #[test]
    fn stat_at_existing_file() {
        let (fs, input, _o) = test_fs();
        host_write_input(&input, "readme.md", b"# Hello");
        let input_fd = preopen_fd(&fs, "/input");
        let s = fs.stat_at(input_fd, "readme.md").unwrap();
        assert_eq!(s.descriptor_type, DescriptorType::RegularFile);
        assert_eq!(s.size, 7);
    }

    #[test]
    fn stat_at_missing_file() {
        let (fs, _i, _o) = test_fs();
        let input_fd = preopen_fd(&fs, "/input");
        assert_eq!(fs.stat_at(input_fd, "nope.txt"), Err(FsError::NoEntry));
    }

    #[test]
    fn stat_at_requires_read_perm() {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("file.txt"), b"data").unwrap();
        let fs = CapFs::new()
            .with_output_dir(output.path(), DirPerms::MUTATE, FilePerms::WRITE)
            .unwrap();
        let output_fd = preopen_fd(&fs, "/output");
        assert_eq!(
            fs.stat_at(output_fd, "file.txt"),
            Err(FsError::NotPermitted)
        );
    }

    #[test]
    fn stat_at_invalid_fd() {
        let (fs, _i, _o) = test_fs();
        assert_eq!(fs.stat_at(999, "file.txt"), Err(FsError::BadDescriptor));
    }

    #[test]
    fn get_flags_input_directory() {
        let (fs, _i, _o) = test_fs();
        let input_fd = preopen_fd(&fs, "/input");
        let f = fs.get_flags(input_fd).unwrap();
        assert!(f.read);
        assert!(!f.write);
        assert!(!f.mutate_directory);
    }

    #[test]
    fn get_flags_output_directory() {
        let (fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let f = fs.get_flags(output_fd).unwrap();
        assert!(f.read);
        assert!(f.write);
        assert!(f.mutate_directory);
    }

    #[test]
    fn get_flags_input_file() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "x.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "x.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let f = fs.get_flags(fd).unwrap();
        assert!(f.read);
        assert!(!f.write);
        assert!(!f.mutate_directory);
    }

    #[test]
    fn get_flags_output_file() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs.open_at(output_fd, "out.txt", OpenFlags::CREATE).unwrap();
        let f = fs.get_flags(fd).unwrap();
        assert!(f.read);
        assert!(f.write);
        assert!(!f.mutate_directory);
    }

    #[test]
    fn get_flags_invalid_fd() {
        let (fs, _i, _o) = test_fs();
        assert_eq!(fs.get_flags(999), Err(FsError::BadDescriptor));
    }

    // ── Permission denial tests ─────────────────────────────────────

    #[test]
    fn read_file_denied_without_read_perm() {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("secret.txt"), b"data").unwrap();
        let mut fs = CapFs::new()
            .with_output_dir(
                output.path(),
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::WRITE,
            )
            .unwrap();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs
            .open_at(output_fd, "secret.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(fs.read_file(fd, 0, 100), Err(FsError::NotPermitted));
    }

    #[test]
    fn write_file_denied_without_write_perm() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "ro.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "ro.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(fs.write_file(fd, 0, b"hacked"), Err(FsError::NotPermitted));
    }

    #[test]
    fn create_read_stream_denied_without_read_perm() {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("f.txt"), b"data").unwrap();
        let mut fs = CapFs::new()
            .with_output_dir(
                output.path(),
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::WRITE,
            )
            .unwrap();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs
            .open_at(output_fd, "f.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(fs.create_read_stream(fd, 0), Err(FsError::NotPermitted));
    }

    #[test]
    fn create_write_stream_denied_without_write_perm() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "ro.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "ro.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(fs.create_write_stream(fd, 0), Err(FsError::NotPermitted));
    }

    #[test]
    fn create_append_stream_denied_without_write_perm() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "ro.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");
        let fd = fs
            .open_at(input_fd, "ro.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(fs.create_append_stream(fd), Err(FsError::NotPermitted));
    }

    #[test]
    fn create_dir_stream_denied_without_read_perm() {
        let output = tempfile::tempdir().unwrap();
        let mut fs = CapFs::new()
            .with_output_dir(output.path(), DirPerms::MUTATE, FilePerms::WRITE)
            .unwrap();
        let output_fd = preopen_fd(&fs, "/output");
        assert_eq!(fs.create_dir_stream(output_fd), Err(FsError::NotPermitted));
    }

    #[test]
    fn write_output_path_denied_without_write_perm() {
        let output = tempfile::tempdir().unwrap();
        let mut fs = CapFs::new()
            .with_output_dir(output.path(), DirPerms::READ, FilePerms::READ)
            .unwrap();
        assert!(
            fs.write_output_path("/output/test.txt", b"data".to_vec())
                .is_err()
        );
    }

    #[test]
    fn read_guest_file_works() {
        let (mut fs, input, _o) = test_fs();
        host_write_input(&input, "hello.txt", b"world");
        let data = fs.read_guest_file("/input/hello.txt").unwrap();
        assert_eq!(data, b"world");
    }

    #[test]
    fn host_file_helpers_use_the_same_nested_path_policy() {
        let (mut fs, input, output) = test_fs();
        host_create_dir(&input, "nested/input");
        host_create_dir(&output, "nested/output");
        host_write_input(&input, "nested/input/file.txt", b"input");

        assert_eq!(
            fs.read_guest_file("/input/./nested/input/file.txt")
                .unwrap(),
            b"input"
        );
        fs.write_output_path("/output/nested/output/file.txt", b"output".to_vec())
            .unwrap();
        assert_eq!(
            std::fs::read(output.path().join("nested/output/file.txt")).unwrap(),
            b"output"
        );

        assert!(fs.read_guest_file("/input/nested/../file.txt").is_err());
        assert!(
            fs.write_output_path("/output/nested/../escape.txt", Vec::new())
                .is_err()
        );
    }

    #[test]
    fn read_guest_file_denied_without_read_perm() {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("f.txt"), b"data").unwrap();
        let mut fs = CapFs::new()
            .with_output_dir(output.path(), DirPerms::MUTATE, FilePerms::WRITE)
            .unwrap();
        assert!(fs.read_guest_file("/output/f.txt").is_err());
    }

    #[test]
    fn read_guest_file_missing() {
        let (mut fs, _i, _o) = test_fs();
        assert!(fs.read_guest_file("/input/nope.txt").is_err());
    }

    #[test]
    fn read_guest_file_outside_preopens() {
        let (mut fs, _i, _o) = test_fs();
        assert!(fs.read_guest_file("/etc/passwd").is_err());
        assert!(fs.read_guest_file("/tmp/something").is_err());
        assert!(fs.read_guest_file("/secret/data.txt").is_err());
    }

    #[test]
    fn read_guest_file_traversal() {
        let (mut fs, _i, _o) = test_fs();
        assert!(fs.read_guest_file("/input/../etc/passwd").is_err());
        assert!(fs.read_guest_file("/input/../../root/.ssh/id_rsa").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_follow_policy_is_explicit_and_capability_scoped() {
        use std::os::unix::fs::symlink;

        let (mut fs, input, _o) = test_fs();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        host_create_dir(&input, "src");
        host_write_input(&input, "src/lib.rs", b"inside");
        symlink("src/lib.rs", input.path().join("internal-link")).unwrap();
        symlink(
            outside.path().join("secret.txt"),
            input.path().join("external-link"),
        )
        .unwrap();

        let input_fd = preopen_fd(&fs, "/input");

        let internal = fs
            .open_at_with_path_flags(
                input_fd,
                "internal-link",
                PathFlags::SYMLINK_FOLLOW,
                OpenFlags::OPEN_EXISTING,
            )
            .unwrap();
        assert_eq!(fs.read_file(internal, 0, 100).unwrap().0, b"inside");
        assert_eq!(
            fs.stat_at_with_path_flags(input_fd, "internal-link", PathFlags::empty())
                .unwrap()
                .descriptor_type,
            DescriptorType::SymbolicLink
        );
        assert_eq!(
            fs.open_at_with_path_flags(
                input_fd,
                "internal-link",
                PathFlags::empty(),
                OpenFlags::OPEN_EXISTING,
            ),
            Err(FsError::SymlinkLoop)
        );

        assert!(
            fs.open_at_with_path_flags(
                input_fd,
                "external-link",
                PathFlags::SYMLINK_FOLLOW,
                OpenFlags::OPEN_EXISTING,
            )
            .is_err()
        );
        assert_eq!(
            fs.stat_at_with_path_flags(input_fd, "external-link", PathFlags::empty())
                .unwrap()
                .descriptor_type,
            DescriptorType::SymbolicLink
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_fail_without_hanging() {
        use std::os::unix::fs::symlink;

        let (mut fs, input, _o) = test_fs();
        symlink("second", input.path().join("first")).unwrap();
        symlink("first", input.path().join("second")).unwrap();
        let input_fd = preopen_fd(&fs, "/input");

        assert!(
            fs.open_at_with_path_flags(
                input_fd,
                "first",
                PathFlags::SYMLINK_FOLLOW,
                OpenFlags::OPEN_EXISTING,
            )
            .is_err()
        );
        assert!(
            fs.stat_at_with_path_flags(input_fd, "first", PathFlags::SYMLINK_FOLLOW)
                .is_err()
        );
    }

    #[test]
    fn open_at_on_nonexistent_preopen() {
        let (mut fs, _i, _o) = test_fs();
        // Try to use a made-up fd that isn't a preopen
        assert_eq!(
            fs.open_at(99, "file.txt", OpenFlags::CREATE),
            Err(FsError::BadDescriptor)
        );
        assert_eq!(
            fs.open_at(0, "file.txt", OpenFlags::CREATE),
            Err(FsError::BadDescriptor)
        );
        assert_eq!(
            fs.open_at(1, "file.txt", OpenFlags::CREATE),
            Err(FsError::BadDescriptor)
        );
        assert_eq!(
            fs.open_at(2, "file.txt", OpenFlags::CREATE),
            Err(FsError::BadDescriptor)
        );
    }

    #[test]
    fn write_output_path_outside_output() {
        let (mut fs, _i, _o) = test_fs();
        // Must be rooted at /output
        assert!(
            fs.write_output_path("/input/sneaky.txt", b"data".to_vec())
                .is_err()
        );
        assert!(
            fs.write_output_path("/etc/passwd", b"data".to_vec())
                .is_err()
        );
        assert!(
            fs.write_output_path("/output/../etc/passwd", b"data".to_vec())
                .is_err()
        );
    }

    // ── Filesystem mutation tests ───────────────────────────────────

    #[test]
    fn create_directory_at_supports_nested_validated_paths() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");

        fs.create_directory_at(output_fd, "build").unwrap();
        fs.create_directory_at(output_fd, "./build//cache/")
            .unwrap();

        assert!(output.path().join("build/cache").is_dir());
        assert_eq!(
            fs.stat_at(output_fd, "build/cache")
                .unwrap()
                .descriptor_type,
            DescriptorType::Directory
        );
        assert_eq!(
            fs.create_directory_at(output_fd, "build/cache"),
            Err(FsError::Exist)
        );
        assert_eq!(
            fs.create_directory_at(output_fd, "missing/child"),
            Err(FsError::NoEntry)
        );
        assert_eq!(
            fs.create_directory_at(output_fd, "../escape"),
            Err(FsError::InvalidPath)
        );
    }

    #[test]
    fn mutations_require_directory_mutation_rights() {
        let (mut fs, input, output) = test_fs();
        host_create_dir(&input, "dir");
        host_write_input(&input, "file.txt", b"input");
        host_write_output(&output, "output.txt", b"output");
        let input_fd = preopen_fd(&fs, "/input");
        let output_fd = preopen_fd(&fs, "/output");

        assert_eq!(
            fs.create_directory_at(input_fd, "created"),
            Err(FsError::NotPermitted)
        );
        assert_eq!(
            fs.unlink_file_at(input_fd, "file.txt"),
            Err(FsError::NotPermitted)
        );
        assert_eq!(
            fs.remove_directory_at(input_fd, "dir"),
            Err(FsError::NotPermitted)
        );
        assert_eq!(
            fs.rename_at(input_fd, "file.txt", output_fd, "moved.txt"),
            Err(FsError::NotPermitted)
        );
        assert_eq!(
            fs.rename_at(output_fd, "output.txt", input_fd, "moved.txt"),
            Err(FsError::NotPermitted)
        );
        assert!(input.path().join("file.txt").is_file());
        assert!(output.path().join("output.txt").is_file());
    }

    #[test]
    fn unlink_file_at_invalidates_open_descriptors_and_streams() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_write_output(&output, "delete.txt", b"contents");
        let first = fs
            .open_at(output_fd, "delete.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let second = fs
            .open_at(output_fd, "delete.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let stream = fs.create_read_stream(first, 0).unwrap();

        fs.unlink_file_at(output_fd, "delete.txt").unwrap();

        assert!(!output.path().join("delete.txt").exists());
        assert_eq!(fs.get_type(first), Err(FsError::BadDescriptor));
        assert_eq!(fs.get_type(second), Err(FsError::BadDescriptor));
        assert_eq!(fs.stream_read(stream, 1), Err(FsError::BadDescriptor));
        assert_eq!(
            fs.unlink_file_at(output_fd, "delete.txt"),
            Err(FsError::NoEntry)
        );
    }

    #[test]
    fn unlink_file_at_rejects_directories_and_trailing_slashes() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_create_dir(&output, "dir");
        host_write_output(&output, "file.txt", b"contents");

        assert_eq!(
            fs.unlink_file_at(output_fd, "dir"),
            Err(FsError::IsDirectory)
        );
        assert_eq!(
            fs.unlink_file_at(output_fd, "dir/"),
            Err(FsError::IsDirectory)
        );
        assert_eq!(
            fs.unlink_file_at(output_fd, "file.txt/"),
            Err(FsError::NotDirectory)
        );
        assert!(output.path().join("file.txt").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn unlink_file_at_removes_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_write_output(&output, "target.txt", b"target");
        symlink("target.txt", output.path().join("link.txt")).unwrap();

        fs.unlink_file_at(output_fd, "link.txt").unwrap();

        assert!(!output.path().join("link.txt").exists());
        assert_eq!(
            std::fs::read(output.path().join("target.txt")).unwrap(),
            b"target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mutation_trailing_slash_does_not_follow_a_final_symlink() {
        use std::os::unix::fs::symlink;

        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_create_dir(&output, "target");
        host_create_dir(&output, "source");
        symlink("target", output.path().join("link")).unwrap();

        assert_eq!(
            fs.unlink_file_at(output_fd, "link/"),
            Err(FsError::NotDirectory)
        );
        assert_eq!(
            fs.rename_at(output_fd, "source", output_fd, "link/"),
            Err(FsError::NotDirectory)
        );
        assert!(
            std::fs::symlink_metadata(output.path().join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(output.path().join("source").is_dir());
        assert!(output.path().join("target").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn mutations_cannot_traverse_an_external_directory_symlink() {
        use std::os::unix::fs::symlink;

        let (mut fs, _input, output) = test_fs();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("sentinel.txt"), b"outside").unwrap();
        symlink(outside.path(), output.path().join("outside-link")).unwrap();
        let output_fd = preopen_fd(&fs, "/output");

        assert!(
            fs.create_directory_at(output_fd, "outside-link/created")
                .is_err()
        );
        assert!(
            fs.unlink_file_at(output_fd, "outside-link/sentinel.txt")
                .is_err()
        );
        assert!(
            fs.rename_at(
                output_fd,
                "outside-link/sentinel.txt",
                output_fd,
                "moved.txt",
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(outside.path().join("sentinel.txt")).unwrap(),
            b"outside"
        );
        assert!(!outside.path().join("created").exists());
        assert!(!output.path().join("moved.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rename_at_moves_a_symlink_and_invalidates_followed_descriptors() {
        use std::os::unix::fs::symlink;

        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_write_output(&output, "target.txt", b"target");
        symlink("target.txt", output.path().join("old-link")).unwrap();
        let followed_fd = fs
            .open_at(output_fd, "old-link", OpenFlags::OPEN_EXISTING)
            .unwrap();

        fs.rename_at(output_fd, "old-link", output_fd, "new-link")
            .unwrap();

        assert_eq!(fs.get_type(followed_fd), Err(FsError::BadDescriptor));
        assert!(
            std::fs::symlink_metadata(output.path().join("new-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(output.path().join("new-link")).unwrap(),
            b"target"
        );
    }

    #[test]
    fn remove_directory_at_requires_an_empty_directory_and_invalidates_handles() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_create_dir(&output, "empty");
        host_create_dir(&output, "nonempty");
        host_write_output(&output, "nonempty/file.txt", b"contents");
        host_write_output(&output, "file.txt", b"contents");
        let empty_fd = fs
            .open_at(output_fd, "empty", OpenFlags::DIRECTORY)
            .unwrap();
        let dir_stream = fs.create_dir_stream(empty_fd).unwrap();

        assert_eq!(
            fs.remove_directory_at(output_fd, "nonempty"),
            Err(FsError::NotEmpty)
        );
        assert_eq!(
            fs.remove_directory_at(output_fd, "file.txt"),
            Err(FsError::NotDirectory)
        );
        fs.remove_directory_at(output_fd, "empty").unwrap();

        assert!(!output.path().join("empty").exists());
        assert_eq!(fs.get_type(empty_fd), Err(FsError::BadDescriptor));
        assert!(!fs.has_dir_stream(dir_stream));
    }

    #[test]
    fn rename_at_invalidates_open_source_descriptors_and_streams() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_write_output(&output, "old.txt", b"contents");
        let file_fd = fs
            .open_at(output_fd, "old.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let stream = fs.create_read_stream(file_fd, 0).unwrap();

        fs.rename_at(output_fd, "old.txt", output_fd, "new.txt")
            .unwrap();

        assert!(!output.path().join("old.txt").exists());
        assert_eq!(
            std::fs::read(output.path().join("new.txt")).unwrap(),
            b"contents"
        );
        assert_eq!(fs.get_type(file_fd), Err(FsError::BadDescriptor));
        assert_eq!(fs.stream_read(stream, 4), Err(FsError::BadDescriptor));
        let reopened = fs
            .open_at(output_fd, "new.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(fs.read_file(reopened, 0, 100).unwrap().0, b"contents");
    }

    #[test]
    fn rename_at_same_path_is_a_noop_and_preserves_descriptors() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_write_output(&output, "same.txt", b"contents");
        let file_fd = fs
            .open_at(output_fd, "same.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let stream = fs.create_read_stream(file_fd, 0).unwrap();

        fs.rename_at(output_fd, "same.txt", output_fd, "./same.txt")
            .unwrap();

        assert_eq!(fs.get_type(file_fd), Ok(DescriptorType::RegularFile));
        assert_eq!(fs.stream_read(stream, 4).unwrap(), b"cont");
    }

    #[test]
    fn rename_at_invalidates_an_open_directory_subtree() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_create_dir(&output, "build/cache");
        host_write_output(&output, "build/cache/result.txt", b"result");
        let build_fd = fs
            .open_at(output_fd, "build", OpenFlags::DIRECTORY)
            .unwrap();
        let file_fd = fs
            .open_at(build_fd, "cache/result.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        fs.rename_at(output_fd, "build", output_fd, "archive")
            .unwrap();

        assert_eq!(fs.get_type(build_fd), Err(FsError::BadDescriptor));
        assert_eq!(fs.get_type(file_fd), Err(FsError::BadDescriptor));
        assert_eq!(
            fs.stat_at(output_fd, "archive/cache/result.txt")
                .unwrap()
                .descriptor_type,
            DescriptorType::RegularFile
        );
    }

    #[test]
    fn rename_at_works_between_open_directories_in_the_same_root() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_create_dir(&output, "from");
        host_create_dir(&output, "to");
        host_write_output(&output, "from/file.txt", b"contents");
        let from_fd = fs.open_at(output_fd, "from", OpenFlags::DIRECTORY).unwrap();
        let to_fd = fs.open_at(output_fd, "to", OpenFlags::DIRECTORY).unwrap();
        let file_fd = fs
            .open_at(from_fd, "file.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        fs.rename_at(from_fd, "file.txt", to_fd, "moved.txt")
            .unwrap();

        assert_eq!(fs.get_type(file_fd), Err(FsError::BadDescriptor));
        assert_eq!(fs.file_dir_fd(file_fd), None);
        assert!(output.path().join("to/moved.txt").is_file());
    }

    #[test]
    fn rename_at_invalidates_descriptors_for_a_replaced_destination() {
        let (mut fs, _input, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        host_write_output(&output, "source.txt", b"source");
        host_write_output(&output, "destination.txt", b"destination");
        let source_fd = fs
            .open_at(output_fd, "source.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let destination_fd = fs
            .open_at(output_fd, "destination.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        let destination_stream = fs.create_read_stream(destination_fd, 0).unwrap();

        fs.rename_at(output_fd, "source.txt", output_fd, "destination.txt")
            .unwrap();

        assert_eq!(fs.get_type(source_fd), Err(FsError::BadDescriptor));
        assert_eq!(fs.get_type(destination_fd), Err(FsError::BadDescriptor));
        assert_eq!(
            fs.stream_read(destination_stream, 1),
            Err(FsError::BadDescriptor)
        );
    }

    #[test]
    fn rename_at_rejects_cross_capability_root_moves() {
        let (mut fs, _input, output) = test_fs();
        let other = tempfile::tempdir().unwrap();
        host_write_output(&output, "file.txt", b"contents");
        let other_cap = CapDir::open_ambient_dir(other.path(), ambient_authority()).unwrap();
        let other_fd = fs
            .register_preopen(
                Dir::new(
                    other_cap,
                    DirPerms::READ | DirPerms::MUTATE,
                    FilePerms::READ | FilePerms::WRITE,
                ),
                "/other",
                MountLifetime::Persistent,
            )
            .unwrap();
        let output_fd = preopen_fd(&fs, "/output");

        assert_eq!(
            fs.rename_at(output_fd, "file.txt", other_fd, "file.txt"),
            Err(FsError::CrossDevice)
        );
        assert!(output.path().join("file.txt").is_file());
        assert!(!other.path().join("file.txt").exists());
    }

    #[test]
    fn filesystem_mutations_reject_unsafe_paths_and_bad_descriptors() {
        let (mut fs, _input, _output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");

        assert_eq!(
            fs.unlink_file_at(output_fd, "../escape"),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.remove_directory_at(output_fd, "/absolute"),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.rename_at(output_fd, "old", output_fd, "../../new"),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.create_directory_at(999, "dir"),
            Err(FsError::BadDescriptor)
        );
        assert_eq!(
            fs.remove_directory_at(output_fd, "."),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.rename_at(output_fd, ".", output_fd, "renamed-root"),
            Err(FsError::InvalidPath)
        );
    }

    #[test]
    fn io_errors_are_classified_for_wasi() {
        use std::io::ErrorKind;

        let cases = [
            (ErrorKind::NotFound, FsError::NoEntry),
            (ErrorKind::PermissionDenied, FsError::Access),
            (ErrorKind::AlreadyExists, FsError::Exist),
            (ErrorKind::NotADirectory, FsError::NotDirectory),
            (ErrorKind::IsADirectory, FsError::IsDirectory),
            (ErrorKind::DirectoryNotEmpty, FsError::NotEmpty),
            (ErrorKind::ReadOnlyFilesystem, FsError::ReadOnly),
            (ErrorKind::StorageFull, FsError::InsufficientSpace),
            (ErrorKind::QuotaExceeded, FsError::Quota),
            (ErrorKind::CrossesDevices, FsError::CrossDevice),
        ];

        for (kind, expected) in cases {
            assert_eq!(FsError::from_io(std::io::Error::from(kind)), expected);
        }
    }

    // ── Directory stream tests ──────────────────────────────────────

    #[test]
    fn dir_stream_lists_files() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        fs.open_at(output_fd, "a.txt", OpenFlags::CREATE).unwrap();
        fs.open_at(output_fd, "b.txt", OpenFlags::CREATE).unwrap();

        let stream = fs.create_dir_stream(output_fd).unwrap();
        let mut names = Vec::new();
        while let Some(Some((name, _is_dir))) = fs.read_dir_entry(stream) {
            names.push(name);
        }
        names.sort();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
        // End of stream
        assert_eq!(fs.read_dir_entry(stream), Some(None));
    }

    #[test]
    fn dir_stream_empty_dir() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let stream = fs.create_dir_stream(output_fd).unwrap();
        assert_eq!(fs.read_dir_entry(stream), Some(None));
    }

    #[test]
    fn dir_stream_includes_names_with_http_prefix() {
        let (mut fs, _i, output) = test_fs();
        host_write_output(&output, "__http_cache.txt", b"data");

        let output_fd = preopen_fd(&fs, "/output");
        let stream = fs.create_dir_stream(output_fd).unwrap();
        let mut names = Vec::new();
        while let Some(Some((name, _is_dir))) = fs.read_dir_entry(stream) {
            names.push(name);
        }

        assert!(names.contains(&"__http_cache.txt".to_string()));
    }

    #[test]
    fn dir_stream_errors_when_directory_exceeds_limit() {
        let (mut fs, _i, output) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        fs.limits.max_directory_entries_per_listing = 2;

        for i in 0..=2 {
            host_write_output(&output, &format!("file-{i}.txt"), b"");
        }

        assert_eq!(fs.create_dir_stream(output_fd), Err(FsError::Quota));
    }

    // ── Append stream test ──────────────────────────────────────────

    #[test]
    fn append_stream_appends() {
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");
        let fd = fs.open_at(output_fd, "log.txt", OpenFlags::CREATE).unwrap();
        fs.write_file(fd, 0, b"first").unwrap();

        let as_ = fs.create_append_stream(fd).unwrap();
        fs.stream_write(as_, b" second").unwrap();

        let (data, _) = fs.read_file(fd, 0, 100).unwrap();
        assert_eq!(data, b"first second");
    }

    #[test]
    fn filesystem_limit_defaults_match_the_phase_one_policy() {
        let limits = FilesystemLimits::default();
        assert_eq!(limits.max_open_descriptors, 1_024);
        assert_eq!(limits.max_open_streams, 1_024);
        assert_eq!(limits.max_single_read_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.max_read_bytes_per_run, 256 * 1024 * 1024);
        assert_eq!(limits.max_written_bytes_per_run, 64 * 1024 * 1024);
        assert_eq!(limits.max_creations_per_run, 10_000);
        assert_eq!(limits.max_directory_entries_per_listing, 10_000);
        assert_eq!(limits.max_directory_entries_per_run, 100_000);
        assert_eq!(limits.max_cleanup_entries, 100_000);
        assert_eq!(limits.max_recursive_depth, 64);
    }

    #[test]
    fn open_descriptor_limit_counts_preopens_and_releases_closed_handles() {
        let limits = FilesystemLimits {
            max_open_descriptors: 3,
            ..FilesystemLimits::default()
        };
        let (mut fs, input, _output) = test_fs_with_limits(limits);
        host_write_input(&input, "a.txt", b"a");
        host_write_input(&input, "b.txt", b"b");
        let input_fd = preopen_fd(&fs, "/input");

        let first = fs
            .open_at(input_fd, "a.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();
        assert_eq!(
            fs.open_at(input_fd, "b.txt", OpenFlags::OPEN_EXISTING),
            Err(FsError::Quota)
        );

        fs.close_descriptor(first).unwrap();
        assert!(
            fs.open_at(input_fd, "b.txt", OpenFlags::OPEN_EXISTING)
                .is_ok()
        );
    }

    #[test]
    fn stream_limit_combines_file_and_directory_streams() {
        let limits = FilesystemLimits {
            max_open_streams: 2,
            ..FilesystemLimits::default()
        };
        let (mut fs, input, _output) = test_fs_with_limits(limits);
        host_write_input(&input, "data.txt", b"data");
        let input_fd = preopen_fd(&fs, "/input");
        let file_fd = fs
            .open_at(input_fd, "data.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        let first = fs.create_read_stream(file_fd, 0).unwrap();
        let directory = fs.create_dir_stream(input_fd).unwrap();
        assert_eq!(fs.create_read_stream(file_fd, 0), Err(FsError::Quota));

        fs.close_stream(first);
        assert!(fs.create_read_stream(file_fd, 0).is_ok());
        fs.close_dir_stream(directory);
    }

    #[test]
    fn read_limits_enforce_single_and_total_bytes_and_reset_per_run() {
        let limits = FilesystemLimits {
            max_single_read_bytes: 3,
            max_read_bytes_per_run: 5,
            ..FilesystemLimits::default()
        };
        let (mut fs, input, _output) = test_fs_with_limits(limits);
        host_write_input(&input, "data.txt", b"abcdef");
        let input_fd = preopen_fd(&fs, "/input");
        let file_fd = fs
            .open_at(input_fd, "data.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        assert_eq!(fs.read_file(file_fd, 0, 100).unwrap().0, b"abc");
        assert_eq!(fs.read_file(file_fd, 3, 2).unwrap().0, b"de");
        assert_eq!(fs.read_file(file_fd, 5, 1), Err(FsError::Quota));

        fs.prepare_for_run().unwrap();
        assert_eq!(fs.read_file(file_fd, 5, 1).unwrap().0, b"f");
        assert!(fs.read_guest_file("/input/data.txt").is_err());
    }

    #[test]
    fn helper_and_descriptor_reads_share_one_per_run_budget() {
        let limits = FilesystemLimits {
            max_single_read_bytes: 8,
            max_read_bytes_per_run: 5,
            ..FilesystemLimits::default()
        };
        let (mut fs, input, _output) = test_fs_with_limits(limits);
        host_write_input(&input, "first.txt", b"abc");
        host_write_input(&input, "second.txt", b"def");
        let input_fd = preopen_fd(&fs, "/input");
        let first = fs
            .open_at(input_fd, "first.txt", OpenFlags::OPEN_EXISTING)
            .unwrap();

        assert_eq!(fs.read_file(first, 0, 3).unwrap().0, b"abc");
        assert!(fs.read_guest_file("/input/second.txt").is_err());
    }

    #[test]
    fn helper_and_descriptor_writes_share_one_per_run_budget() {
        let limits = FilesystemLimits {
            max_written_bytes_per_run: 5,
            ..FilesystemLimits::default()
        };
        let (mut fs, _input, output) = test_fs_with_limits(limits);
        let output_fd = preopen_fd(&fs, "/output");
        let file_fd = fs
            .open_at(output_fd, "first.txt", OpenFlags::CREATE)
            .unwrap();

        fs.write_file(file_fd, 0, b"abc").unwrap();
        assert!(
            fs.write_output_path("/output/second.txt", b"def".to_vec())
                .is_err()
        );
        assert_eq!(
            std::fs::read(output.path().join("first.txt")).unwrap(),
            b"abc"
        );
        assert!(!output.path().join("second.txt").exists());
    }

    #[test]
    fn creation_budget_resets_only_after_successful_run_preparation() {
        let limits = FilesystemLimits {
            max_creations_per_run: 1,
            ..FilesystemLimits::default()
        };
        let (mut fs, _input, output) = test_fs_with_limits(limits);
        let output_fd = preopen_fd(&fs, "/output");

        fs.open_at(output_fd, "first.txt", OpenFlags::CREATE)
            .unwrap();
        assert_eq!(
            fs.create_directory_at(output_fd, "blocked"),
            Err(FsError::Quota)
        );

        fs.prepare_for_run().unwrap();
        fs.create_directory_at(output_fd, "allowed").unwrap();
        assert!(output.path().join("allowed").is_dir());
    }

    #[test]
    fn directory_entry_budget_is_cumulative_within_a_run() {
        let limits = FilesystemLimits {
            max_directory_entries_per_listing: 10,
            max_directory_entries_per_run: 2,
            ..FilesystemLimits::default()
        };
        let (mut fs, input, _output) = test_fs_with_limits(limits);
        host_write_input(&input, "a.txt", b"a");
        host_write_input(&input, "b.txt", b"b");
        let input_fd = preopen_fd(&fs, "/input");

        let stream = fs.create_dir_stream(input_fd).unwrap();
        fs.close_dir_stream(stream);
        assert_eq!(fs.create_dir_stream(input_fd), Err(FsError::Quota));

        fs.prepare_for_run().unwrap();
        assert!(fs.create_dir_stream(input_fd).is_ok());
    }

    #[test]
    fn alloc_handle_overflow_returns_error() {
        let mut fs = CapFs::new();
        fs.next_handle = u32::MAX;

        assert_eq!(
            fs.alloc_handle(),
            Err(FsError::Io("file descriptor handle space exhausted".into()))
        );
    }

    #[test]
    fn work_mount_can_be_read_only() {
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("existing.txt"), b"hello").unwrap();

        let fs = CapFs::new()
            .with_work(work.path(), WorkDirAccess::ReadOnly)
            .unwrap();

        let work_fd = fs
            .preopen_dirs
            .iter()
            .find_map(|(&fd, entry)| (entry.guest_path == "/work").then_some(fd))
            .unwrap();

        assert_eq!(
            fs.preopen_dirs[&work_fd].lifetime,
            MountLifetime::Persistent
        );
    }

    #[test]
    fn persistent_work_mount_is_not_cleared() {
        let work = tempfile::tempdir().unwrap();

        let mut fs = CapFs::new()
            .with_work(work.path(), WorkDirAccess::ReadWrite)
            .unwrap();

        std::fs::write(work.path().join("persistent.txt"), b"survives").unwrap();

        fs.prepare_for_run().unwrap();

        assert_eq!(
            std::fs::read(work.path().join("persistent.txt")).unwrap(),
            b"survives"
        );
    }

    //This test verifies three important properties:
    // /work contents survive cleanup.
    // /tmp contents are deleted.
    // Descriptors under /tmp are invalidated, while /work descriptors remain valid.
    #[test]
    fn temp_mount_is_cleared_but_work_mount_persists() {
        let work = tempfile::tempdir().unwrap();

        let mut fs = CapFs::new()
            .with_work(work.path(), WorkDirAccess::ReadWrite)
            .unwrap()
            .with_temp_dir()
            .unwrap();

        let work_fd = preopen_fd(&fs, "/work");
        let temp_fd = preopen_fd(&fs, "/tmp");

        let work_file = fs.open_at(work_fd, "persistent.txt", OpenFlags::CREATE).unwrap();
        fs.write_file(work_file, 0, b"survives").unwrap();

        let temp_file = fs.open_at(temp_fd, "temporary.txt", OpenFlags::CREATE).unwrap();
        fs.write_file(temp_file, 0, b"survives").unwrap();

        fs.prepare_for_run().unwrap();

        assert!(work.path().join("persistent.txt").exists());

        let temp_path = fs.temp_paths.as_ref().unwrap();
        assert!(!temp_path.join("temporary.txt").exists());

        assert_eq!(
            fs.get_type(work_file),
            Ok(DescriptorType::RegularFile)
        );
        assert_eq!(
            fs.get_type(temp_file),
            Err(FsError::BadDescriptor)
        );

    }


    // Then add a test proving that both ephemeral mounts are cleaned:
    #[test]
    fn all_ephemeral_mounts_are_cleared() {
        let work = tempfile::tempdir().unwrap();

        let mut fs = CapFs::new()
            .with_output_dir(work.path(),
            DirPerms::READ | DirPerms::MUTATE,
            FilePerms::READ | FilePerms::WRITE,
            )
            .unwrap()
            .with_temp_dir()
            .unwrap();
        let output_fd = preopen_fd(&fs, "/output");
        let temp_fd = preopen_fd(&fs, "/tmp");

        let output_file = fs.open_at(output_fd, "output.txt", OpenFlags::CREATE).unwrap();
        fs.write_file(output_file, 0, b"survives").unwrap();

        let temp_file = fs.open_at(temp_fd, "temporary.txt", OpenFlags::CREATE).unwrap();
        fs.write_file(temp_file, 0, b"survives").unwrap();

        fs.clear_ephemeral_mounts().unwrap();

        // output file does not exist
        assert!(!work.path().join("output.txt").exists());
        // temp file does not exist
        assert!(!fs.temp_paths.as_ref().unwrap().join("temporary.txt").exists());
    }
            
}
