//! Phase 2.5 integration and cancellation acceptance tests.
//!
//! These tests require the Python AOT component built by
//! `just -f src/wasm_sandbox/Justfile guest-build`. They are ignored during the
//! ordinary unit-test suite because Hyperlight requires virtualization support.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use hyperlight_sandbox::{SandboxBuilder, WorkDirAccess};
use hyperlight_wasm_sandbox::Wasm;

const DEADLINE: Duration = Duration::from_secs(2);
const GRACE: Duration = Duration::from_secs(1);

fn guest_path() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("guests/python/python-sandbox.aot")
        .display()
        .to_string()
}

fn wait_until(
    child: &mut Child,
    limit: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if started.elapsed() >= limit {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_worker(mode: &str, work: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--ignored", "--exact", "phase_2_5_worker"])
        .env("PHASE_2_5_WORKER", mode)
        .env("PHASE_2_5_WORK", work)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn isolated sandbox worker")
}

fn run_sandbox(work: PathBuf, code: &str) {
    let mut sandbox = SandboxBuilder::new()
        .guest(Wasm)
        .module_path(guest_path())
        .work_dir(work, WorkDirAccess::ReadWrite)
        .tmp_dir()
        .build()
        .expect("build AOT sandbox");
    let result = sandbox.run(code).expect("run guest");
    assert_eq!(result.exit_code, 0, "guest stderr: {}", result.stderr);
}

#[test]
#[ignore = "requires the AOT guest and Hyperlight virtualization"]
fn phase_2_5_worker() {
    let Ok(mode) = std::env::var("PHASE_2_5_WORKER") else {
        return;
    };
    let work = PathBuf::from(std::env::var_os("PHASE_2_5_WORK").expect("worker path"));
    match mode.as_str() {
        "hang" => run_sandbox(work, "while True: pass"),
        "healthy" => run_sandbox(work, "print(open('/work/nested/input.txt').read())"),
        other => panic!("unknown worker mode: {other}"),
    }
}

#[test]
#[ignore = "requires the AOT guest and Hyperlight virtualization"]
fn aot_filesystem_persistence_and_tmp_cleanup() {
    let work = tempfile::tempdir().expect("work directory");
    std::fs::create_dir(work.path().join("nested")).unwrap();
    std::fs::write(work.path().join("nested/input.txt"), "phase-2.5").unwrap();

    let mut sandbox = SandboxBuilder::new()
        .guest(Wasm)
        .module_path(guest_path())
        .work_dir(work.path(), WorkDirAccess::ReadWrite)
        .tmp_dir()
        .build()
        .expect("build AOT sandbox");

    let first = sandbox
        .run(
            r#"
import os
assert open('/work/nested/input.txt').read() == 'phase-2.5'
open('/work/nested/created.txt', 'w').write('one')
open('/work/nested/created.txt', 'a').write('-two')
os.rename('/work/nested/created.txt', '/work/nested/persisted.txt')
open('/work/nested/delete-me.txt', 'w').write('delete')
os.remove('/work/nested/delete-me.txt')
open('/tmp/ephemeral.txt', 'w').write('temporary')
"#,
        )
        .expect("first invocation");
    assert_eq!(first.exit_code, 0, "guest stderr: {}", first.stderr);

    let second = sandbox
        .run(
            r#"
import os
assert open('/work/nested/persisted.txt').read() == 'one-two'
assert not os.path.exists('/work/nested/delete-me.txt')
assert not os.path.exists('/tmp/ephemeral.txt')
"#,
        )
        .expect("second invocation");
    assert_eq!(second.exit_code, 0, "guest stderr: {}", second.stderr);
    assert_eq!(
        std::fs::read_to_string(work.path().join("nested/persisted.txt")).unwrap(),
        "one-two"
    );
}

#[test]
#[ignore = "requires the AOT guest and Hyperlight virtualization"]
fn hard_timeout_discards_worker_and_recreates_sandbox() {
    let work = tempfile::tempdir().expect("work directory");
    std::fs::create_dir(work.path().join("nested")).unwrap();
    std::fs::write(work.path().join("nested/input.txt"), "healthy").unwrap();

    for _ in 0..3 {
        let started = Instant::now();
        let mut poisoned = spawn_worker("hang", work.path());
        assert!(wait_until(&mut poisoned, DEADLINE).unwrap().is_none());
        poisoned.kill().expect("hard-kill timed-out worker");
        let status = wait_until(&mut poisoned, GRACE)
            .expect("wait for killed worker")
            .expect("worker must terminate within bounded grace");
        assert!(!status.success());
        assert!(started.elapsed() <= DEADLINE + GRACE);

        // Never reuse the timed-out process: a new process owns a fresh sandbox.
        let mut replacement = spawn_worker("healthy", work.path());
        let status = wait_until(&mut replacement, DEADLINE + GRACE)
            .expect("wait for replacement")
            .expect("replacement sandbox did not finish");
        assert!(status.success(), "replacement sandbox failed: {status}");
    }
}
