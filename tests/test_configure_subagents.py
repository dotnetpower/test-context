import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "configure_subagents.py"
SPEC = importlib.util.spec_from_file_location("configure_subagents", SCRIPT)
configure_subagents = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(configure_subagents)


class ConfigureSubagentsTests(unittest.TestCase):
    def test_patches_yaml_and_explore_md_with_tctx_and_cctx_and_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            defs_dir = (
                root
                / ".vscode-server/bin/abc/node_modules/@github/copilot-sdk-linux-x64/definitions"
            )
            defs_dir.mkdir(parents=True)
            explore_yaml = defs_dir / "explore.agent.yaml"
            explore_yaml.write_text(
                "name: explore\ntools:\n  - view\n  - lsp\nprompt: |\n  Explore prompt.\n",
                encoding="utf-8",
            )
            task_yaml = defs_dir / "task.agent.yaml"
            task_yaml.write_text(
                "name: task\ntools:\n  - \"*\"\nprompt: |\n  Task prompt.\n",
                encoding="utf-8",
            )
            explore_md = (
                root
                / ".vscode-server/data/User/globalStorage/github.copilot-chat/explore-agent/Explore.agent.md"
            )
            explore_md.parent.mkdir(parents=True)
            explore_md.write_text(
                "---\nname: Explore\ntools: ['search', 'read']\n---\nExplore body.\n",
                encoding="utf-8",
            )

            first = configure_subagents.configure_all(root)
            self.assertEqual(first["definition_dirs"], 1)
            self.assertEqual(first["patched_yaml"], 2)
            self.assertEqual(first["patched_md"], 1)

            explore_text = explore_yaml.read_text(encoding="utf-8")
            self.assertIn("  - tctxNativeAffected\n", explore_text)
            self.assertIn("  - test-context-native_testAffected\n", explore_text)
            self.assertIn("  - test-context-test_affected\n", explore_text)
            self.assertIn("tctxNativeRun tctxNativeAffected tctxNativeGraph", explore_text)

            task_text = task_yaml.read_text(encoding="utf-8")
            self.assertIn("test-context-test_run", task_text)
            self.assertIn("No matching client connected", task_text)

            md_text = explore_md.read_text(encoding="utf-8")
            self.assertIn("'tctxNativeAffected'", md_text)
            self.assertIn("'test-context/*'", md_text)
            self.assertIn("#tctxNativeAffected", md_text)

            second = configure_subagents.configure_all(root)
            self.assertEqual(second["patched_yaml"], 0)
            self.assertEqual(second["patched_md"], 0)


if __name__ == "__main__":
    unittest.main()
