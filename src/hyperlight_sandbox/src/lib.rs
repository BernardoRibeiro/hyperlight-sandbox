//! High-level host library for running sandbox guests across multiple backends.

extern crate alloc;

pub mod cap_fs;
pub mod http;
pub mod network;
pub mod runtime;
#[cfg(feature = "test-utils")]
pub mod test_utils;
pub mod tools;

use std::path::{Path, PathBuf};

use anyhow::Result;
pub use cap_fs::{
    CapFs, DescriptorFlags, DescriptorStat, DescriptorType, Dir, DirPerms, FilePerms,
    FilesystemLimits, FsError, MountLifetime, OpenFlags, WorkDirAccess,
};
pub use network::{HttpMethod, MethodFilter, NetworkPermission, NetworkPermissions};
use serde::{Deserialize, Serialize};
pub use tools::{ArgType, ToolRegistry, ToolSchema};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default guest heap size in bytes (platform-dependent).
#[cfg(windows)]
pub const DEFAULT_HEAP_SIZE: u64 = 400 * 1024 * 1024;
#[cfg(not(windows))]
pub const DEFAULT_HEAP_SIZE: u64 = 25 * 1024 * 1024;

/// Default guest stack / scratch size in bytes (platform-dependent).
#[cfg(windows)]
pub const DEFAULT_STACK_SIZE: u64 = 200 * 1024 * 1024;
#[cfg(not(windows))]
pub const DEFAULT_STACK_SIZE: u64 = 35 * 1024 * 1024;

/// Configuration for building a sandbox guest.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Path to the AOT-compiled Wasm component (e.g. `python-sandbox.aot`).
    pub module_path: String,
    /// Guest heap size in bytes.
    pub heap_size: u64,
    /// Guest scratch / stack size in bytes.
    pub stack_size: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            module_path: String::new(),
            heap_size: DEFAULT_HEAP_SIZE,
            stack_size: DEFAULT_STACK_SIZE,
        }
    }
}

// ---------------------------------------------------------------------------
// Execution result
// ---------------------------------------------------------------------------

/// The result of executing code inside the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

pub struct Snapshot<T> {
    kind: &'static str,
    snapshot: std::sync::Arc<T>,
}

impl<T> Clone for Snapshot<T> {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            snapshot: self.snapshot.clone(),
        }
    }
}

impl<T> Snapshot<T> {
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    pub fn new(kind: &'static str, snapshot: std::sync::Arc<T>) -> Self {
        Self { kind, snapshot }
    }

    pub fn snapshot(&self) -> &std::sync::Arc<T> {
        &self.snapshot
    }
}

// ---------------------------------------------------------------------------
// Guest traits
// ---------------------------------------------------------------------------

pub trait Guest: Sized {
    type Sandbox: GuestSandbox;
    fn build(
        self,
        config: SandboxConfig,
        tools: ToolRegistry,
        network: std::sync::Arc<std::sync::Mutex<NetworkPermissions>>,
        fs: std::sync::Arc<std::sync::Mutex<CapFs>>,
    ) -> Result<Self::Sandbox>;
}

pub trait GuestSandbox: Send {
    type SnapshotData: Send + Sync + 'static;
    /// Execute guest code.
    ///
    /// Ephemeral files under `/output` and `/tmp` are wiped before each
    /// execution. Persistent `/input` and `/work` mounts are preserved.
    fn run(&mut self, code: &str) -> Result<ExecutionResult>;
    /// Capture a snapshot of the guest runtime state.
    fn snapshot(&mut self) -> Result<Snapshot<Self::SnapshotData>>;
    /// Restore a previously captured guest runtime state.
    fn restore(&mut self, snapshot: &Snapshot<Self::SnapshotData>) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

pub struct Sandbox<G: Guest> {
    inner: G::Sandbox,
    network: std::sync::Arc<std::sync::Mutex<NetworkPermissions>>,
    fs: std::sync::Arc<std::sync::Mutex<CapFs>>,
}

impl<G: Guest> Sandbox<G> {
    /// Create a sandbox without filesystem access.
    pub fn new(guest: G, config: SandboxConfig, tools: ToolRegistry) -> Result<Self> {
        let network = std::sync::Arc::new(std::sync::Mutex::new(NetworkPermissions::new()));
        let fs = std::sync::Arc::new(std::sync::Mutex::new(CapFs::new()));
        let inner = guest.build(config, tools, network.clone(), fs.clone())?;
        Ok(Self { inner, network, fs })
    }

    /// Create a sandbox with a read-only input directory.
    pub fn with_input(
        guest: G,
        config: SandboxConfig,
        tools: ToolRegistry,
        input_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let network = std::sync::Arc::new(std::sync::Mutex::new(NetworkPermissions::new()));
        let fs = CapFs::new().with_input(input_dir)?;
        let fs = std::sync::Arc::new(std::sync::Mutex::new(fs));
        let inner = guest.build(config, tools, network.clone(), fs.clone())?;
        Ok(Self { inner, network, fs })
    }

    /// Execute guest code.
    ///
    /// Ephemeral `/output` and `/tmp` mounts are cleared before each run.
    /// Persistent `/input` and `/work` mounts are preserved.
    pub fn run(&mut self, code: &str) -> Result<ExecutionResult> {
        self.inner.run(code)
    }

    pub fn snapshot(&mut self) -> Result<Snapshot<<G::Sandbox as GuestSandbox>::SnapshotData>> {
        self.inner.snapshot()
    }

    pub fn restore(
        &mut self,
        snapshot: &Snapshot<<G::Sandbox as GuestSandbox>::SnapshotData>,
    ) -> Result<()> {
        // Runtime state is rewound by the backend. Host filesystem state is
        // governed separately by each preopen's MountLifetime: ephemeral
        // mounts are cleared and persistent mounts such as /work are kept.
        self.inner.restore(snapshot)?;
        self.fs
            .lock()
            .map_err(|_| anyhow::anyhow!("filesystem mutex poisoned during snapshot restore"))?
            .prepare_for_run()
            .map_err(|error| {
                anyhow::anyhow!("failed to reset filesystem after restore: {error:?}")
            })?;
        Ok(())
    }

    /// List filenames in the output directory (without reading contents).
    pub fn get_output_files(&self) -> Result<Vec<String>> {
        Ok(self
            .fs
            .lock()
            .map_err(|_| anyhow::anyhow!("filesystem mutex poisoned"))?
            .get_output_files())
    }

    /// Return the host filesystem path of the output directory, if configured.
    pub fn output_path(&self) -> Result<Option<std::path::PathBuf>> {
        Ok(self
            .fs
            .lock()
            .map_err(|_| anyhow::anyhow!("filesystem mutex poisoned"))?
            .output_path()
            .map(|p| p.to_path_buf()))
    }

    pub fn allow_domain(&mut self, target: &str, methods: impl Into<MethodFilter>) -> Result<()> {
        self.network
            .lock()
            .map_err(|_| anyhow::anyhow!("network mutex poisoned"))?
            .allow_domain(target, methods)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Typestate marker indicating no guest backend has been selected yet.
/// Prevents calling `.build()` before `.guest(...)`.
pub struct NoGuest;

/// Builder for constructing a [`Sandbox`].
///
/// ```rust,ignore
/// let sandbox = SandboxBuilder::new()
///     .module_path("guest.aot")
///     .output_dir("/tmp/sandbox-out")
///     .guest(Wasm)
///     .build()?;
/// ```
pub struct SandboxBuilder<G = NoGuest> {
    guest: G,
    config: SandboxConfig,
    tools: ToolRegistry,
    input_dir: Option<PathBuf>,
    output_dir: Option<(PathBuf, DirPerms, FilePerms)>,
    temp_output: bool,
    work_dir: Option<(PathBuf, WorkDirAccess)>,
    temp_dir: bool,
    filesystem_limits: FilesystemLimits,
}

impl SandboxBuilder<NoGuest> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for SandboxBuilder<NoGuest> {
    fn default() -> Self {
        Self {
            guest: NoGuest,
            config: SandboxConfig::default(),
            tools: ToolRegistry::default(),
            input_dir: None,
            output_dir: None,
            temp_output: false,
            work_dir: None,
            temp_dir: false,
            filesystem_limits: FilesystemLimits::default(),
        }
    }
}

impl<G> SandboxBuilder<G> {
    pub fn module_path(mut self, module_path: impl Into<String>) -> Self {
        self.config.module_path = module_path.into();
        self
    }

    pub fn heap_size(mut self, heap_size: u64) -> Self {
        self.config.heap_size = heap_size;
        self
    }

    pub fn stack_size(mut self, stack_size: u64) -> Self {
        self.config.stack_size = stack_size;
        self
    }

    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    pub fn tool_typed<T, F>(mut self, name: &str, handler: F) -> Self
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: Fn(T) -> Result<serde_json::Value> + Send + Sync + 'static,
    {
        self.tools.register_typed::<T, F>(name, handler);
        self
    }

    /// Set the host directory exposed as the read-only `/input` preopen.
    pub fn input_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.input_dir = Some(path.into());
        self
    }

    /// Set the host directory exposed as the writable `/output` preopen,
    /// with explicit permissions. Without this, output uses a temp directory
    /// with full read-write access.
    pub fn output_dir(
        mut self,
        path: impl Into<PathBuf>,
        dir_perms: DirPerms,
        file_perms: FilePerms,
    ) -> Self {
        self.output_dir = Some((path.into(), dir_perms, file_perms));
        self
    }

    /// Enable a temporary writable `/output` directory. Ignored when an
    /// explicit `output_dir` is set.
    pub fn temp_output(mut self) -> Self {
        self.temp_output = true;
        self
    }

    /// Set the host directory exposed as the persistent `/work` preopen.
    ///
    /// Callers must choose the access mode explicitly. Prefer
    /// [`WorkDirAccess::ReadOnly`] unless guest writes are required.
    pub fn work_dir(mut self, path: impl Into<PathBuf>, access: WorkDirAccess) -> Self {
        self.work_dir = Some((path.into(), access));
        self
    }

    /// Enable a private writable `/tmp` preopen.
    ///
    /// Its backing directory is owned by the sandbox and recursively cleared
    /// before every run and after snapshot restore.
    pub fn temp_dir(mut self) -> Self {
        self.temp_dir = true;
        self
    }

    /// Configure filesystem resource limits and per-run quotas.
    pub fn filesystem_limits(mut self, limits: FilesystemLimits) -> Self {
        self.filesystem_limits = limits;
        self
    }
}

impl SandboxBuilder<NoGuest> {
    pub fn guest<G>(self, guest: G) -> SandboxBuilder<G>
    where
        G: Guest,
    {
        SandboxBuilder {
            guest,
            config: self.config,
            tools: self.tools,
            input_dir: self.input_dir,
            output_dir: self.output_dir,
            temp_output: self.temp_output,
            work_dir: self.work_dir,
            temp_dir: self.temp_dir,
            filesystem_limits: self.filesystem_limits,
        }
    }
}

impl<G> SandboxBuilder<G>
where
    G: Guest,
{
    pub fn build(self) -> Result<Sandbox<G>> {
        let network = std::sync::Arc::new(std::sync::Mutex::new(NetworkPermissions::new()));
        let mut vfs = CapFs::new().with_limits(self.filesystem_limits);
        if let Some(input_dir) = &self.input_dir {
            vfs = vfs.with_input(input_dir)?;
        }
        vfs = match self.output_dir {
            Some((path, dir_perms, file_perms)) => {
                vfs.with_output_dir(path, dir_perms, file_perms)?
            }
            None if self.temp_output => vfs.with_temp_output()?,
            None => vfs,
        };
        if let Some((path, access)) = self.work_dir {
            vfs = vfs.with_work_dir(path, access)?;
        }
        if self.temp_dir {
            vfs = vfs.with_temp_dir()?;
        }
        let fs = std::sync::Arc::new(std::sync::Mutex::new(vfs));
        let inner = self
            .guest
            .build(self.config, self.tools, network.clone(), fs.clone())?;
        Ok(Sandbox { inner, network, fs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestGuest;

    struct TestSandbox;

    impl Guest for TestGuest {
        type Sandbox = TestSandbox;

        fn build(
            self,
            _config: SandboxConfig,
            _tools: ToolRegistry,
            _network: std::sync::Arc<std::sync::Mutex<NetworkPermissions>>,
            _fs: std::sync::Arc<std::sync::Mutex<CapFs>>,
        ) -> Result<Self::Sandbox> {
            Ok(TestSandbox)
        }
    }

    impl GuestSandbox for TestSandbox {
        type SnapshotData = ();

        fn run(&mut self, _code: &str) -> Result<ExecutionResult> {
            Ok(ExecutionResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }

        fn snapshot(&mut self) -> Result<Snapshot<Self::SnapshotData>> {
            Ok(Snapshot::new("test", std::sync::Arc::new(())))
        }

        fn restore(&mut self, _snapshot: &Snapshot<Self::SnapshotData>) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn builder_registers_mounts_in_documented_order() {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let sandbox = SandboxBuilder::new()
            .input_dir(input.path())
            .output_dir(
                output.path(),
                DirPerms::READ | DirPerms::MUTATE,
                FilePerms::READ | FilePerms::WRITE,
            )
            .work_dir(work.path(), WorkDirAccess::ReadOnly)
            .temp_dir()
            .guest(TestGuest)
            .build()
            .unwrap();
        let fs = sandbox.fs.lock().unwrap();

        assert_eq!(
            fs.preopens(),
            vec![(3, "/input"), (4, "/output"), (5, "/work"), (6, "/tmp")]
        );
        let work_flags = fs.get_flags(5).unwrap();
        assert!(work_flags.read);
        assert!(!work_flags.write);
        assert!(!work_flags.mutate_directory);
        let tmp_flags = fs.get_flags(6).unwrap();
        assert!(tmp_flags.read);
        assert!(tmp_flags.write);
        assert!(tmp_flags.mutate_directory);
    }

    #[test]
    fn builder_rejects_a_missing_work_directory() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        let result = SandboxBuilder::new()
            .work_dir(&missing, WorkDirAccess::ReadOnly)
            .guest(TestGuest)
            .build();

        let error = result.err().expect("missing work directory must fail");
        assert!(error.to_string().contains("failed to open work dir"));
    }

    #[test]
    fn restore_clears_tmp_and_preserves_work() {
        let work = tempfile::tempdir().unwrap();
        let mut sandbox = SandboxBuilder::new()
            .work_dir(work.path(), WorkDirAccess::ReadWrite)
            .temp_dir()
            .guest(TestGuest)
            .build()
            .unwrap();
        {
            let mut fs = sandbox.fs.lock().unwrap();
            let preopens = fs.preopens();
            let work_fd = preopens
                .iter()
                .find_map(|(fd, path)| (*path == "/work").then_some(*fd))
                .unwrap();
            let tmp_fd = preopens
                .iter()
                .find_map(|(fd, path)| (*path == "/tmp").then_some(*fd))
                .unwrap();
            drop(preopens);
            let work_file = fs
                .open_at(work_fd, "persist.txt", OpenFlags::CREATE)
                .unwrap();
            fs.write_file(work_file, 0, b"work").unwrap();
            let tmp_file = fs
                .open_at(tmp_fd, "discard.txt", OpenFlags::CREATE)
                .unwrap();
            fs.write_file(tmp_file, 0, b"tmp").unwrap();
        }
        let snapshot = sandbox.snapshot().unwrap();

        sandbox.restore(&snapshot).unwrap();

        let fs = sandbox.fs.lock().unwrap();
        assert_eq!(
            std::fs::read(work.path().join("persist.txt")).unwrap(),
            b"work"
        );
        assert_eq!(std::fs::read_dir(fs.temp_path().unwrap()).unwrap().count(), 0);
    }
}
