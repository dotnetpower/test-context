import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = (
    Path(__file__).resolve().parents[1]
    / "vscode-extension/scripts/configure-mcp.py"
)
SPEC = importlib.util.spec_from_file_location("configure_mcp", SCRIPT)
configure_mcp = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(configure_mcp)
EXECUTABLE = "/home/test/.local/bin/tctx"


class ConfigureMcpTests(unittest.TestCase):
    def test_native_default_removes_only_managed_entry_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "mcp.json"
            original = {
                "servers": {
                    "other": {"command": "keep"},
                    "test-context": {
                        "type": "stdio",
                        "command": EXECUTABLE,
                        "args": ["shared-mcp"],
                        "cwd": "/home/test",
                    },
                },
                "inputs": [{"id": "keep"}],
            }
            path.write_text(json.dumps(original))
            self.assertTrue(configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", False))
            del original["servers"]["test-context"]
            self.assertEqual(json.loads(path.read_text()), original)
            self.assertFalse(configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", False))

    def test_windows_opt_in_uses_wsl_and_migrates_linux_command(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "mcp.json"
            path.write_text(json.dumps({
                "servers": {
                    "test-context": {"command": EXECUTABLE, "args": ["shared-mcp"]}
                }
            }))
            self.assertTrue(configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", True))
            server = json.loads(path.read_text())["servers"]["test-context"]
            self.assertEqual(server["command"], "wsl.exe")
            self.assertEqual(server["args"], [
                "--distribution", "Ubuntu", "--exec", EXECUTABLE, "shared-mcp"
            ])
            self.assertNotIn("cwd", server)
            self.assertFalse(configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", True))
            self.assertTrue(configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", False))

    def test_remote_opt_in_creates_config_and_native_default_does_not(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "User/mcp.json"
            self.assertFalse(configure_mcp.configure_mcp(path, EXECUTABLE, None, False))
            self.assertFalse(path.exists())
            self.assertTrue(configure_mcp.configure_mcp(path, EXECUTABLE, None, True))
            server = json.loads(path.read_text())["servers"]["test-context"]
            self.assertEqual(server["command"], EXECUTABLE)
            self.assertEqual(server["args"], ["shared-mcp"])

    def test_custom_registration_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "mcp.json"
            original = '{"servers":{"test-context":{"command":"custom"}}}'
            path.write_text(original)
            self.assertFalse(configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", False))
            with self.assertRaisesRegex(RuntimeError, "custom test-context"):
                configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", True)
            self.assertEqual(path.read_text(), original)

    def test_invalid_or_jsonc_input_is_not_modified(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "mcp.json"
            for original in ["// comment\n{}", "[]", '{"servers": []}']:
                path.write_text(original)
                with self.assertRaises(RuntimeError):
                    configure_mcp.configure_mcp(path, EXECUTABLE, "Ubuntu", False)
                self.assertEqual(path.read_text(), original)


if __name__ == "__main__":
    unittest.main()
