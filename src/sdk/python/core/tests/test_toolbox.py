import asyncio
import unittest
from unittest.mock import patch

from hyperlight_sandbox import ExecutionResult, Toolbox


class ToolboxTests(unittest.TestCase):
    @patch("hyperlight_sandbox.Sandbox")
    def test_execute_cli_forwards_to_wasm_toolbox(self, sandbox_cls):
        sandbox_cls.return_value.run.return_value = ExecutionResult(
            stdout="ok\n", stderr="", exit_code=0
        )
        toolbox = Toolbox(module_path="toolbox.aot", work_dir="/host/work")
        result = asyncio.run(toolbox.execute_cli("pwd"))

        sandbox_cls.assert_called_once_with(
            backend="wasm",
            module=None,
            module_path="toolbox.aot",
            work_dir="/host/work",
            work_dir_access="ro",
            temp_dir=True,
        )
        sandbox_cls.return_value.run.assert_called_once_with("pwd")
        self.assertEqual(result.stdout, "ok\n")
        self.assertFalse(result.timed_out)
        self.assertFalse(result.truncated)

    @patch("hyperlight_sandbox.Sandbox")
    def test_execute_cli_reports_guest_truncation_marker(self, sandbox_cls):
        sandbox_cls.return_value.run.return_value = ExecutionResult(
            stdout="", stderr="[toolbox output truncated]\n", exit_code=0
        )
        toolbox = Toolbox(module_path="toolbox.aot", work_dir="/host/work")
        result = asyncio.run(toolbox.execute_cli("cat huge"))
        self.assertTrue(result.truncated)
