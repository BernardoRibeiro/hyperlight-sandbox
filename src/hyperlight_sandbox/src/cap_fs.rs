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
//!   permissions, exposed as a writable WASI preopen.  Wiped clean after
//!   each run.
//!
//! Snapshots only capture runtime state — input is immutable and output is
//! ephemeral, so no filesystem state needs to be saved or restored.

use std::collections::HashMap;
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
    BadDescriptor,
    NotPermitted,
    NoEntry,
    InvalidPath,
    Io(String),
}

// ---------------------------------------------------------------------------
// Metadata types
// ---------------------------------------------------------------------------

/// Type of a filesystem descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorType {
    Directory,
    RegularFile,
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

struct DescriptorEntry {
    root_fd: u32,
    relative_path: PathBuf,
    descriptor_type: DescriptorType,
    is_preopen: bool,
    parent_fd: Option<u32>,
    directory: Option<Dir>,
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

// ---------------------------------------------------------------------------
// CapFs
// ---------------------------------------------------------------------------

/// A preopened directory registered with the filesystem.
#[derive(Clone)]
struct PreopenEntry {
    dir: Dir,
    guest_path: String,
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

    // Host filesystem path to the output directory.
    output_path: Option<PathBuf>,
    // Owns the output temp dir when using the default.
    _output_tmp: Option<tempfile::TempDir>,
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
            output_path: None,
            _output_tmp: None,
        }
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
            .register_preopen(Dir::new(output_cap, dir_perms, file_perms), "/output")
            .map_err(|err| anyhow::anyhow!("{err:?}"))?;
        self.output_fd = Some(fd);
        self.output_path = Some(output_path.as_ref().to_path_buf());
        Ok(self)
    }

    // -----------------------------------------------------------------------
    // Host-side output API
    // -----------------------------------------------------------------------

    pub fn write_output_path(&mut self, path: &str, data: Vec<u8>) -> Result<()> {
        let key = Self::normalize_path(path, "output")?;
        let output_fd = self
            .output_fd
            .ok_or_else(|| anyhow::anyhow!("no output directory configured"))?;
        let output = &self.preopen_dirs[&output_fd];
        if !output.dir.perms().contains(DirPerms::MUTATE) {
            anyhow::bail!("write permission denied on output directory");
        }
        if !output.dir.file_perms().contains(FilePerms::WRITE) {
            anyhow::bail!("write permission denied on output files");
        }
        let mut file = output
            .dir
            .cap_std()
            .create(&key)
            .with_context(|| format!("failed to create output file: {key}"))?;
        file.write_all(&data)
            .with_context(|| format!("failed to write output file: {key}"))?;
        Ok(())
    }

    /// Maximum size (in bytes) for a single file read into memory.
    const MAX_FILE_READ_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

    /// Read a file from a preopened directory by guest path (e.g. "/input/data.txt").
    pub fn read_guest_file(&self, guest_path: &str) -> Result<Vec<u8>> {
        // Determine which preopen and filename from the path.
        let trimmed = guest_path.trim().trim_start_matches('/');
        let (dir_name, file_name) = trimmed.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("path must include a directory and filename: {guest_path}")
        })?;
        let preopen_path = format!("/{dir_name}");
        let dir = self
            .dir_by_guest_path(&preopen_path)
            .ok_or_else(|| anyhow::anyhow!("no preopen for {preopen_path}"))?;
        if !dir.perms().contains(DirPerms::READ) {
            anyhow::bail!("read permission denied on {preopen_path}");
        }
        if !dir.file_perms().contains(FilePerms::READ) {
            anyhow::bail!("read permission denied on files in {preopen_path}");
        }
        let file = dir
            .cap_std()
            .open(file_name)
            .map_err(|_| anyhow::anyhow!("file not found: {guest_path}"))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to stat: {guest_path}"))?;
        if metadata.len() > Self::MAX_FILE_READ_BYTES {
            anyhow::bail!(
                "file too large ({} bytes, max {} bytes): {guest_path}",
                metadata.len(),
                Self::MAX_FILE_READ_BYTES,
            );
        }
        let mut data = Vec::new();
        file.take(Self::MAX_FILE_READ_BYTES)
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

    /// Wipe output files and reset all handles/streams. Input is untouched.
    pub fn clear_output_files(&mut self) {
        let Some(output_fd) = self.output_fd else {
            return;
        };
        if let Some(output) = self.preopen_dirs.get(&output_fd) {
            let dir = output.dir.cap_std();
            if let Ok(entries) = dir.entries() {
                for entry in entries.flatten() {
                    let _ = dir.remove_file(entry.file_name());
                }
            }
        }
        self.descriptors
            .retain(|_, e| e.root_fd != output_fd || e.is_preopen);
        self.streams.clear();
        self.dir_streams.clear();
        self.next_handle = self
            .descriptors
            .keys()
            .copied()
            .max()
            .map(|h| h + 1)
            .unwrap_or(FIRST_PREOPEN_FD);
    }

    /// Clear output directory. Input is host-managed and left untouched.
    pub fn clear(&mut self) {
        self.clear_output_files();
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
        Some(dir.cap_std().metadata(&entry.relative_path).ok()?.len())
    }

    pub fn find_file_in_dir(&self, dir_fd: u32, name: &str) -> Option<u32> {
        for (&fd, entry) in &self.descriptors {
            if entry.descriptor_type != DescriptorType::RegularFile {
                continue;
            }
            if entry.parent_fd == Some(dir_fd)
                && entry.relative_path.file_name().and_then(|n| n.to_str()) == Some(name)
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
        let dir = self.get_dir(dir_fd).ok_or(FsError::BadDescriptor)?;
        if !dir.perms().contains(DirPerms::READ) {
            return Err(FsError::NotPermitted);
        }
        let metadata = dir.cap_std().metadata(path).map_err(|_| FsError::NoEntry)?;
        let descriptor_type = if metadata.is_dir() {
            DescriptorType::Directory
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

    /// Maximum number of open descendant descriptors.
    const MAX_OPEN_DESCRIPTORS: usize = 1024;

    /// Cap single read allocation to prevent guest-triggered OOM.
    const MAX_READ_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

    pub fn open_at(&mut self, dir_fd: u32, path: &str, flags: OpenFlags) -> Result<u32, FsError> {
        let (root_fd, relative_path) = self.resolve_at(dir_fd, path)?;
        let dir = self
            .get_dir(dir_fd)
            .cloned()
            .ok_or(FsError::BadDescriptor)?;

        let create = flags.contains(OpenFlags::CREATE);
        let truncate = flags.contains(OpenFlags::TRUNCATE);
        let require_directory = flags.contains(OpenFlags::DIRECTORY);

        if (create || truncate) && !dir.perms().contains(DirPerms::MUTATE) {
            return Err(FsError::NotPermitted);
        }

        let metadata = dir.cap_std().metadata(path).ok();
        let exists = metadata.is_some();
        if !exists && !create {
            return Err(FsError::NoEntry);
        }
        if require_directory && !metadata.as_ref().is_some_and(|meta| meta.is_dir()) {
            return Err(FsError::NoEntry);
        }
        if truncate && metadata.as_ref().is_some_and(|meta| meta.is_dir()) {
            return Err(FsError::InvalidPath);
        }
        if self
            .descriptors
            .values()
            .filter(|entry| !entry.is_preopen)
            .count()
            >= Self::MAX_OPEN_DESCRIPTORS
        {
            return Err(FsError::Io("too many open descriptors".into()));
        }
        if !exists || truncate {
            dir.cap_std()
                .create(path)
                .map_err(|e| FsError::Io(e.to_string()))?;
        }

        let metadata = dir
            .cap_std()
            .metadata(path)
            .map_err(|e| FsError::Io(e.to_string()))?;
        let descriptor_type = if metadata.is_dir() {
            DescriptorType::Directory
        } else {
            DescriptorType::RegularFile
        };
        let directory = if descriptor_type == DescriptorType::Directory {
            let opened = dir
                .cap_std()
                .open_dir(path)
                .map_err(|e| FsError::Io(e.to_string()))?;
            Some(Dir::new(opened, dir.perms(), dir.file_perms()))
        } else {
            None
        };

        let fd = self.alloc_handle()?;
        self.descriptors.insert(
            fd,
            DescriptorEntry {
                root_fd,
                relative_path,
                descriptor_type,
                is_preopen: false,
                parent_fd: Some(dir_fd),
                directory,
            },
        );
        Ok(fd)
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

    pub fn read_file(&self, fd: u32, offset: u64, len: u64) -> Result<(Vec<u8>, bool), FsError> {
        let (root, path) = self.file_parts(fd)?;
        if !root.file_perms().contains(FilePerms::READ) {
            return Err(FsError::NotPermitted);
        }
        let mut file = root
            .cap_std()
            .open(path)
            .map_err(|e| FsError::Io(e.to_string()))?;
        let file_size = file
            .metadata()
            .map_err(|e| FsError::Io(e.to_string()))?
            .len();

        let start = offset.min(file_size);
        let remaining = file_size - start;
        let to_read = len.min(remaining).min(Self::MAX_READ_BYTES as u64) as usize;
        if to_read == 0 {
            return Ok((Vec::new(), true));
        }
        file.seek(SeekFrom::Start(start))
            .map_err(|e| FsError::Io(e.to_string()))?;
        let mut buf = vec![0u8; to_read];
        let n = file
            .read(&mut buf)
            .map_err(|e| FsError::Io(e.to_string()))?;
        buf.truncate(n);
        let eof = start + n as u64 >= file_size;
        Ok((buf, eof))
    }

    pub fn write_file(&mut self, fd: u32, offset: u64, buffer: &[u8]) -> Result<u64, FsError> {
        let (root, path) = self.file_parts(fd)?;
        if !root.file_perms().contains(FilePerms::WRITE) {
            return Err(FsError::NotPermitted);
        }
        let mut opts = cap_std::fs::OpenOptions::new();
        opts.read(true).write(true);
        let mut file = root
            .cap_std()
            .open_with(path, &opts)
            .map_err(|e| FsError::Io(e.to_string()))?;

        let file_size = file
            .metadata()
            .map_err(|e| FsError::Io(e.to_string()))?
            .len();
        let new_end = offset
            .checked_add(buffer.len() as u64)
            .ok_or(FsError::Io("integer overflow".into()))?;
        if new_end > file_size {
            file.set_len(new_end)
                .map_err(|e| FsError::Io(e.to_string()))?;
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FsError::Io(e.to_string()))?;
        file.write_all(buffer)
            .map_err(|e| FsError::Io(e.to_string()))?;
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

        let (root, path) = self.file_parts(file_fd)?;
        let mut file = root
            .cap_std()
            .open(path)
            .map_err(|e| FsError::Io(e.to_string()))?;
        let file_size = file
            .metadata()
            .map_err(|e| FsError::Io(e.to_string()))?
            .len();
        if offset >= file_size {
            return Err(FsError::Io("stream read past end of file".into()));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| FsError::Io(e.to_string()))?;
        let remaining = file_size - offset;
        let to_read = len.min(remaining).min(Self::MAX_READ_BYTES as u64) as usize;
        let mut buf = vec![0u8; to_read];
        let n = file
            .read(&mut buf)
            .map_err(|e| FsError::Io(e.to_string()))?;
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

    /// Maximum number of entries materialized from a single directory listing.
    const MAX_DIR_ENTRIES: usize = 10_000;

    pub fn create_dir_stream(&mut self, dir_fd: u32) -> Result<u32, FsError> {
        let dir = self.get_dir(dir_fd).ok_or(FsError::BadDescriptor)?;
        if !dir.perms().contains(DirPerms::READ) {
            return Err(FsError::NotPermitted);
        }
        let mut file_entries = Vec::new();
        if let Ok(entries) = dir.cap_std().entries() {
            for entry in entries.flatten() {
                if file_entries.len() >= Self::MAX_DIR_ENTRIES {
                    return Err(FsError::Io("directory listing exceeds entry limit".into()));
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());
                file_entries.push((name, is_dir));
            }
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

    fn register_preopen(&mut self, dir: Dir, guest_path: &str) -> Result<u32, FsError> {
        let fd = self.alloc_handle()?;
        self.preopen_dirs.insert(
            fd,
            PreopenEntry {
                dir: dir.clone(),
                guest_path: guest_path.to_string(),
            },
        );
        self.descriptors.insert(
            fd,
            DescriptorEntry {
                root_fd: fd,
                relative_path: PathBuf::new(),
                descriptor_type: DescriptorType::Directory,
                is_preopen: true,
                parent_fd: None,
                directory: Some(dir),
            },
        );
        Ok(fd)
    }

    fn validate_relative_component(path: &str) -> Result<(), FsError> {
        if path.is_empty()
            || path == "."
            || path == ".."
            || path.contains('/')
            || path.contains('\\')
            || path.contains('\0')
        {
            return Err(FsError::InvalidPath);
        }
        Ok(())
    }

    /// Resolve one path component relative to an opened directory while
    /// retaining the descriptor's preopen-relative identity.
    fn resolve_at(&self, dir_fd: u32, path: &str) -> Result<(u32, PathBuf), FsError> {
        Self::validate_relative_component(path)?;
        let parent = self
            .descriptors
            .get(&dir_fd)
            .ok_or(FsError::BadDescriptor)?;
        if parent.descriptor_type != DescriptorType::Directory || parent.directory.is_none() {
            return Err(FsError::BadDescriptor);
        }
        let mut relative_path = parent.relative_path.clone();
        relative_path.push(path);
        Ok((parent.root_fd, relative_path))
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

    fn normalize_path(path: &str, root: &str) -> Result<String> {
        let trimmed = path.trim();
        let without_leading = trimmed.trim_start_matches('/');
        let Some(remainder) = without_leading.strip_prefix(root) else {
            return Err(anyhow::anyhow!("path must be rooted at /{root}: {trimmed}"));
        };
        let Some(normalized) = remainder.strip_prefix('/') else {
            return Err(anyhow::anyhow!(
                "path must include a file name under /{root}: {trimmed}"
            ));
        };
        if normalized.is_empty() {
            return Err(anyhow::anyhow!(
                "path must include a file name under /{root}: {trimmed}"
            ));
        }
        for component in normalized.split('/') {
            if component.is_empty() || component == "." || component == ".." {
                return Err(anyhow::anyhow!("path contains unsafe component: {trimmed}"));
            }
            if component.contains('\\') {
                return Err(anyhow::anyhow!(
                    "path contains unsupported separator: {trimmed}"
                ));
            }
        }
        Ok(normalized.to_string())
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
    use super::*;

    fn test_fs() -> (CapFs, tempfile::TempDir, tempfile::TempDir) {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let fs = CapFs::new()
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

        fs.clear_output_files();

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

        fs.clear_output_files();

        assert_eq!(fs.get_type(input_dir), Ok(DescriptorType::Directory));
        assert_eq!(fs.read_file(input_file, 0, 100).unwrap().0, b"input");
        assert_eq!(fs.get_type(output_dir), Err(FsError::BadDescriptor));
        assert_eq!(fs.get_type(output_file), Err(FsError::BadDescriptor));
        assert_eq!(fs.get_type(output_fd), Ok(DescriptorType::Directory));
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
        let (mut fs, _i, _o) = test_fs();
        let output_fd = preopen_fd(&fs, "/output");

        // Path traversal
        assert_eq!(
            fs.open_at(output_fd, "../x", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "../../etc/passwd", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // Nested paths (only single-segment names allowed)
        assert_eq!(
            fs.open_at(output_fd, "a/b", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "dir/file.txt", OpenFlags::CREATE),
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

        // Leading/trailing slashes
        assert_eq!(
            fs.open_at(output_fd, "/absolute", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );
        assert_eq!(
            fs.open_at(output_fd, "trailing/", OpenFlags::CREATE),
            Err(FsError::InvalidPath)
        );

        // Invalid descriptor
        assert_eq!(
            fs.open_at(999, "file.txt", OpenFlags::CREATE),
            Err(FsError::BadDescriptor)
        );

        // Valid names should work
        assert!(fs.open_at(output_fd, "file.txt", OpenFlags::CREATE).is_ok());
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
        let (fs, input, _o) = test_fs();
        host_write_input(&input, "hello.txt", b"world");
        let data = fs.read_guest_file("/input/hello.txt").unwrap();
        assert_eq!(data, b"world");
    }

    #[test]
    fn read_guest_file_denied_without_read_perm() {
        let output = tempfile::tempdir().unwrap();
        std::fs::write(output.path().join("f.txt"), b"data").unwrap();
        let fs = CapFs::new()
            .with_output_dir(output.path(), DirPerms::MUTATE, FilePerms::WRITE)
            .unwrap();
        assert!(fs.read_guest_file("/output/f.txt").is_err());
    }

    #[test]
    fn read_guest_file_missing() {
        let (fs, _i, _o) = test_fs();
        assert!(fs.read_guest_file("/input/nope.txt").is_err());
    }

    #[test]
    fn read_guest_file_outside_preopens() {
        let (fs, _i, _o) = test_fs();
        assert!(fs.read_guest_file("/etc/passwd").is_err());
        assert!(fs.read_guest_file("/tmp/something").is_err());
        assert!(fs.read_guest_file("/secret/data.txt").is_err());
    }

    #[test]
    fn read_guest_file_traversal() {
        let (fs, _i, _o) = test_fs();
        assert!(fs.read_guest_file("/input/../etc/passwd").is_err());
        assert!(fs.read_guest_file("/input/../../root/.ssh/id_rsa").is_err());
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

        for i in 0..=CapFs::MAX_DIR_ENTRIES {
            host_write_output(&output, &format!("file-{i}.txt"), b"");
        }

        assert!(matches!(
            fs.create_dir_stream(output_fd),
            Err(FsError::Io(_))
        ));
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
    fn alloc_handle_overflow_returns_error() {
        let mut fs = CapFs::new();
        fs.next_handle = u32::MAX;

        assert_eq!(
            fs.alloc_handle(),
            Err(FsError::Io("file descriptor handle space exhausted".into()))
        );
    }
}
