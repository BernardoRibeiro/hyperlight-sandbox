//! Basics demo for the Phase 3 bounded Bash-compatible toolbox.
//!
//! `Toolbox` is a host-native, deterministic command executor: it never
//! shells out, it only dispatches the small set of built-ins registered in
//! `toolbox.rs` and reaches the filesystem exclusively through `CapFs`
//! capabilities. This example mounts a temporary host directory at `/work`
//! and runs a series of scripts through `execute_cli` to exercise `pwd`,
//! `cd`, `mkdir`, `touch`, `cat`, `rm`, `echo`, quoting, `;`/`&&`/`||`
//! composition, and `$?` expansion.

use hyperlight_sandbox::toolbox::Toolbox;
use hyperlight_sandbox::{CapFs, WorkDirAccess};

fn separator(title: &str) {
    println!("\n{}", "─".repeat(60));
    println!("{title}");
    println!("{}", "─".repeat(60));
}

fn run(shell: &mut Toolbox, fs: &mut CapFs, script: &str) {
    let result = shell.execute_cli(fs, script);
    println!("$ {script}");
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
    println!("(exit {})", result.exit_code);
}

fn main() {
    let work_dir = tempfile::tempdir().expect("create temp work dir");
    let mut fs = CapFs::new()
        .with_work(work_dir.path(), WorkDirAccess::ReadWrite)
        .expect("mount /work read-write");
    let mut shell = Toolbox::default();

    separator("Test 1: pwd and basic composition");
    run(&mut shell, &mut fs, "pwd");
    run(&mut shell, &mut fs, "true && echo 'first command ok'");
    run(&mut shell, &mut fs, "false || echo 'fallback ran'");

    separator("Test 2: directories and cd");
    run(&mut shell, &mut fs, "mkdir project");
    // Known limitation: `cd` only recognizes preopened mount roots (via
    // CapFs::dir_by_guest_path), not directories created afterwards with
    // `mkdir`. So this currently fails even though `project` now exists.
    run(&mut shell, &mut fs, "cd project && pwd");
    run(&mut shell, &mut fs, "cd missing");

    separator("Test 3: files with touch, cat, and quoting (paths relative to /work)");
    run(&mut shell, &mut fs, "touch project/notes.txt");
    run(&mut shell, &mut fs, "cat project/notes.txt && echo 'notes.txt exists and is empty'");

    separator("Test 4: exit-status propagation via $?");
    // Known limitation: Toolbox only writes `last_status` once, after the
    // whole script finishes, so `$?` inside a single composed script still
    // sees the status left over from the *previous* execute_cli call rather
    // than an earlier command in this one.
    run(&mut shell, &mut fs, "cat project/missing.txt; echo \"same-script status: $?\"");
    // Across two separate execute_cli calls it does carry forward correctly.
    run(&mut shell, &mut fs, "cat project/missing.txt");
    run(&mut shell, &mut fs, "echo \"cross-call status: $?\"");

    separator("Test 5: cleanup with rm");
    run(&mut shell, &mut fs, "rm project/notes.txt");
    run(&mut shell, &mut fs, "cat project/notes.txt");

    println!("\nAll toolbox commands executed without shelling out to a host process.");
}
