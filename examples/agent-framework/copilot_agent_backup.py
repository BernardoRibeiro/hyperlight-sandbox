#!/usr/bin/env python3
"""Agent Framework + Hyperlight Wasm sandbox example.

Supports OpenAI (default: gpt-5-mini) or GitHub Copilot as the chat backend.
Exposes tools for operating on a host testbed (read_file, write_file, run_bash)
plus execute_code for isolated Python in a Hyperlight Wasm sandbox.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shutil
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Annotated, Any

from agent_framework import (  # noqa: E402, F401
    Agent,
    BaseChatClient,
    ChatResponse,
    Message,
    tool,
)
from hyperlight_sandbox import Sandbox
from pydantic import Field

DEFAULT_OPENAI_MODEL = "gpt-5-mini"
DEFAULT_BASH_TIMEOUT_S = 600
MAX_FILE_CHARS = 100_000

SYSTEM_PROMPT = """You are a coding agent working on a local testbed directory.

Tools:
- read_file(path): read a file relative to the testbed root
- write_file(path, content): create/overwrite a file relative to the testbed root
- run_bash(command): run a shell command with cwd set to the testbed root
- execute_code(code): run Python in an isolated Hyperlight Wasm sandbox

Use read_file / write_file / run_bash for repository exploration, edits, and tests.
Use execute_code for small isolated Python experiments that should not touch the testbed.
Prefer small, targeted edits. After changing code, run the project's tests with run_bash.

A project virtualenv is prepared at `.venv` in the testbed. Prefer:
  pytest -q
  python -m pytest -q
Do NOT install packages into other environments. If deps are missing, use:
  uv pip install -e . -r requirements-dev.txt
Always report command output and the final git diff when you finish a fix."""

ISSUE_SOLVE_PROMPT = """Fix the issue described below. The repository is already checked out in the testbed.

IMPORTANT: Complete the FULL workflow:
1. Read and understand the issue thoroughly
2. Explore the codebase to find relevant files
3. Implement the fix with write_file (or carefully targeted edits)
4. Run the project's original test suite with run_bash, e.g. `pytest -q` or `pytest -q tests/features/...`
5. If ANY test fails, analyze the error and fix it
6. Repeat steps 4-5 until tests pass
7. Stop only when tests pass, then show the final git diff via run_bash

Do NOT write custom verification scripts to bypass the project tests.
Success means the project's real test suite passes.
A `.venv` with project + dev deps is already available on PATH for run_bash.

## Issue (from issue.md)

{issue}
"""

SMOKE_TEST_PROMPT = """Run a quick end-to-end tool smoke test on the testbed. Do exactly these steps:

1. Use write_file to create `smoke_hello.py` with this exact content:
   print("smoke-test-ok")
2. Use read_file to read `smoke_hello.py` back and confirm the content.
3. Use run_bash to execute: python smoke_hello.py
4. Report the command exit code and stdout. Success means stdout contains smoke-test-ok.

Do not solve any repository issue. Stop after the smoke test.
"""


# --- Testbed / log state -----------------------------------------------------

_testbed_root: Path | None = None
_log_dir: Path | None = None
_tool_call_seq = 0
_sandbox = None
_snapshot = None
_input_dir = None


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _default_module_path() -> Path:
    return _repo_root() / "src/wasm_sandbox/guests/python/python-sandbox.aot"


def _require_testbed() -> Path:
    if _testbed_root is None:
        raise RuntimeError("Testbed root is not set. Pass --testbed PATH.")
    return _testbed_root


def _resolve_testbed_path(path: str) -> Path:
    """Resolve path relative to the testbed and reject escapes outside it."""
    root = _require_testbed().resolve()
    candidate = Path(path)
    if not candidate.is_absolute():
        candidate = root / candidate
    resolved = candidate.resolve()
    try:
        resolved.relative_to(root)
    except ValueError as exc:
        raise PermissionError(f"Path escapes testbed root: {path}") from exc
    return resolved


def _init_log_dir(log_dir: Path | None = None) -> Path:
    """Create a per-run log directory for tool calls and command outputs."""
    global _log_dir, _tool_call_seq
    _tool_call_seq = 0
    if log_dir is not None:
        root = log_dir.expanduser().resolve()
    else:
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        root = Path(__file__).resolve().parent / "logs" / stamp
    root.mkdir(parents=True, exist_ok=True)
    (root / "commands").mkdir(exist_ok=True)
    _log_dir = root
    print(f"📝 Tool logs: {_log_dir}")
    return root


def _log_tool_call(tool_name: str, arguments: dict[str, Any], result: str) -> None:
    """Append one tool invocation to tools.jsonl / tools.log and commands/."""
    global _tool_call_seq
    if _log_dir is None:
        return

    _tool_call_seq += 1
    seq = _tool_call_seq
    ts = datetime.now(timezone.utc).isoformat()
    entry = {
        "seq": seq,
        "timestamp": ts,
        "tool": tool_name,
        "arguments": arguments,
        "result": result,
    }

    with (_log_dir / "tools.jsonl").open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")

    with (_log_dir / "tools.log").open("a", encoding="utf-8") as f:
        f.write(f"=== [{seq}] {ts} {tool_name} ===\n")
        f.write("arguments:\n")
        f.write(json.dumps(arguments, ensure_ascii=False, indent=2))
        f.write("\nresult:\n")
        f.write(result)
        f.write("\n\n")

    call_path = _log_dir / "commands" / f"{seq:04d}_{tool_name}.txt"
    with call_path.open("w", encoding="utf-8") as f:
        f.write(f"tool: {tool_name}\n")
        f.write(f"timestamp: {ts}\n")
        f.write("arguments:\n")
        f.write(json.dumps(arguments, ensure_ascii=False, indent=2))
        f.write("\n\nresult:\n")
        f.write(result)
        f.write("\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run the Agent Framework sandbox / testbed demo")
    parser.add_argument(
        "--provider",
        choices=("openai", "copilot"),
        default="openai",
        help="Chat provider (default: openai)",
    )
    parser.add_argument(
        "--model",
        default=os.environ.get("OPENAI_CHAT_MODEL", DEFAULT_OPENAI_MODEL),
        help=f"OpenAI model id (default: {DEFAULT_OPENAI_MODEL})",
    )
    parser.add_argument(
        "--testbed",
        type=Path,
        default=None,
        help="Root directory the file/bash tools may access (default: temp demo dir)",
    )
    parser.add_argument(
        "--issue",
        type=Path,
        default=None,
        help="Path to issue.md (default: <testbed>/issue.md)",
    )
    parser.add_argument(
        "--log-dir",
        type=Path,
        default=None,
        help="Directory for tool call logs (default: examples/agent-framework/logs/<timestamp>)",
    )
    parser.add_argument(
        "--smoke-test",
        action="store_true",
        help="Run a tool smoke test (write_file + read_file + run python) instead of solving issue.md",
    )
    parser.add_argument(
        "--no-sandbox",
        action="store_true",
        help="Skip Hyperlight sandbox init and omit execute_code (testbed tools only)",
    )
    parser.add_argument(
        "--skip-testbed-venv",
        action="store_true",
        help="Do not auto-create/install the testbed .venv (pytest deps)",
    )
    parser.add_argument(
        "--interactive",
        action="store_true",
        help="Run the interactive multi-turn REPL",
    )
    parser.add_argument(
        "--devui",
        action="store_true",
        help="Run the DevUI web interface",
    )
    parser.add_argument(
        "--prompt",
        action="append",
        dest="prompts",
        help="Override the default issue-solving prompt. May be provided multiple times.",
    )
    return parser.parse_args()


# --- Testbed tools -----------------------------------------------------------


@tool
def read_file(
    path: Annotated[str, Field(description="File path relative to the testbed root.")],
) -> str:
    """Read a UTF-8 text file from the testbed."""
    target = _resolve_testbed_path(path)
    print(f"Reading file: {target}")
    if not target.is_file():
        result = f"Error: file not found: {path}"
    else:
        try:
            text = target.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            result = f"Error: file is not valid UTF-8 text: {path}"
        else:
            if len(text) > MAX_FILE_CHARS:
                result = text[:MAX_FILE_CHARS] + f"\n\n...[truncated, {len(text)} chars total]"
            else:
                result = text
    _log_tool_call("read_file", {"path": path}, result)
    return result


@tool
def write_file(
    path: Annotated[str, Field(description="File path relative to the testbed root.")],
    content: Annotated[str, Field(description="Full file contents to write.")],
) -> str:
    """Create or overwrite a UTF-8 text file in the testbed."""
    print(f"Writing file: {path}")
    target = _resolve_testbed_path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")
    result = f"Wrote {len(content)} chars to {path}"
    _log_tool_call(
        "write_file",
        {"path": path, "content": content, "content_chars": len(content)},
        result,
    )
    return result


def _bash_env(root: Path) -> dict[str, str]:
    """Build an env for testbed commands that does not inherit the agent project venv.

    When this script is launched via `uv run`, PATH points at hyperlight-sandbox/.venv
    (often without pip/pytest). Prefer the testbed's own `.venv` instead.
    """
    env = os.environ.copy()
    env.pop("VIRTUAL_ENV", None)
    env.pop("PYTHONHOME", None)
    env.pop("UV_PROJECT_ENVIRONMENT", None)

    path_parts = [p for p in env.get("PATH", "").split(os.pathsep) if p]
    # Drop the hyperlight project venv so `python`/`pytest` resolve to the testbed.
    project_venv_bin = str((_repo_root() / ".venv" / "bin").resolve())
    path_parts = [p for p in path_parts if Path(p).resolve().as_posix() != Path(project_venv_bin).as_posix()]

    testbed_venv_bin = root / ".venv" / "bin"
    if testbed_venv_bin.is_dir():
        path_parts = [str(testbed_venv_bin), *path_parts]
        env["VIRTUAL_ENV"] = str(root / ".venv")

    # Ensure common user-local tools (uv) remain available.
    home_local = Path.home() / ".local" / "bin"
    if home_local.is_dir() and str(home_local) not in path_parts:
        path_parts.append(str(home_local))

    env["PATH"] = os.pathsep.join(path_parts)
    return env


def _ensure_testbed_venv(root: Path) -> None:
    """Create testbed/.venv and install project + requirements-dev if present."""
    venv_dir = root / ".venv"
    req = root / "requirements-dev.txt"
    setup_py = root / "setup.py"
    pyproject = root / "pyproject.toml"
    if not (req.is_file() or setup_py.is_file() or pyproject.is_file()):
        return

    uv = shutil.which("uv") or str(Path.home() / ".local" / "bin" / "uv")
    if not Path(uv).is_file():
        print("⚠️  uv not found; skipping testbed venv bootstrap (pytest may be unavailable)")
        return

    if not venv_dir.is_dir():
        print(f"🐍 Creating testbed venv: {venv_dir}")
        subprocess.run([uv, "venv", str(venv_dir)], cwd=root, check=True)

    install_cmd = [uv, "pip", "install", "--python", str(venv_dir / "bin" / "python")]
    if setup_py.is_file() or pyproject.is_file():
        install_cmd.extend(["-e", "."])
    if req.is_file():
        install_cmd.extend(["-r", str(req)])

    print(f"📦 Installing testbed deps: {' '.join(install_cmd)}")
    subprocess.run(install_cmd, cwd=root, check=True)
    print("✅ Testbed venv ready")


@tool
def run_bash(
    command: Annotated[str, Field(description="Shell command to run with cwd=testbed root.")],
) -> str:
    """Run a bash command inside the testbed directory."""
    root = _require_testbed()
    env = _bash_env(root)
    try:
        print(f"Running command: {command}")
        completed = subprocess.run(
            ["bash", "-lc", command],
            cwd=root,
            env=env,
            capture_output=True,
            text=True,
            timeout=DEFAULT_BASH_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        result = f"Error: command timed out after {DEFAULT_BASH_TIMEOUT_S}s: {command}"
        _log_tool_call("run_bash", {"command": command, "cwd": str(root)}, result)
        return result
    parts = [
        f"exit_code={completed.returncode}",
        "--- stdout ---",
        completed.stdout.rstrip() or "(empty)",
        "--- stderr ---",
        completed.stderr.rstrip() or "(empty)",
    ]
    result = "\n".join(parts)
    _log_tool_call("run_bash", {"command": command, "cwd": str(root)}, result)
    return result


# --- Hyperlight sandbox ------------------------------------------------------


def _init_sandbox() -> None:
    """Initialize the sandbox and take a snapshot. Call once at program start."""
    global _sandbox, _snapshot, _input_dir

    module_path = _default_module_path()
    if not module_path.exists():
        raise RuntimeError(
            "Hyperlight Wasm module not found.\n"
            f"  module: {module_path} (MISSING)\n"
            "Build the python-sandbox AOT module first (`just wasm::guest-build`)."
        )

    start = time.perf_counter()
    _input_dir = tempfile.TemporaryDirectory(prefix="hyperlight-agent-input-")
    (Path(_input_dir.name) / "team.json").write_text(
        '{"members": [{"name": "Alice", "role": "eng"}, {"name": "Bob", "role": "pm"}]}'
    )

    _sandbox = Sandbox(
        backend="wasm",
        module_path=str(module_path),
        input_dir=_input_dir.name,
    )
    _sandbox.allow_domain("https://httpbin.org", methods=["GET"])
    _sandbox.run("None")
    _snapshot = _sandbox.snapshot()
    elapsed_ms = (time.perf_counter() - start) * 1000
    print(f"📸 Sandbox initialized and snapshotted ({elapsed_ms:.0f}ms)")


def _get_sandbox() -> Sandbox:
    """Restore sandbox to clean snapshot state and return it."""
    if _sandbox is None or _snapshot is None:
        raise RuntimeError("Hyperlight sandbox is not initialized (use without --no-sandbox).")
    _sandbox.restore(_snapshot)
    return _sandbox


@tool
async def execute_code(
    code: Annotated[
        str,
        Field(description="Python code to execute in an isolated Hyperlight Wasm sandbox."),
    ],
) -> str:
    """Execute Python in Hyperlight with snapshot/restore between calls."""
    try:
        print(f"--- generated code ---\n{code}\n--- end ---\n")
        sandbox = _get_sandbox()
        start = time.perf_counter()
        result = sandbox.run(code=code)
        elapsed_ms = (time.perf_counter() - start) * 1000
        if result.success:
            stdout = result.stdout.replace("\r\n", "\n")
            print(f"⏱️  execute_code completed ({elapsed_ms:.1f}ms)")
            if not stdout:
                out = "Code executed successfully (no output)."
            else:
                out = (
                    "The code ran successfully. Here is the exact output — "
                    "include it verbatim in your response:\n\n"
                    f"```\n{stdout}\n```"
                )
        else:
            stderr = result.stderr or "Unknown error"
            print(f"⏱️  execute_code failed ({elapsed_ms:.1f}ms)")
            out = f"Execution error:\n{stderr}"
    except Exception as exc:
        out = f"Sandbox error: {exc}"
    _log_tool_call("execute_code", {"code": code}, out)
    return out


def _use_sandbox(args: argparse.Namespace) -> bool:
    """Smoke tests only need testbed tools; sandbox is optional otherwise."""
    if args.smoke_test or args.no_sandbox:
        return False
    return True


# --- Agent setup -------------------------------------------------------------


def _init_testbed(testbed: Path | None, *, bootstrap_venv: bool = True) -> Path:
    global _testbed_root
    if testbed is not None:
        root = testbed.expanduser().resolve()
        if not root.is_dir():
            raise FileNotFoundError(f"Testbed directory does not exist: {root}")
        _testbed_root = root
        print(f"📁 Testbed: {_testbed_root}")
    else:
        demo = Path(tempfile.mkdtemp(prefix="hyperlight-testbed-"))
        (demo / "README.md").write_text("# Demo testbed\n\nCreated by copilot_agent.py\n")
        (demo / "hello.py").write_text("print('hello from testbed')\n")
        _testbed_root = demo
        print(f"📁 Testbed (temp demo): {_testbed_root}")

    if bootstrap_venv:
        try:
            _ensure_testbed_venv(_testbed_root)
        except Exception as exc:
            print(f"⚠️  Testbed venv bootstrap failed: {exc}")
    return _testbed_root


def _load_issue_text(issue_path: Path | None = None) -> str:
    """Load issue.md from the testbed (or an explicit path)."""
    if issue_path is not None:
        path = issue_path.expanduser().resolve()
    else:
        path = _require_testbed() / "issue.md"
    if not path.is_file():
        raise FileNotFoundError(
            f"issue.md not found at {path}. Copy it from the SWE-bench image or pass --issue."
        )
    text = path.read_text(encoding="utf-8").strip()
    if not text:
        raise RuntimeError(f"issue.md is empty: {path}")
    print(f"📄 Loaded issue: {path} ({len(text)} chars)")
    return text


def _build_solve_prompt(issue_text: str) -> str:
    return ISSUE_SOLVE_PROMPT.format(issue=issue_text)


def create_chat_client(provider: str, model: str) -> BaseChatClient:
    """Create the chat client used by Agent."""
    if provider == "openai":
        from agent_framework.openai import OpenAIChatClient

        if not os.environ.get("OPENAI_API_KEY"):
            raise RuntimeError("OPENAI_API_KEY is required when --provider openai")
        return OpenAIChatClient(model=model)

    if provider == "copilot":
        raise RuntimeError("create_chat_client() is only used for the openai provider")

    raise ValueError(f"Unknown provider: {provider}")


def create_agent(
    provider: str = "openai",
    model: str = DEFAULT_OPENAI_MODEL,
    *,
    enable_sandbox: bool = True,
) -> Any:
    tools: list[Any] = [read_file, write_file, run_bash]
    if enable_sandbox:
        tools.append(execute_code)

    instructions = SYSTEM_PROMPT
    if not enable_sandbox:
        instructions += (
            "\n\nNote: execute_code / Hyperlight sandbox is disabled for this run. "
            "Use only read_file, write_file, and run_bash."
        )

    if provider == "openai":
        client = create_chat_client(provider, model)
        return Agent(
            client=client,
            name="HyperlightSandbox",
            instructions=instructions,
            tools=tools,
        )

    if provider == "copilot":
        from agent_framework.github import GitHubCopilotAgent
        from copilot import PermissionHandler

        return GitHubCopilotAgent(
            name="HyperlightSandbox",
            default_options={
                "instructions": instructions,
                "on_permission_request": PermissionHandler.approve_all,
            },
            tools=tools,
        )

    raise ValueError(f"Unknown provider: {provider}")


async def main(args: argparse.Namespace) -> None:
    _init_testbed(args.testbed, bootstrap_venv=not args.skip_testbed_venv)
    _init_log_dir(args.log_dir)
    enable_sandbox = _use_sandbox(args)
    if enable_sandbox:
        _init_sandbox()
    else:
        print("⏭️  Skipping Hyperlight sandbox init (testbed tools only)")
    agent = create_agent(
        provider=args.provider,
        model=args.model,
        enable_sandbox=enable_sandbox,
    )
    async with agent:
        session = agent.create_session()
        if args.interactive:
            print("Hyperlight Wasm Sandbox Agent (type 'quit' to exit)\n")
            while True:
                try:
                    prompt = input("You: ").strip()
                except (EOFError, KeyboardInterrupt):
                    break
                if not prompt or prompt.lower() in ("quit", "exit"):
                    break
                result = await agent.run(prompt, session=session)
                print(f"Agent: {result}\n")
            return

        if args.prompts:
            prompts = args.prompts
        elif args.smoke_test:
            prompts = [SMOKE_TEST_PROMPT]
        else:
            issue_text = _load_issue_text(args.issue)
            prompts = [_build_solve_prompt(issue_text)]

        for prompt in prompts:
            print(f"User: {prompt}\n")
            result = await agent.run(prompt, session=session)
            print(f"Agent: {result}\n")

    if _log_dir is not None:
        print(f"📝 Tool logs written to: {_log_dir}")


if __name__ == "__main__":
    args = parse_args()
    if args.devui:
        from agent_framework.devui import serve

        _init_testbed(args.testbed, bootstrap_venv=not args.skip_testbed_venv)
        _init_log_dir(args.log_dir)
        enable_sandbox = _use_sandbox(args)
        if enable_sandbox:
            _init_sandbox()
        agent = create_agent(
            provider=args.provider,
            model=args.model,
            enable_sandbox=enable_sandbox,
        )
        serve(entities=[agent], auto_open=True)
    else:
        asyncio.run(main(args))
