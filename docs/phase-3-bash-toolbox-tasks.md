# Phase 3: Bash-compatible toolbox task list

Phase 3 starts only after the Phase 2.5 cancellation gate passes. Its goal is a
bounded, Wasm-native shell interface; it must never fall back to a host shell or
host executable.

## 1. Foundations and contracts

- [x] Create the toolbox component under `src/wasm_sandbox/guests/toolbox`.
- [x] Define execution state with stdin, stdout, stderr, cwd, environment, and
      shared invocation budgets; the Phase 2.5 process supervisor owns the hard
      wall-clock deadline and discards timed-out sandboxes.
- [x] Define Python `ToolResult` with `exit_code`, `stdout`, `stderr`, `timed_out`, and
      `truncated`.
- [x] Add limits for script bytes, AST nodes/depth, pipeline stages, expansion
      results, output bytes, and filesystem traversal depth.
- [x] Implement a guest-side command registry. Unknown commands exit 127;
      registry misses must never invoke a host process.
- [x] Establish ordinary command-error handling: invalid input and command
      failures return an exit status and diagnostic instead of panicking.

## 2. Lexer, parser, and bounded AST

- [x] Tokenize unquoted, single-quoted, and double-quoted words, escapes,
      operators, and empty arguments.
- [x] Parse simple commands and the `;`, `&&`, and `||` operators with explicit
      precedence tests.
- [x] Parse pipelines and `<`, `>`, and `>>` redirections.
- [x] Reject malformed input with stable diagnostics.
- [x] Enforce AST node/depth and pipeline-stage limits while parsing, before
      execution or expansion begins.
- [x] Cover quoting, composition, pipelines, redirects, and filesystem policy
      in the AOT acceptance suite and SDK tests.

## 3. Shell state and expansion

- [x] Implement cwd initialized to `/work`.
- [x] Keep a small deterministic environment; do not inherit ambient host
      variables or credentials.
- [x] Implement assignment, `export`, `unset`, `$VAR`, `${VAR}`, and `$?`.
- [x] Implement bounded glob expansion relative to cwd without following paths
      outside the mounted capabilities.
- [x] Add `cd` path normalization and ensure all commands resolve paths through
      the same capability-relative helper.
- [x] Keep command substitution and shell loops unsupported; hard cancellation
      remains the responsibility of the proven Phase 2.5 process supervisor.

## 4. Execution and composition

- [x] Execute `;`, `&&`, and `||` with Bash-compatible status propagation.
- [x] Implement sequential pipelines with output-bounded in-memory buffers;
      commands use the private `/tmp` mount for explicit intermediate files.
- [x] Apply input/output redirections through WASI/CapFs, including append semantics
      and read-only mount failures.
- [x] Share parsing, expansion, traversal, pipeline, and output budgets across
      every pipeline stage and registered command.
- [x] Add deterministic stdout/stderr truncation markers and preserve the
      command exit status when truncation occurs.

## 5. Wasm-native commands (implementation order)

- [x] State/trivial: `echo`, `printf`, `true`, `false`, `pwd`, and `cd`.
- [x] Inspection: `ls`, `cat`, `head`, `tail`, and `wc`.
- [x] Search/tests: `grep`, `find`, `test`, and `[`.
- [x] Mutation: `mkdir`, `touch`, `cp`, `mv`, and `rm`.
- [x] Editing: a deterministic `sed` substitution subset plus `cp`-based
      whole-file replacement command.
- [x] Exercise successful behavior, pipeline composition, ordinary I/O errors,
      and attempts to mutate read-only mounts.

## 6. Agent-facing API and integration

- [x] Expose asynchronous `execute_cli(script: str) -> ToolResult` through the
      Python SDK.
- [x] Add reproducible Wasm component and AOT build recipes.
- [x] Add an AOT acceptance script containing quotes, pipelines,
      redirections, variables, globs, and conditionals through the complete AOT
      path.
- [x] Demonstrate nested repository inspection and editing using only
      `execute_cli`.
- [x] Bound output floods, recursive traversal, script size, and AST size; the
      supervisor bounds non-terminating guest execution.
- [x] Keep the component registry free of host process dispatch; no unknown
      command reaches `bash`, `sh`, `subprocess`, `fork`, `exec`, or another
      host binary.

## 7. Phase 3 completion evidence

- [x] Record component build and integration-test commands in the Justfile.
- [x] Document supported syntax and intentional differences from Bash/GNU
      utilities.
- [x] Add JSON machine-readable telemetry for unsupported commands to
      guide Phase 4 workload tracing.
- [x] Provide an AOT Phase 3 integration suite intended to run alongside the
      Phase 2.5 timeout, discard, and recreation gate.
