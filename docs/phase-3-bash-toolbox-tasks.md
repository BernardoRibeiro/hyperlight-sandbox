# Phase 3: Bash-compatible toolbox task list

Phase 3 starts only after the Phase 2.5 cancellation gate passes. Its goal is a
bounded, Wasm-native shell interface; it must never fall back to a host shell or
host executable.

## 1. Foundations and contracts

- [ ] Create the toolbox component under `src/wasm_sandbox/guests/toolbox`.
- [ ] Define `ExecutionContext` with stdin, stdout, stderr, cwd, environment,
      deadline, and a shared invocation budget.
- [ ] Define `ToolResult` with `exit_code`, `stdout`, `stderr`, `timed_out`, and
      `truncated`.
- [ ] Add configurable limits for script bytes, AST nodes/depth, pipeline
      stages, expansion results, output bytes, and wall time.
- [ ] Implement a guest-side command registry. Unknown commands must exit 127;
      registry misses must never invoke a host process.
- [ ] Establish ordinary command-error handling: invalid input and command
      failures return an exit status and diagnostic instead of panicking.

## 2. Lexer, parser, and bounded AST

- [ ] Tokenize unquoted, single-quoted, and double-quoted words, escapes,
      operators, and empty arguments.
- [ ] Parse simple commands and the `;`, `&&`, and `||` operators with explicit
      precedence tests.
- [ ] Parse pipelines and `<`, `>`, and `>>` redirections.
- [ ] Reject malformed input with source locations and stable diagnostics.
- [ ] Enforce AST node/depth and pipeline-stage limits while parsing, before
      execution or expansion begins.
- [ ] Add table-driven lexer/parser tests for quoting, escaping, adjacency,
      malformed operators, and limit boundaries.

## 3. Shell state and expansion

- [ ] Implement cwd initialized to `/work` when mounted, otherwise `/`.
- [ ] Keep a small deterministic environment; do not inherit ambient host
      variables or credentials.
- [ ] Implement assignment, `export`, `unset`, `$VAR`, `${VAR}`, and `$?`.
- [ ] Implement bounded glob expansion relative to cwd without following paths
      outside the mounted capabilities.
- [ ] Add `cd` path normalization and ensure all commands resolve paths through
      the same capability-relative helper.
- [ ] Defer command substitution and all shell loops until cancellation remains
      proven for the integrated toolbox component.

## 4. Execution and composition

- [ ] Execute `;`, `&&`, and `||` with Bash-compatible status propagation.
- [ ] Implement sequential pipelines with bounded in-memory buffers and
      optional spill files only under `/tmp`.
- [ ] Apply input/output redirections through CapFs, including append semantics
      and read-only mount failures.
- [ ] Share one deadline and one budget across parsing, expansion, every
      pipeline stage, and every registered command.
- [ ] Add deterministic stdout/stderr truncation markers and preserve the
      command exit status when truncation occurs.

## 5. Wasm-native commands (implementation order)

- [ ] State/trivial: `echo`, `printf`, `true`, `false`, `pwd`, and `cd`.
- [ ] Inspection: `ls`, `cat`, `head`, `tail`, and `wc`.
- [ ] Search/tests: `grep`, `find`, `test`, and `[`.
- [ ] Mutation: `mkdir`, `touch`, `cp`, `mv`, and `rm`.
- [ ] Editing: a deterministic `sed` substitution subset plus a direct
      whole-file replacement command.
- [ ] For every command, test successful behavior, invalid options, ordinary
      I/O errors, budget exhaustion, and attempts to mutate read-only mounts.

## 6. Agent-facing API and integration

- [ ] Expose `execute_cli(script: str) -> ToolResult` through the Rust and Python
      SDK paths with identical semantics.
- [ ] Package and AOT-compile the toolbox component reproducibly.
- [ ] Exercise representative scripts containing quotes, pipelines,
      redirections, variables, globs, and conditionals through the complete AOT
      path.
- [ ] Demonstrate nested repository inspection and editing using only
      `execute_cli`.
- [ ] Verify an output flood, deep traversal, oversized script, oversized AST,
      and long-running command are bounded.
- [ ] Audit the component and registry to confirm no syntax or unknown-command
      path reaches `bash`, `sh`, `subprocess`, `fork`, `exec`, or an arbitrary
      host binary.

## 7. Phase 3 completion evidence

- [ ] Record the exact component build and integration-test commands.
- [ ] Document supported syntax and intentional differences from Bash/GNU
      utilities.
- [ ] Add machine-readable telemetry for unsupported syntax and commands to
      guide Phase 4 workload tracing.
- [ ] Re-run the Phase 2.5 filesystem, timeout, discard, and recreation gate
      with the Phase 3 toolbox artifact.

