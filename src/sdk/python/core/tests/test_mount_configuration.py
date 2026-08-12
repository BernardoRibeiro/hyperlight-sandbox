"""Unit tests for Phase 2 mount configuration in the stable Python API."""

from __future__ import annotations

import unittest
from unittest.mock import patch

from hyperlight_sandbox import CodeExecutionTool, Sandbox, SandboxEnvironment


class _FakeNativeSandbox:
    def __init__(self, **kwargs):
        self.kwargs = kwargs

    def register_tool(self, *_args, **_kwargs):
        return None


class MountConfigurationTests(unittest.TestCase):
    def _sandbox(self, **kwargs) -> Sandbox:
        with (
            patch(
                "hyperlight_sandbox._load_backend",
                return_value=("wasm", _FakeNativeSandbox),
            ),
            patch(
                "hyperlight_sandbox.resolve_module_path",
                return_value="/runtime/python-sandbox.aot",
            ),
        ):
            return Sandbox(backend="wasm", **kwargs)

    def test_work_dir_defaults_to_read_only(self):
        sandbox = self._sandbox(work_dir="/host/work")

        self.assertEqual(sandbox._inner.kwargs["work_dir"], "/host/work")
        self.assertEqual(sandbox._inner.kwargs["work_dir_access"], "ro")

    def test_read_write_work_dir_requires_explicit_mode(self):
        sandbox = self._sandbox(work_dir="/host/work", work_dir_access="rw")

        self.assertEqual(sandbox._inner.kwargs["work_dir_access"], "rw")

    def test_access_aliases_are_normalized(self):
        sandbox = self._sandbox(work_dir="/host/work", work_dir_access="read_write")

        self.assertEqual(sandbox._inner.kwargs["work_dir_access"], "rw")

    def test_invalid_access_fails_before_backend_loading(self):
        with patch("hyperlight_sandbox._load_backend") as load_backend:
            with self.assertRaisesRegex(ValueError, "Expected 'ro' or 'rw'"):
                Sandbox(work_dir="/host/work", work_dir_access="write")

        load_backend.assert_not_called()

    def test_private_temp_dir_is_forwarded(self):
        sandbox = self._sandbox(temp_dir=True)

        self.assertIs(sandbox._inner.kwargs["temp_dir"], True)

    def test_environment_forwards_work_and_temp_configuration(self):
        environment = SandboxEnvironment(
            work_dir="/host/work",
            work_dir_access="rw",
            temp_dir=True,
        )
        tool = CodeExecutionTool(environment=environment)

        with (
            patch(
                "hyperlight_sandbox._load_backend",
                return_value=("wasm", _FakeNativeSandbox),
            ),
            patch(
                "hyperlight_sandbox.resolve_module_path",
                return_value="/runtime/python-sandbox.aot",
            ),
        ):
            sandbox = tool._get_sandbox()

        self.assertEqual(sandbox._inner.kwargs["work_dir"], "/host/work")
        self.assertEqual(sandbox._inner.kwargs["work_dir_access"], "rw")
        self.assertIs(sandbox._inner.kwargs["temp_dir"], True)


if __name__ == "__main__":
    unittest.main()
