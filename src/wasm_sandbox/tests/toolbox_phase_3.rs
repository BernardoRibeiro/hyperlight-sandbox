//! End-to-end acceptance tests for the Wasm-native Phase 3 toolbox.

use std::path::Path;

use hyperlight_sandbox::{SandboxBuilder, WorkDirAccess};
use hyperlight_wasm_sandbox::Wasm;

fn toolbox_guest_path() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("guests/toolbox/toolbox.aot")
        .display()
        .to_string()
}

#[test]
#[ignore = "requires the AOT toolbox component and Hyperlight virtualization"]
fn toolbox_runs_repository_workflow_inside_the_guest() {
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join("src")).unwrap();
    std::fs::write(work.path().join("src/lib.rs"), "old name\nkeep\n").unwrap();

    let mut sandbox = SandboxBuilder::new()
        .guest(Wasm)
        .module_path(toolbox_guest_path())
        .work_dir(work.path(), WorkDirAccess::ReadWrite)
        .tmp_dir()
        .build()
        .unwrap();

    let result = sandbox
        .run(
            "cd /work && find src -name '*.rs' | sort > /tmp/files; \
             test -s /tmp/files && cat src/lib.rs | grep old; \
             sed 's/old/new/g' src/lib.rs > /tmp/lib.rs && cp /tmp/lib.rs src/lib.rs",
        )
        .unwrap();
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert!(result.stdout.contains("old name"));
    assert_eq!(
        std::fs::read_to_string(work.path().join("src/lib.rs")).unwrap(),
        "new name\nkeep\n"
    );
}

#[test]
#[ignore = "requires the AOT toolbox component and Hyperlight virtualization"]
fn toolbox_enforces_read_only_work_mount() {
    let work = tempfile::tempdir().unwrap();
    let mut sandbox = SandboxBuilder::new()
        .guest(Wasm)
        .module_path(toolbox_guest_path())
        .work_dir(work.path(), WorkDirAccess::ReadOnly)
        .tmp_dir()
        .build()
        .unwrap();
    let result = sandbox.run("touch /work/denied").unwrap();
    assert_ne!(result.exit_code, 0);
    assert!(!work.path().join("denied").exists());
}

#[test]
#[ignore = "requires the AOT toolbox component and Hyperlight virtualization"]
fn toolbox_has_no_host_command_fallback() {
    let work = tempfile::tempdir().unwrap();
    let mut sandbox = SandboxBuilder::new()
        .guest(Wasm)
        .module_path(toolbox_guest_path())
        .work_dir(work.path(), WorkDirAccess::ReadWrite)
        .build()
        .unwrap();
    let result = sandbox.run("sh -c 'touch /work/escaped'").unwrap();
    assert_eq!(result.exit_code, 127);
    assert!(result.stderr.contains("command not found"));
    assert!(!work.path().join("escaped").exists());
}
