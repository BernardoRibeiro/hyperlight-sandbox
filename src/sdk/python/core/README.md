# hyperlight-sandbox

Python API package for running code inside Hyperlight sandboxes with separately
installable backends and packaged guest packages.

## Quick Start

```python
from hyperlight_sandbox import Sandbox

sandbox = Sandbox(backend="wasm", module="python_guest.path")
sandbox.register_tool("add", lambda a=0, b=0: a + b)

result = sandbox.run('''
result = call_tool('add', a=3, b=4)
print(result)
''')
print(result.stdout)  # "7\n"
```

Install the local repo packages for development with:

```bash
uv sync          # installs core + guest packages via workspace
just python-build  # builds maturin backends
```

Packaged guest packages expose importable module references such as
`python_guest.path` and `javascript_guest.path`. The API resolves those to the
packaged `.aot` artifact automatically.

Example dependency sets:

- `Sandbox(backend="wasm", module="python_guest.path")` requires `hyperlight-sandbox[wasm,python_guest]`
- `Sandbox(backend="wasm", module="javascript_guest.path")` requires `hyperlight-sandbox[wasm,javascript_guest]`
- `Sandbox(backend="hyperlight-js")` requires `hyperlight-sandbox[hyperlight_js]`

Use `Sandbox(backend="wasm", module="javascript_guest.path")` to run the
packaged JavaScript Wasm guest package.

Use `Sandbox(backend="hyperlight-js")` to run the separate HyperlightJS
backend package.

## Filesystem Mounts

```python
sandbox = Sandbox(
    backend="wasm",
    module="python_guest.path",
    input_dir="./fixtures",
    temp_output=True,
    work_dir="./disposable-workspace",
    work_dir_access="rw",
    temp_dir=True,
)
```

| Guest path | Default access | Lifetime |
|---|---|---|
| `/input` | Read-only | Preserved |
| `/output` | Read/write | Cleared recursively before each run and after restore |
| `/work` | Read-only; `"rw"` must be explicit | Preserved |
| `/tmp` | Read/write and private to the sandbox | Cleared recursively before each run and after restore |

Mount a disposable copy or Git worktree when using
`work_dir_access="rw"`. Capability isolation prevents access outside `/work`,
but it cannot prevent guest code from deleting or corrupting files inside the
workspace it was explicitly given. Host directories mounted with different
lifetimes must not alias or contain one another; construction fails before an
ephemeral cleanup could reach a persistent mount.

## Snapshot Semantics

- `snapshot()` captures guest runtime state, not persistent host filesystem state.
- `restore()` rewinds runtime state, clears `/output` and `/tmp`, and preserves `/work`.
- Every `run()` recursively clears `/output` and `/tmp` before guest execution.
- Filesystem changes under `/work` persist across runs and snapshot restore.

## Build

```bash
just python-build
```
