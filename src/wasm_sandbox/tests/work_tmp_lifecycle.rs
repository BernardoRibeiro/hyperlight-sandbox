//! Phase 2 component acceptance tests for `/work` and `/tmp` lifetimes.

use std::path::Path;

use hyperlight_sandbox::{SandboxBuilder, WorkDirAccess};
use hyperlight_wasm_sandbox::Wasm;

fn python_guest_path() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("guests/python/python-sandbox.aot")
        .display()
        .to_string()
}

#[test]
fn read_only_work_mount_is_visible_but_not_mutable() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("project.txt"), b"host project").unwrap();
    let mut sandbox = SandboxBuilder::new()
        .module_path(python_guest_path())
        .work_dir(work.path(), WorkDirAccess::ReadOnly)
        .guest(Wasm)
        .build()
        .expect("failed to create read-only work sandbox");

    let result = sandbox
        .run(
            r#"
with open('/work/project.txt') as source:
    print(source.read())
try:
    with open('/work/created.txt', 'w') as destination:
        destination.write('denied')
except OSError:
    print('write blocked')
"#,
        )
        .expect("read-only work run failed");

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("host project"));
    assert!(result.stdout.contains("write blocked"));
    assert!(!work.path().join("created.txt").exists());
}

#[test]
fn work_persists_while_tmp_is_cleared_across_runs_and_restore() {
    let work = tempfile::tempdir().unwrap();
    let mut sandbox = SandboxBuilder::new()
        .module_path(python_guest_path())
        .work_dir(work.path(), WorkDirAccess::ReadWrite)
        .temp_dir()
        .guest(Wasm)
        .build()
        .expect("failed to create read-write work sandbox");
    let initial = sandbox.snapshot().expect("snapshot failed");

    let first = sandbox
        .run(
            r#"
with open('/work/persist.txt', 'w') as persistent:
    persistent.write('work survives')
with open('/tmp/discard.txt', 'w') as temporary:
    temporary.write('tmp does not survive')
"#,
        )
        .expect("first run failed");
    assert_eq!(first.exit_code, 0, "stderr: {}", first.stderr);

    let second = sandbox
        .run(
            r#"
import os
with open('/work/persist.txt') as persistent:
    print(persistent.read())
print('tmp exists:', os.path.exists('/tmp/discard.txt'))
with open('/tmp/after-second-run.txt', 'w') as temporary:
    temporary.write('discard on restore')
"#,
        )
        .expect("second run failed");
    assert_eq!(second.exit_code, 0, "stderr: {}", second.stderr);
    assert!(second.stdout.contains("work survives"));
    assert!(second.stdout.contains("tmp exists: False"));

    sandbox.restore(&initial).expect("restore failed");
    let after_restore = sandbox
        .run(
            r#"
import os
with open('/work/persist.txt') as persistent:
    print(persistent.read())
print('tmp exists:', os.path.exists('/tmp/after-second-run.txt'))
"#,
        )
        .expect("post-restore run failed");

    assert_eq!(
        after_restore.exit_code, 0,
        "stderr: {}",
        after_restore.stderr
    );
    assert!(after_restore.stdout.contains("work survives"));
    assert!(after_restore.stdout.contains("tmp exists: False"));
    assert_eq!(
        std::fs::read(work.path().join("persist.txt")).unwrap(),
        b"work survives"
    );
}
