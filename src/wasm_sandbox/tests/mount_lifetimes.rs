//! End-to-end gates for persistent and ephemeral mounts and hard cancellation.
//!
//! These tests execute the AOT-compiled Python component so they cover the
//! complete Hyperlight-Wasm, WIT, WASI filesystem, and CapFs path.

use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use hyperlight_sandbox::{SandboxBuilder, WorkDirAccess};
use hyperlight_wasm_sandbox::Wasm;

fn python_guest_path() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("guests/python/python-sandbox.aot")
        .display()
        .to_string()
}

#[test]
fn work_persists_while_tmp_is_cleared_across_runs_and_restore() {
    let work = tempfile::tempdir().expect("failed to create work directory");
    let source_dir = work.path().join("src");
    std::fs::create_dir(&source_dir).expect("failed to create nested source directory");
    std::fs::write(source_dir.join("module.py"), "VALUE = 41\n")
        .expect("failed to seed work directory");

    let mut sandbox = SandboxBuilder::new()
        .guest(Wasm)
        .module_path(python_guest_path())
        .work_dir(work.path(), WorkDirAccess::ReadWrite)
        .tmp_dir()
        .build()
        .expect("failed to create sandbox");
    let clean_runtime = sandbox.snapshot().expect("failed to snapshot sandbox");

    let first = sandbox
        .run(
            r#"
import os
with open('/work/src/module.py') as source:
    original = source.read()
with open('/work/src/generated.txt', 'w') as generated:
    generated.write(original)
with open('/work/src/generated.txt', 'a') as generated:
    generated.write('generated\n')
os.rename('/work/src/generated.txt', '/work/src/renamed.txt')
with open('/work/src/delete-me.txt', 'w') as temporary:
    temporary.write('delete me')
os.remove('/work/src/delete-me.txt')
with open('/tmp/transient.txt', 'w') as transient:
    transient.write('discard me')
print(original.strip())
"#,
        )
        .expect("first guest invocation failed");

    assert_eq!(first.exit_code, 0, "stderr: {}", first.stderr);
    assert_eq!(first.stdout.trim(), "VALUE = 41");
    assert!(!source_dir.join("generated.txt").exists());
    assert!(!source_dir.join("delete-me.txt").exists());
    assert_eq!(
        std::fs::read_to_string(source_dir.join("renamed.txt")).unwrap(),
        "VALUE = 41\ngenerated\n"
    );

    let second = sandbox
        .run(
            r#"
import os
print(f"work={os.path.exists('/work/src/renamed.txt')}")
print(f"tmp={os.path.exists('/tmp/transient.txt')}")
with open('/tmp/before-restore.txt', 'w') as temporary:
    temporary.write('discard on restore')
"#,
        )
        .expect("second guest invocation failed");

    assert_eq!(second.exit_code, 0, "stderr: {}", second.stderr);
    assert!(second.stdout.contains("work=True"), "{}", second.stdout);
    assert!(second.stdout.contains("tmp=False"), "{}", second.stdout);

    sandbox
        .restore(&clean_runtime)
        .expect("failed to restore sandbox");
    let after_restore = sandbox
        .run(
            r#"
import os
print(f"work={os.path.exists('/work/src/renamed.txt')}")
print(f"tmp={os.path.exists('/tmp/before-restore.txt')}")
"#,
        )
        .expect("post-restore guest invocation failed");

    assert_eq!(
        after_restore.exit_code, 0,
        "stderr: {}",
        after_restore.stderr
    );
    assert!(
        after_restore.stdout.contains("work=True"),
        "{}",
        after_restore.stdout
    );
    assert!(
        after_restore.stdout.contains("tmp=False"),
        "{}",
        after_restore.stdout
    );
}

fn interrupt_one_sandbox() -> (Duration, Duration) {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let mut sandbox = SandboxBuilder::new()
        .guest(Wasm)
        .module_path(python_guest_path())
        .tool_typed::<serde_json::Value, _>("entered_guest", move |_| {
            entered_tx
                .send(())
                .map_err(|error| anyhow::anyhow!("failed to signal watchdog: {error}"))?;
            Ok(serde_json::Value::Null)
        })
        .build()
        .expect("failed to create sandbox");

    let interrupt = sandbox
        .interrupt_handle()
        .expect("failed to obtain interrupt handle")
        .expect("Wasm backend must support hard interruption");

    let watchdog = thread::spawn(move || {
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("guest did not reach the cancellation barrier");
        let cancellation_started = Instant::now();
        assert!(interrupt.kill(), "guest should be active when interrupted");
        cancellation_started.elapsed()
    });

    let run_started = Instant::now();
    let interrupted = sandbox.run(
        r#"
call_tool('entered_guest', {})
while True:
    pass
"#,
    );
    let total_elapsed = run_started.elapsed();
    let cancellation_elapsed = watchdog.join().expect("watchdog thread panicked");

    let error = interrupted.expect_err("infinite guest invocation should be interrupted");
    assert!(
        error.to_string().contains("guest execution failed"),
        "unexpected cancellation error: {error:#}"
    );
    let poisoned = sandbox
        .run("print('must not run')")
        .expect_err("interrupted sandbox should remain poisoned");
    assert!(
        poisoned.to_string().contains("guest execution failed"),
        "unexpected poisoned-sandbox error: {poisoned:#}"
    );
    drop(sandbox);

    (cancellation_elapsed, total_elapsed)
}

#[test]
fn repeated_hard_interrupts_poison_old_sandboxes_and_fresh_sandboxes_run() {
    for attempt in 1..=3 {
        let (cancellation_elapsed, total_elapsed) = interrupt_one_sandbox();
        assert!(
            cancellation_elapsed < Duration::from_secs(5),
            "attempt {attempt}: hard cancellation took {cancellation_elapsed:?}"
        );
        assert!(
            total_elapsed < Duration::from_secs(10),
            "attempt {attempt}: interrupted invocation took {total_elapsed:?}"
        );

        let mut fresh = SandboxBuilder::new()
            .guest(Wasm)
            .module_path(python_guest_path())
            .build()
            .expect("failed to recreate sandbox");
        let recovered = fresh
            .run("print('fresh sandbox')")
            .expect("fresh sandbox should run after discarding poisoned instance");
        assert_eq!(recovered.exit_code, 0, "stderr: {}", recovered.stderr);
        assert_eq!(recovered.stdout.trim(), "fresh sandbox");
    }
}
