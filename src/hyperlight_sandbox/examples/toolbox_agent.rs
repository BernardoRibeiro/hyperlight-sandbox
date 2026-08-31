//! A simple rule-based agent built on the Phase 3 bounded toolbox.
//!
//! Unlike `toolbox_basics.rs`, which just replays a fixed transcript of
//! commands, this agent works from a plan of goals. For each goal it runs a
//! primary bash command through `Toolbox::execute_cli`, inspects the exit
//! code, and — if the primary command fails — falls back to a recovery
//! command before re-checking the goal. That mirrors the minimal loop real
//! coding agents run: act, observe, decide, and it never leaves the bounded
//! `execute_cli` surface (no shelling out to a host process).

use hyperlight_sandbox::toolbox::Toolbox;
use hyperlight_sandbox::{CapFs, ExecutionResult, WorkDirAccess};

/// One step in the agent's plan: a goal, the command that should satisfy it,
/// and an optional recovery command to run if the primary one fails.
struct Goal {
    description: &'static str,
    command: &'static str,
    recover: Option<&'static str>,
}

struct SimpleAgent {
    shell: Toolbox,
}

impl SimpleAgent {
    fn new() -> Self {
        Self { shell: Toolbox::default() }
    }

    /// Run one goal: act, observe, and self-heal once if the primary command
    /// failed and a recovery command was provided.
    fn pursue(&mut self, fs: &mut CapFs, goal: &Goal) -> ExecutionResult {
        println!("\nGoal: {}", goal.description);
        println!("  action:      {}", goal.command);
        let mut result = self.shell.execute_cli(fs, goal.command);
        report("  observation:", &result);

        if result.exit_code != 0 {
            if let Some(recovery) = goal.recover {
                println!("  decision:    primary action failed, recovering with: {recovery}");
                let recovery_result = self.shell.execute_cli(fs, recovery);
                report("  recovery:   ", &recovery_result);

                println!("  action:      {} (retry)", goal.command);
                result = self.shell.execute_cli(fs, goal.command);
                report("  observation:", &result);
            } else {
                println!("  decision:    no recovery configured, giving up on this goal");
            }
        } else {
            println!("  decision:    goal satisfied on the first try");
        }
        result
    }
}

fn report(label: &str, result: &ExecutionResult) {
    print!("{label} exit={}", result.exit_code);
    if !result.stdout.is_empty() {
        print!(" stdout={:?}", result.stdout.trim_end());
    }
    if !result.stderr.is_empty() {
        print!(" stderr={:?}", result.stderr.trim_end());
    }
    println!();
}

fn main() {
    let work_dir = tempfile::tempdir().expect("create temp work dir");
    let mut fs = CapFs::new()
        .with_work(work_dir.path(), WorkDirAccess::ReadWrite)
        .expect("mount /work read-write");
    let mut agent = SimpleAgent::new();

    let plan = [
        Goal {
            description: "Create the project workspace",
            command: "mkdir project",
            recover: None,
        },
        Goal {
            description: "Move into the workspace and confirm the location",
            command: "cd project && pwd",
            recover: None,
        },
        Goal {
            description: "Make sure README.md exists",
            // Fails on the first pass (no README yet); the agent notices the
            // non-zero exit code and heals by touching the file, then
            // re-verifies with the same `cat` check.
            command: "cat README.md",
            recover: Some("touch README.md"),
        },
        Goal {
            description: "Record that provisioning succeeded",
            command: "echo 'workspace provisioned' && true",
            recover: None,
        },
        Goal {
            description: "Tear down the workspace file",
            command: "rm README.md",
            recover: None,
        },
        Goal {
            description: "Confirm teardown removed README.md",
            // This one is *expected* to fail (exit_code != 0) since the file
            // is gone — the agent has no recovery for it, so it reports and
            // moves on rather than treating every failure as a bug.
            command: "cat README.md",
            recover: None,
        },
    ];

    let mut satisfied = 0;
    for goal in &plan {
        if agent.pursue(&mut fs, goal).exit_code == 0 {
            satisfied += 1;
        }
    }

    println!(
        "\n{satisfied}/{} goals ended in exit code 0 (the final teardown check is expected to fail).",
        plan.len()
    );
}
