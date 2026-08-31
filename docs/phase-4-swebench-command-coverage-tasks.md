# Phase 4: SWE-bench command coverage checklist

## Pilot and telemetry
- [ ] Select at least five diverse SWE-bench tasks and record their repository revisions.
- [ ] Capture each shell invocation, normalized syntax, command name, exit code,
      elapsed time, output size, filesystem operation class, and whether it was
      required for task progress.
- [ ] Redact repository contents, credentials, and other sensitive arguments.
- [ ] Publish a coverage table marking every observed item `supported`,
      `partial`, `oracle-only`, or `unsupported`.

## High-value shell and command gaps
- [ ] Add bounded command substitution only after cancellation regression tests pass.
- [ ] Add traced compound commands (`if`, `for`) only when pilot data proves they
      block tasks; retain AST, expansion, and deadline bounds.
- [ ] Implement an `rg`-compatible recursive search subset with binary-file,
      path, depth, match-count, and output limits.
- [ ] Expand `sed` for practical address selection and substitution behavior.
- [ ] Add deterministic `diff`, `patch`, `sort`, `uniq`, `cut`, `tr`, `tee`,
      `basename`, and `dirname` implementations.

## Git and filesystem safety
- [ ] Add only traced read-only Git operations: `status`, `diff`, `log`, and
      `show`, backed by a Wasm-native library.
- [ ] Ignore hooks, credentials, pagers, editors, filters, external diff/merge
      programs, global/system configuration, submodules, and network protocols.
- [ ] Add adversarial tests for hostile filenames, symlinks, malformed patches,
      huge trees, and malicious repository Git configuration.

## Compatibility and acceptance
- [ ] Re-run the identical pilot after each command tranche and quantify the
      unsupported-command rate and task impact.
- [ ] Verify at least five tasks can be attempted without a missing shell
      primitive blocking the agent.
- [ ] Document intentional differences from Bash and GNU utilities.
- [ ] Keep unknown commands as exit 127 and verify no execution path invokes a
      host shell, process, or arbitrary executable.
