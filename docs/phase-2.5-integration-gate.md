# Phase 2.5 integration and cancellation gate

Phase 2.5 verifies the Phase 1 filesystem and Phase 2 mount-lifetime work
through a real AOT-compiled component before the Bash-compatible toolbox is
implemented.

## What the gate proves

The `mount_lifetimes` integration test reuses the existing small AOT Python
component to exercise the complete path without overlapping the Phase 3
toolbox implementation:

```text
guest component -> WASI filesystem imports -> HostState -> CapFs -> host mount
```

It verifies that:

- nested files under a read-write `/work` mount can be read, created,
  modified, renamed, and deleted;
- `/work` changes persist across `run()` and runtime snapshot restore;
- `/tmp` is recursively cleared before the next run and on restore;
- a running guest can be interrupted through Hyperlight's interrupt handle;
- interruption poisons the old sandbox;
- three consecutive interrupt, discard, recreate, and recovery cycles
  complete successfully.

## Cancellation policy for the toolbox

The pinned Hyperlight-Wasm revision exposes
`LoadedWasmSandbox::interrupt_handle()`. Hyperlight Sandbox surfaces it as an
optional backend-neutral `ExecutionInterruptHandle`.

A toolbox supervisor should:

1. obtain the handle before starting an invocation;
2. start a watchdog using the invocation deadline;
3. call `kill()` only if the invocation is still active at the deadline;
4. treat the interrupted sandbox as poisoned;
5. discard it instead of returning it to a warm pool;
6. create or acquire a clean sandbox for the next invocation.

The handle is intentionally separate from a timeout policy. Phase 3 owns the
deadline configuration, watchdog lifecycle, timeout result, and recreation
policy used by `execute_cli`.

## Running the gate

Build the existing Python component and run the Wasm integration tests:

```bash
just wasm build
just wasm test
```

The tests require a supported Hyperlight virtualization backend such as KVM on
Linux or WHP on Windows.
