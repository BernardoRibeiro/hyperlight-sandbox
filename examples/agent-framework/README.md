# Agent Framework + Hyperlight Wasm Sandbox

Run a coding agent with Microsoft Agent Framework against a local testbed using
**code mode**: the model calls `execute_code`, and guest Python reaches the host
testbed through `call_tool(...)` (file + git ops). Sandbox Python state persists
across turns (no restore between `execute_code` calls).

Default chat provider is **OpenAI** (`gpt-5-mini`). GitHub Copilot is still available
via `--provider copilot`.

## Quick Start

```bash
# From repo root (preferably inside the Vagrant VM)
uv sync --group agent-framework \
  --inexact \
  --no-install-package hyperlight-sandbox-backend-wasm \
  --no-install-package hyperlight-sandbox-backend-hyperlight-js

# Build guest + install local Hyperlight Python package
just wasm::guest-build
just python::python-build

# OpenAI (requires OPENAI_API_KEY)
export OPENAI_API_KEY=sk-...

# Code-mode smoke test (execute_code + call_tool)
uv run python examples/agent-framework/copilot_agent.py \
  --provider openai \
  --model gpt-5-mini \
  --testbed "$HOME/testbeds/CLI_Tools_Easy" \
  --smoke-test

# Host tools only (no Wasm backend required)
uv run python examples/agent-framework/copilot_agent.py \
  --provider openai \
  --model gpt-5-mini \
  --testbed "$HOME/testbeds/CLI_Tools_Easy" \
  --no-sandbox

# Full code-mode solve of issue.md
uv run python examples/agent-framework/copilot_agent.py \
  --provider openai \
  --model gpt-5-mini \
  --testbed "$HOME/testbeds/CLI_Tools_Easy"

# Optional interactive REPL
uv run python examples/agent-framework/copilot_agent.py \
  --provider openai \
  --model gpt-5-mini \
  --testbed "$HOME/testbeds/CLI_Tools_Easy" \
  --interactive

# GitHub Copilot instead of OpenAI
gh auth login
uv run python examples/agent-framework/copilot_agent.py \
  --provider copilot \
  --testbed "$HOME/testbeds/CLI_Tools_Easy"

# DevUI web interface
uv sync --group agent-framework-devui
just agent-framework-example-devui
```

## Tools

### Code mode (default)

The agent sees only `execute_code`. Inside the guest, use `call_tool`:

| Host tool via `call_tool` | Purpose |
|---------------------------|---------|
| `read_file(path)` | Read a UTF-8 file under `--testbed` |
| `write_file(path, content)` | Create/overwrite a file under `--testbed` |
| `list_files(path, recursive)` | List files (skips `.venv` / `.git` / caches) |
| `git_status` / `git_diff` / `git_log` / `git_show` | Inspect repo state |
| `git_add(paths)` / `git_commit(message)` | Stage and commit |

There is **no** `run_bash` tool. Paths are relative to the testbed root and cannot escape it.

### `--no-sandbox`

Exposes the same file/git tools directly to the agent (no Hyperlight).

Tool calls are logged under `examples/agent-framework/logs/<timestamp>/` (or `--log-dir`):
`tools.jsonl`, `tools.log`, and `commands/NNNN_<tool>.txt`.

## How It Works

```
Agent (OpenAI gpt-5-mini or Copilot)
  └── execute_code(code="...")
          │
          ▼
     Sandbox(backend="wasm").run(code)   # state kept across turns
          │
          ▼
     Guest: call_tool("read_file" | "write_file" | "list_files" | "git_*")
          │
          ▼
     Host callbacks operate on --testbed
```
