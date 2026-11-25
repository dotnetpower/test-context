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

    def test_reconfiguration_preserves_custom_content_after_managed_block(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            defs_dir = (
                root
                / ".vscode-server/bin/abc/node_modules/@github/copilot-sdk-linux-x64/definitions"
            )
            defs_dir.mkdir(parents=True)
            explore_yaml = defs_dir / "explore.agent.yaml"
            explore_yaml.write_text(
                "name: explore\ntools:\n  - view\nprompt: |\n  Explore prompt.\n",
                encoding="utf-8",
            )
            explore_md = (
                root
                / ".vscode-server/data/User/globalStorage/github.copilot-chat/explore-agent/Explore.agent.md"
            )
            explore_md.parent.mkdir(parents=True)
            explore_md.write_text(
                "---\nname: Explore\ntools: ['search']\n---\nExplore body.\n",
                encoding="utf-8",
            )

            configure_subagents.configure_all(root)

            # Append custom user instructions AFTER the managed block (both delimited and legacy)
            yaml_custom = "  Custom trailing team rule.\n  Another line.\n"
            explore_yaml.write_text(
                explore_yaml.read_text(encoding="utf-8") + "\n" + yaml_custom,
                encoding="utf-8",
            )
            md_custom = "\n## Team Overrides\nAlways keep this section.\n"
            explore_md.write_text(
                explore_md.read_text(encoding="utf-8") + md_custom,
                encoding="utf-8",
            )

            # Mutate a line inside the managed block so reconfiguration rewrites it
            explore_yaml.write_text(
                explore_yaml.read_text(encoding="utf-8").replace("<=120L", "<=100L"),
                encoding="utf-8",
            )
            explore_md.write_text(
                explore_md.read_text(encoding="utf-8").replace("<=120L", "<=100L"),
                encoding="utf-8",
            )

            res = configure_subagents.configure_all(root)
            self.assertEqual(res["patched_yaml"], 1)
            self.assertEqual(res["patched_md"], 1)

            final_yaml = explore_yaml.read_text(encoding="utf-8")
            self.assertIn("Custom trailing team rule.", final_yaml)
            self.assertIn("Another line.", final_yaml)
            self.assertIn("<=120L", final_yaml)

            final_md = explore_md.read_text(encoding="utf-8")
            self.assertIn("## Team Overrides", final_md)
            self.assertIn("Always keep this section.", final_md)
            self.assertIn("<=120L", final_md)

            # Legacy block without END_MARKER should replace exact old numbered instructions AND
            # preserve trailing custom user text/bullets (even sharing prefixes like `- Source search/read:`).
            legacy_yaml = defs_dir / "task.agent.yaml"
            legacy_yaml.write_text(
                "name: task\nprompt: |\n  Base.\n\n"
                f"  {configure_subagents.MARKER}\n"
                "  **Code Context (`cctx`) & Test Context (`tctx`) Priority (MANDATORY FIRST PATH):**\n"
                "  - For locating or reading saved source in a registered workspace, ALWAYS use Code Context FIRST before `view`, `grep`, `rg`, `glob`, or `bash`:\n"
                "    1. Prefer native `cctxNativeContext`, `cctxNativeRead`, `cctxNativeGrep`, `cctxNativeFiles` when callable; otherwise use MCP `code-context-code_context`, `code-context-code_read`, `code-context-code_grep`, `code-context-code_files`.\n"
                "    2. Always pass the explicit absolute `root` parameter (shown in `<environment_context>` or your task prompt). Keep `cctxNativeContext` `limit <= 64` (`radius 0..100`) and keep `cctxNativeRead` / `code_read` batches `<= 120` total lines (`<8KiB`) so responses stay inline.\n"
                "  - Source search/read: retain my custom YAML policy.\n"
                "\n  User custom footer after legacy block.\n",
                encoding="utf-8",
            )
            legacy_md = (
                root
                / ".vscode-server/data/User/globalStorage/github.copilot-chat/explore-agent/Explore.agent.md"
            )
            legacy_md.write_text(
                "---\nname: Explore\ntools: ['search']\n---\nExplore body.\n\n"
                f"{configure_subagents.MD_MARKER}\n"
                "## Code Context (`cctx`) & Test Context (`tctx`) Priority (Mandatory First Path)\n\n"
                "- For locating or reading saved source in a registered workspace, ALWAYS use `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep`, and `#cctxNativeFiles` (or MCP `code-context-code_*`) FIRST before built-in search/read tools.\n"
                "- On `CACHE_MISS`, use a permitted host search/read for that single lookup only, then resume using Code Context on subsequent turns.\n"
                "- Source search/read: retain my custom markdown policy.\n",
                encoding="utf-8",
            )
            configure_subagents.configure_all(root)
            migrated_yaml = legacy_yaml.read_text(encoding="utf-8")
            self.assertNotIn("Prefer native `cctxNativeContext`", migrated_yaml)
            self.assertIn("  - Source search/read: retain my custom YAML policy.", migrated_yaml)
            self.assertIn("User custom footer after legacy block.", migrated_yaml)
            migrated_md = legacy_md.read_text(encoding="utf-8")
            self.assertNotIn("Mandatory First Path", migrated_md)
            self.assertIn("- Source search/read: retain my custom markdown policy.", migrated_md)

    def test_patch_yaml_preserves_indentless_tools_4space_prompt_chomp_and_trailing_keys(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            agent_yaml = Path(tmp) / "explore.yaml"
            agent_yaml.write_text(
                "tools:\n"
                "- lsp\n"
                "- grep\n"
                "description: custom explore\n"
                "prompt: |-\n"
                "    You are a custom explore agent.\n"
                "    Follow instructions carefully.\n"
                "model: gpt-5\n",
                encoding="utf-8",
            )
            changed = configure_subagents.patch_yaml_agent(agent_yaml)
            self.assertTrue(changed)
            patched = agent_yaml.read_text(encoding="utf-8")
            # Indentless tools sequence keeps indentless items before `description:`
            self.assertIn("- lsp\n# Code Context & Test Context tools (read-only)\n- cctxNativeFiles\n", patched)
            self.assertIn("- grep\ndescription: custom explore\n", patched)
            # 4-space indented `prompt: |-` gets 4-space managed block before `model: gpt-5`
            self.assertIn("    # [cctx-subagent-instructions]\n", patched)
            self.assertLess(patched.index("    # [/cctx-subagent-instructions]"), patched.index("model: gpt-5"))
            # Idempotent re-run preserves 4-space indentation and trailing key
            self.assertFalse(configure_subagents.patch_yaml_agent(agent_yaml))

    def test_patch_yaml_flow_tools_and_empty_explicit_indent_prompt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            agent_yaml = Path(tmp) / "explore_flow.yaml"
            agent_yaml.write_text(
                "tools: [view, lsp]\n"
                "prompt: |4-  \n"
                "model: gpt-5\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(agent_yaml))
            patched = agent_yaml.read_text(encoding="utf-8")
            self.assertIn("tools: [view, lsp, cctxNativeFiles,", patched)
            self.assertIn("    # [cctx-subagent-instructions]\n", patched)
            self.assertLess(patched.index("    # [/cctx-subagent-instructions]"), patched.index("model: gpt-5"))
            self.assertFalse(configure_subagents.patch_yaml_agent(agent_yaml))

    def test_patch_yaml_multiline_flow_tools(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            agent_yaml = Path(tmp) / "explore_multi_flow.yaml"
            agent_yaml.write_text(
                'tools: [\n'
                '  "name\\\\",\n'
                '  view, # keep [read-only] tools\n'
                '  lsp # comment\n'
                ']\n'
                'prompt: |\n'
                '  Explore prompt mentioning strictToolsList: true in prose.\n',
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(agent_yaml))
            patched = agent_yaml.read_text(encoding="utf-8")
            self.assertIn("  lsp,\n", patched)
            self.assertNotIn("# comment,", patched)
            self.assertIn('  "cctxNativeFiles"', patched)
            self.assertIn('  "tctxNativeRun"', patched)
            self.assertFalse(configure_subagents.patch_yaml_agent(agent_yaml))

    def test_patch_yaml_respects_tagged_quoted_strict_tools_list(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            agent_yaml = Path(tmp) / "explore_strict.yaml"
            agent_yaml.write_text(
                'strictToolsList: !!bool "true"\n'
                'tools: [view, lsp]\n'
                'prompt: |\n'
                '  Strict agent.\n',
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(agent_yaml))
            patched = agent_yaml.read_text(encoding="utf-8")
            self.assertIn("tools: [view, lsp]\n", patched)
            self.assertNotIn("cctxNativeFiles,", patched)
            self.assertIn("  # [cctx-subagent-instructions]\n", patched)

    def test_patch_explore_md_checks_front_matter_tools_not_body_prose(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            md_path = Path(tmp) / "explore.agent.md"
            md_path.write_text(
                "---\n"
                "name: Explore\n"
                "tools: ['read', 'search']\n"
                "---\n"
                "Use tctxNativeAffected and cctxNativeContext when inspecting code.\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_explore_agent_md(md_path))
            patched = md_path.read_text(encoding="utf-8")
            fm = patched.split("---", 2)[1]
            self.assertIn("'tctxNativeAffected'", fm)
            self.assertIn("'cctxNativeContext'", fm)
            self.assertFalse(configure_subagents.patch_explore_agent_md(md_path))

    def test_patch_explore_md_preserves_multiline_flow_tools_with_trailing_comment(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            md_path = Path(tmp) / "explore_multi.agent.md"
            md_path.write_text(
                "---\n"
                "name: Explore\n"
                "tools: [\n"
                "  'read',\n"
                "  'search' # keep search\n"
                "]\n"
                "---\n"
                "Explore body.\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_explore_agent_md(md_path))
            patched = md_path.read_text(encoding="utf-8")
            self.assertIn("  'search' # keep search\n]\n", patched)
            self.assertNotIn("# keep search]", patched)
            self.assertIn("  'tctxNativeAffected',\n", patched)
            self.assertFalse(configure_subagents.patch_explore_agent_md(md_path))

    def test_patch_explore_md_and_yaml_support_commented_tools_key(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            md_path = Path(tmp) / "explore_comment_key.agent.md"
            md_path.write_text(
                "---\n"
                "name: Explore\n"
                "tools: # allowed tools\n"
                "  - read\n"
                "---\n"
                "Explore body.\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_explore_agent_md(md_path))
            patched_md = md_path.read_text(encoding="utf-8")
            self.assertIn("tools: # allowed tools\n", patched_md)
            self.assertIn("  - tctxNativeAffected\n", patched_md)
            self.assertFalse(configure_subagents.patch_explore_agent_md(md_path))

            yaml_path = Path(tmp) / "explore_comment_key.yaml"
            yaml_path.write_text(
                "tools: # allowed tools\n"
                "  - lsp\n"
                "prompt: |\n"
                "  Explore prompt.\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(yaml_path))
            patched_yaml = yaml_path.read_text(encoding="utf-8")
            self.assertIn("  - cctxNativeFiles\n", patched_yaml)
            self.assertFalse(configure_subagents.patch_yaml_agent(yaml_path))

    def test_patch_yaml_1space_indented_prompt_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            yaml_path = Path(tmp) / "explore_1space.yaml"
            yaml_path.write_text(
                "tools: [view]\n"
                "prompt: |\n"
                " You are an explore agent.\n"
                "model: gpt-5\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(yaml_path))
            patched = yaml_path.read_text(encoding="utf-8")
            self.assertEqual(patched.count(configure_subagents.MARKER), 1)
            self.assertEqual(patched.count(configure_subagents.END_MARKER), 1)
            self.assertIn(" # [/cctx-subagent-instructions]\n", patched)
            # Second run must recognize the 1-space end marker and stay idempotent
            self.assertFalse(configure_subagents.patch_yaml_agent(yaml_path))

    def test_patch_yaml_preserves_user_text_between_unterminated_and_second_block(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            yaml_path = Path(tmp) / "explore_unterminated.yaml"
            yaml_path.write_text(
                "tools: [view]\n"
                "prompt: |\n"
                "  # [cctx-subagent-instructions]\n"
                "  IMPORTANT USER INSTRUCTION MUST SURVIVE.\n"
                "  # [cctx-subagent-instructions]\n"
                "  old body\n"
                "  # [/cctx-subagent-instructions]\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(yaml_path))
            patched = yaml_path.read_text(encoding="utf-8")
            self.assertIn("IMPORTANT USER INSTRUCTION MUST SURVIVE.", patched)

            spaced_key_path = Path(tmp) / "explore_unterminated_spaced_key.yaml"
            spaced_key_path.write_text(
                "tools: [view]\n"
                "prompt: |\n"
                "  # [cctx-subagent-instructions]\n"
                "description : |\n"
                "  KEEP THIS DESCRIPTION.\n"
                "  # [/cctx-subagent-instructions]\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(spaced_key_path))
            patched_spaced = spaced_key_path.read_text(encoding="utf-8")
            self.assertIn("description : |\n  KEEP THIS DESCRIPTION.", patched_spaced)

            quoted_dotted_key_path = Path(tmp) / "explore_unterminated_quoted_key.yaml"
            quoted_dotted_key_path.write_text(
                "tools: [view]\n"
                "prompt: |\n"
                "  # [cctx-subagent-instructions]\n"
                '"metadata.version": |\n'
                "  KEEP THIS VERSION BLOCK.\n"
                "  # [/cctx-subagent-instructions]\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(quoted_dotted_key_path))
            patched_quoted = quoted_dotted_key_path.read_text(encoding="utf-8")
            self.assertIn('"metadata.version": |\n  KEEP THIS VERSION BLOCK.', patched_quoted)

            unindented_comment_path = Path(tmp) / "explore_unterminated_comment.yaml"
            unindented_comment_path.write_text(
                "tools: [view]\n"
                "prompt: |\n"
                "  # [cctx-subagent-instructions]\n"
                "# UNINDENTED TOP-LEVEL COMMENT MUST SURVIVE\n"
                "# [/cctx-subagent-instructions]\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(unindented_comment_path))
            patched_comment = unindented_comment_path.read_text(encoding="utf-8")
            self.assertIn("# UNINDENTED TOP-LEVEL COMMENT MUST SURVIVE\n", patched_comment)

            dedented_comment_path = Path(tmp) / "explore_unterminated_dedented_comment.yaml"
            dedented_comment_path.write_text(
                "tools: [view]\n"
                "prompt: |2\n"
                "  # [cctx-subagent-instructions]\n"
                " # 1-SPACE DEDENTED COMMENT MUST SURVIVE\n"
                " # [/cctx-subagent-instructions]\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(dedented_comment_path))
            patched_dedented = dedented_comment_path.read_text(encoding="utf-8")
            self.assertIn(" # 1-SPACE DEDENTED COMMENT MUST SURVIVE\n", patched_dedented)

            insert_dedented_path = Path(tmp) / "explore_insert_dedented_comment.yaml"
            insert_dedented_path.write_text(
                "tools: [view]\n"
                "prompt: |2\n"
                "  First prompt line.\n"
                " # 1-space comment outside |2 scalar\n"
                "model: gpt-5\n",
                encoding="utf-8",
            )
            self.assertTrue(configure_subagents.patch_yaml_agent(insert_dedented_path))
            patched_insert = insert_dedented_path.read_text(encoding="utf-8")
            self.assertLess(
                patched_insert.index("  # [/cctx-subagent-instructions]"),
                patched_insert.index(" # 1-space comment outside |2 scalar"),
            )


if __name__ == "__main__":
    unittest.main()
