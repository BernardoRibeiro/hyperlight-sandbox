#!/usr/bin/env python3
"""Minimal Agent Framework + Hyperlight Wasm agent.

--testbed is mounted as guest /input (read-only). Nested repo paths are unreliable
via WASI CapFs (no '/' in relative paths; listdir('/input') often fails), so file
access for the real tree goes through call_tool → host callbacks.

After execute_code, any flat files under /output are still merged into the testbed.
Guest Python state persists across turns (no restore).
"""

from __future__ import annotations

import argparse
import asyncio
import os
import shutil
import time
from pathlib import Path
from typing import Annotated, Any

from agent_framework import Agent, BaseChatClient, tool
from hyperlight_sandbox import Sandbox
from pydantic import Field

DEFAULT_OPENAI_MODEL = "gpt-5-mini"
MAX_FILE_CHARS = 100_000
MAX_LIST_ENTRIES = 500
LIST_SKIP_DIR_NAMES = frozenset(
    {
        ".git",
        ".venv",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".tox",
        "node_modules",
        "dist",
        "build",
    }
)

SYSTEM_PROMPT = """You are a coding agent. You MUST keep calling execute_code until the user task is fully done.
Do not stop after a failed listing or a partial inspection. Retry with a different approach.

How to access files (important):
- Prefer call_tool(...) for ALL file access. call_tool is a built-in (no import).
- Do NOT rely on os.listdir('/input') — it often fails with Errno 44 even when the mount works.
- Do NOT conclude the repo is missing just because listdir failed.

Host tools via call_tool:
  call_tool('read_file', path='issue.md')
  call_tool('write_file', path='testbed/foo.py', content='...')
  call_tool('list_files', path='.', recursive=False)
  call_tool('list_files', path='testbed', recursive=True)

Paths are relative to the testbed root (the directory passed as --testbed).
Sandbox Python state persists across execute_code turns.

Typical first step:
  print(call_tool('read_file', path='issue.md'))
  print(call_tool('list_files', path='.', recursive=False))

When editing: read with call_tool('read_file'), then write the full new file with
call_tool('write_file', path=..., content=...). Print results every turn.
Keep going until the requested task is complete, then summarize.
"""

# --- State -------------------------------------------------------------------

_sandbox: Sandbox | None = None
_testbed: Path | None = None


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _default_module_path() -> Path:
    return _repo_root() / "src/wasm_sandbox/guests/python/python-sandbox.aot"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Simple Hyperlight code-mode agent")
    p.add_argument(
        "--testbed",
        type=Path,
        required=True,
        help="Host directory mounted as guest /input and used by call_tool file ops",
    )
    p.add_argument(
        "--model",
        default=os.environ.get("OPENAI_CHAT_MODEL", DEFAULT_OPENAI_MODEL),
        help=f"OpenAI model id (default: {DEFAULT_OPENAI_MODEL})",
    )
    p.add_argument(
        "--prompt",
        action="append",
        dest="prompts",
        help="User prompt (repeatable). Default: read issue.md and summarize, keep going until done.",
    )
    p.add_argument(
        "--interactive",
        action="store_true",
        help="Multi-turn REPL",
    )
    p.add_argument(
        "--module-path",
        type=Path,
        default=None,
        help="Path to python-sandbox.aot (default: built guest under src/wasm_sandbox)",
    )
    return p.parse_args()


def create_chat_client(model: str) -> BaseChatClient:
    from agent_framework.openai import OpenAIChatClient

    if not os.environ.get("OPENAI_API_KEY"):
        raise RuntimeError("OPENAI_API_KEY is required")
    return OpenAIChatClient(model=model)


def _require_testbed() -> Path:
    if _testbed is None:
        raise RuntimeError("Testbed is not initialized")
    return _testbed


def _resolve_testbed_path(path: str) -> Path:
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


def _read_file_impl(path: str) -> str:
    target = _resolve_testbed_path(path)
    print(f"Reading file: {target}")
    if not target.is_file():
        return f"Error: file not found: {path}"
    try:
        text = target.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return f"Error: file is not valid UTF-8 text: {path}"
    if len(text) > MAX_FILE_CHARS:
        return text[:MAX_FILE_CHARS] + f"\n\n...[truncated, {len(text)} chars total]"
    return text


def _write_file_impl(path: str, content: str) -> str:
    print(f"Writing file: {path}")
    target = _resolve_testbed_path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")
    return f"Wrote {len(content)} chars to {path}"


def _should_skip_dir(name: str) -> bool:
    return name in LIST_SKIP_DIR_NAMES or name.endswith(".egg-info")


def _list_files_impl(path: str = ".", recursive: bool = True) -> str:
    root = _require_testbed().resolve()
    start = _resolve_testbed_path(path)
    if not start.exists():
        return f"Error: path not found: {path}"
    if start.is_file():
        return start.relative_to(root).as_posix()

    entries: list[str] = []
    truncated = False
    if recursive:
        for dirpath, dirnames, filenames in os.walk(start):
            dirnames[:] = sorted(d for d in dirnames if not _should_skip_dir(d))
            for name in sorted(filenames):
                full = Path(dirpath) / name
                entries.append(full.relative_to(root).as_posix())
                if len(entries) >= MAX_LIST_ENTRIES:
                    truncated = True
                    break
            if truncated:
                break
    else:
        for child in sorted(start.iterdir(), key=lambda p: p.name):
            if child.is_dir() and _should_skip_dir(child.name):
                continue
            rel = child.relative_to(root).as_posix()
            entries.append(rel + ("/" if child.is_dir() else ""))
            if len(entries) >= MAX_LIST_ENTRIES:
                truncated = True
                break

    result = "\n".join(entries) if entries else "(empty)"
    if truncated:
        result += f"\n...[truncated at {MAX_LIST_ENTRIES} entries]"
    return result


def _register_sandbox_tools(sandbox: Sandbox) -> None:
    sandbox.register_tool("read_file", lambda **kw: _read_file_impl(str(kw["path"])))
    sandbox.register_tool(
        "write_file",
        lambda **kw: _write_file_impl(str(kw["path"]), str(kw.get("content", ""))),
    )
    sandbox.register_tool(
        "list_files",
        lambda **kw: _list_files_impl(
            str(kw.get("path", ".")),
            bool(kw["recursive"]) if "recursive" in kw else True,
        ),
    )


def _init_sandbox(testbed: Path, module_path: Path | None) -> Sandbox:
    global _sandbox, _testbed

    root = testbed.expanduser().resolve()
    if not root.is_dir():
        raise FileNotFoundError(f"Testbed directory does not exist: {root}")

    issue = root / "issue.md"
    if not issue.is_file():
        raise FileNotFoundError(
            f"Expected issue.md at {issue}. "
            "For examples/testbeds/CLI_Tools_Easy, pass that directory (it contains "
            "issue.md plus a nested testbed/ repo)."
        )

    resolved_module = (module_path or _default_module_path()).expanduser().resolve()
    if not resolved_module.is_file():
        raise RuntimeError(
            "Hyperlight Wasm module not found.\n"
            f"  module: {resolved_module} (MISSING)\n"
            "Build it with: just wasm::guest-build"
        )

    start = time.perf_counter()
    _testbed = root
    _sandbox = Sandbox(
        backend="wasm",
        module_path=str(resolved_module),
        input_dir=str(root),
        temp_output=True,
    )
    _register_sandbox_tools(_sandbox)

    # Warm-up + prove host tools see issue.md (do not rely on guest /input listdir).
    probe = _sandbox.run(
        "text = call_tool('read_file', path='issue.md')\n"
        "print('PROBE_CHARS', len(text))\n"
        "print(text[:120])\n"
    )
    if not probe.success or "PROBE_CHARS" not in probe.stdout:
        raise RuntimeError(
            "Sandbox tool probe failed — call_tool('read_file') could not read issue.md.\n"
            f"stdout={probe.stdout!r}\nstderr={probe.stderr!r}"
        )

    elapsed_ms = (time.perf_counter() - start) * 1000
    print(f"📁 Testbed (host + /input mount): {root}")
    print(f"📄 issue.md: {issue} ({issue.stat().st_size} bytes)")
    print(f"📤 Guest /output staging: {_sandbox.output_path()}")
    print(f"📸 Sandbox ready — call_tool read_file(issue.md) OK ({elapsed_ms:.0f}ms)")
    print(f"   probe stdout:\n{probe.stdout}")
    return _sandbox


def _get_sandbox() -> Sandbox:
    if _sandbox is None:
        raise RuntimeError("Sandbox is not initialized")
    return _sandbox


def _apply_output_to_testbed(sandbox: Sandbox) -> list[str]:
    """Copy flat files under guest /output into the testbed (CapFs has no nested paths)."""
    out_root = sandbox.output_path()
    if not out_root:
        return []
    out_root_path = Path(out_root).resolve()
    if not out_root_path.is_dir():
        return []

    testbed = _require_testbed().resolve()
    applied: list[str] = []

    for dirpath, _dirnames, filenames in os.walk(out_root_path):
        for name in filenames:
            src = Path(dirpath) / name
            rel = src.relative_to(out_root_path).as_posix()
            dest = (testbed / rel).resolve()
            try:
                dest.relative_to(testbed)
            except ValueError as exc:
                raise PermissionError(f"Output path escapes testbed: {rel}") from exc
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dest)
            applied.append(rel)
            print(f"📥 Merged /output/{rel} → testbed/{rel}")

    return applied


@tool
async def execute_code(
    code: Annotated[
        str,
        Field(
            description=(
                "Python to run in Hyperlight. Use call_tool('read_file'|'write_file'|'list_files') "
                "for the testbed. Do not rely on os.listdir('/input'). State persists across calls."
            ),
        ),
    ],
) -> str:
    """Run Python in Hyperlight; host file tools via call_tool."""
    try:
        print(f"--- generated code ---\n{code}\n--- end ---\n")
        sandbox = _get_sandbox()
        start = time.perf_counter()
        result = sandbox.run(code=code)
        elapsed_ms = (time.perf_counter() - start) * 1000

        if result.success:
            stdout = result.stdout.replace("\r\n", "\n")
            applied = _apply_output_to_testbed(sandbox)
            print(f"⏱️  execute_code completed ({elapsed_ms:.1f}ms)")
            parts = []
            if stdout:
                parts.append("stdout:\n```\n" + stdout + "\n```")
            else:
                parts.append("Code executed successfully (no stdout).")
            if applied:
                parts.append(
                    "Merged /output into testbed:\n- " + "\n- ".join(applied)
                )
            # Nudge the model if it only hit the /input listdir trap.
            if "Errno 44" in stdout or "No such file or directory: '/input'" in stdout:
                parts.append(
                    "HINT: /input listdir is unreliable. Use "
                    "call_tool('read_file', path='issue.md') and "
                    "call_tool('list_files', path='.', recursive=False) and continue."
                )
            return "\n".join(parts)

        stderr = result.stderr or "Unknown error"
        print(f"⏱️  execute_code failed ({elapsed_ms:.1f}ms)")
        return f"Execution error:\n{stderr}"
    except Exception as exc:
        return f"Sandbox error: {exc}"


def create_agent(model: str = DEFAULT_OPENAI_MODEL) -> Any:
    return Agent(
        client=create_chat_client(model),
        name="HyperlightSimpleAgent",
        instructions=SYSTEM_PROMPT,
        tools=[execute_code],
    )


def _default_prompt() -> str:
    return (
        "Solve the bug described in issue.md. Keep calling execute_code until done.\n"
        "Do not stop after inspection — implement the fix.\n\n"
        "Workflow:\n"
        "1) call_tool('read_file', path='issue.md') and understand the bug\n"
        "2) Explore under testbed/ with list_files / read_file "
        "(likely testbed/pyupgrade/_plugins/shlex_join.py and related tests)\n"
        "3) Implement the fix with call_tool('write_file', ...) — write the FULL file contents\n"
        "4) Read the related tests; update or add a regression test if needed\n"
        "5) Summarize what you changed and why\n\n"
        "Paths are relative to the testbed root (issue.md at top level; code under testbed/).\n"
        "Never rely on os.listdir('/input'). Always use call_tool for file access."
    )


async def main(args: argparse.Namespace) -> None:
    print(f"Initializing sandbox with testbed: {args.testbed}")
    _init_sandbox(args.testbed, args.module_path)
    agent = create_agent(model=args.model)

    async with agent:
        session = agent.create_session()

        if args.interactive:
            print("Simple Hyperlight agent (type 'quit' to exit)\n")
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

        prompts = args.prompts or [_default_prompt()]
        for prompt in prompts:
            print(f"User: {prompt}\n")
            result = await agent.run(prompt, session=session)
            print(f"Agent: {result}\n")


if __name__ == "__main__":
    asyncio.run(main(parse_args()))
