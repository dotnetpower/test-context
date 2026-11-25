#!/usr/bin/env python3
"""Configure Copilot SDK and VS Code subagent definitions to support Code Context (cctx) and Test Context (tctx)."""

from __future__ import annotations

import argparse
import os
import re
import tempfile
from pathlib import Path

MARKER = "# [cctx-subagent-instructions]"
END_MARKER = "# [/cctx-subagent-instructions]"
MD_MARKER = "<!-- [cctx-subagent-instructions] -->"
MD_END_MARKER = "<!-- [/cctx-subagent-instructions] -->"

EXPLORE_EXTRA_TOOLS = (
    "cctxNativeFiles",
    "cctxNativeGrep",
    "cctxNativeContext",
    "cctxNativeRead",
    "cctxNativeStatus",
    "cctxNativeWorkspaces",
    "code-context-native_codeFiles",
    "code-context-native_codeGrep",
    "code-context-native_codeContext",
    "code-context-native_codeRead",
    "code-context-code_files",
    "code-context-code_grep",
    "code-context-code_context",
    "code-context-code_read",
    "code-context-code_status",
    "tctxNativeRun",
    "tctxNativeAffected",
    "tctxNativeGraph",
    "tctxNativeStatus",
    "tctxNativeWorkspaces",
    "test-context-native_testRun",
    "test-context-native_testAffected",
    "test-context-native_testGraph",
    "test-context-test_run",
    "test-context-test_affected",
    "test-context-test_graph",
    "test-context-test_status",
)

_CORE_POLICY_LINES = (
    "Source search/read: prefer `code-context-code_*` MCP (or `#cctxNative*` in Editor Chat) before `view`/`grep`/`glob`/`bash`; pass explicit absolute `root`; keep reads <=120L (<8KiB); skip `.github/*`, `.vscode/*`, `/tmp/*`; on `CACHE_MISS` do 1 host read.",
    "Tests/lint/AST graph: prefer `test-context-test_run`, `test-context-test_affected`, `test-context-test_graph` MCP (or `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`) before shell runners; pass explicit absolute `root`.",
    "Deferred discovery (`tool_search` in small pairs): `\"code-context-code_context code-context-code_grep\"` + `\"code-context-code_read code-context-code_files\"`; `\"test-context-test_run test-context-test_affected\"` + `\"test-context-test_graph test-context-test_status\"` (or `\"tctxNativeRun tctxNativeAffected tctxNativeGraph\"`).",
    "Fallback: on `\"No matching client connected\"`, switch immediately to `code-context-code_*` / `test-context-test_*` MCP or `cctx --backend shared --root <root>` / `tctx --root <root> run` CLI.",
)

YAML_PROMPT_BLOCK = (
    f"  {MARKER}\n"
    "  **Code Context (`cctx`) & Test Context (`tctx`) Priority:**\n"
    + "".join(f"  - {line}\n" for line in _CORE_POLICY_LINES)
    + f"  {END_MARKER}\n"
)

MD_PROMPT_BLOCK = (
    f"\n{MD_MARKER}\n"
    "## Code Context (`cctx`) & Test Context (`tctx`) Priority\n\n"
    + "".join(f"- {line}\n" for line in _CORE_POLICY_LINES)
    + f"{MD_END_MARKER}\n"
)

TARGET_YAML_AGENTS = (
    "explore.agent.yaml",
    "code-review.agent.yaml",
    "rubber-duck.agent.yaml",
    "security-review.agent.yaml",
    "research.agent.yaml",
    "task.agent.yaml",
    "sidekick/github-context.yaml",
    "sidekick/github-context-memory.yaml",
)

MD_EXTRA_TOOLS = (
    "cctxNativeFiles",
    "cctxNativeGrep",
    "cctxNativeContext",
    "cctxNativeRead",
    "cctxNativeStatus",
    "cctxNativeWorkspaces",
    "code-context/*",
    "tctxNativeRun",
    "tctxNativeAffected",
    "tctxNativeGraph",
    "tctxNativeStatus",
    "tctxNativeWorkspaces",
    "test-context/*",
)


def atomic_write(path: Path, content: str) -> None:
    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp_name, path)
    except BaseException:
        Path(tmp_name).unlink(missing_ok=True)
        raise


_EXACT_LEGACY_YAML_LINES = frozenset(
    {
        "  **Code Context (`cctx`) & Test Context (`tctx`) Priority (MANDATORY FIRST PATH):**",
        "  **Code Context (`cctx`) & Test Context (`tctx`) Priority:**",
        "  - For locating or reading saved source in a registered workspace, ALWAYS use Code Context FIRST before `view`, `grep`, `rg`, `glob`, or `bash`:",
        "    1. Prefer native `cctxNativeContext`, `cctxNativeRead`, `cctxNativeGrep`, `cctxNativeFiles` when callable; otherwise use MCP `code-context-code_context`, `code-context-code_read`, `code-context-code_grep`, `code-context-code_files`.",
        "    2. Always pass the explicit absolute `root` parameter (shown in `<environment_context>` or your task prompt). Keep `cctxNativeContext` `limit <= 64` (`radius 0..100`) and keep `cctxNativeRead` / `code_read` batches `<= 120` total lines (`<8KiB`) so responses stay inline.",
        "    3. Do NOT pass hidden paths (`.github/*`, `.vscode/*`, `.gitignore`) or `/tmp/*` files to Code Context; use `view` directly for those.",
        "    4. If Code Context or Test Context tools are deferred or not visible in your active tool list, call `tool_search` (`tool_search_tool`) FIRST with exact names: `\"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles code-context-code_context code-context-code_read code-context-code_grep code-context-code_files tctxNativeRun tctxNativeAffected tctxNativeGraph tctxNativeStatus test-context-test_run test-context-test_affected test-context-test_graph test-context-test_status\"`.",
        "    5. If a native `cctxNative*` or `tctxNative*` call fails with `\"No matching client connected\"`, immediately switch to `code-context-code_*` / `test-context-test_*` MCP tools (or `cctx --backend shared --root <root>` / `tctx --root <root> run` CLI).",
        "    6. On `CACHE_MISS`, use a permitted host read/search for that single lookup only, then resume using Code Context on subsequent turns.",
        "    7. For running or inspecting tests/lints (`pytest`, `cargo test`, `vitest`, `jest`, `node --test`, `ruff`, `eslint`) or AST test dependency graphs in a registered workspace, ALWAYS use `tctxNativeRun`, `tctxNativeAffected`, `tctxNativeGraph` (or MCP `test-context-test_run`, `test-context-test_affected`, `test-context-test_graph`) FIRST before `bash`, passing the explicit absolute `root`.",
        *(f"  - {line}" for line in _CORE_POLICY_LINES),
    }
)

_EXACT_LEGACY_MD_LINES = frozenset(
    {
        "## Code Context (`cctx`) & Test Context (`tctx`) Priority (Mandatory First Path)",
        "## Code Context (`cctx`) & Test Context (`tctx`) Priority",
        "- For locating or reading saved source in a registered workspace, ALWAYS use `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep`, and `#cctxNativeFiles` (or MCP `code-context-code_*`) FIRST before built-in search/read tools.",
        "- For running or inspecting tests/lints or AST test dependency graphs in a registered workspace, ALWAYS use `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, and `#tctxNativeStatus` (or MCP `test-context-test_*`) FIRST before shell test/lint commands.",
        "- Always pass the explicit absolute `root` parameter when known, keep `cctxNativeContext` `limit <= 64`, and keep read batches `<= 120` lines (`<8KiB`). Never send `.github/*`, `.vscode/*`, or `/tmp/*` paths to Code Context.",
        "- If tools are deferred, discover them with `tool_search` using exact names: `\"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles code-context-code_context code-context-code_read code-context-code_grep code-context-code_files tctxNativeRun tctxNativeAffected tctxNativeGraph tctxNativeStatus test-context-test_run test-context-test_affected test-context-test_graph test-context-test_status\"`.",
        "- On `CACHE_MISS`, use a permitted host search/read for that single lookup only, then resume using Code Context on subsequent turns.",
        *(f"- {line}" for line in _CORE_POLICY_LINES),
    }
)


def _extract_yaml_tail_after_managed_block(tail: str, min_indent: int = 1) -> str:
    # Inside an indented YAML `prompt: |` block scalar, the end marker MUST itself be indented
    # by at least `min_indent` leading whitespace characters AND NO non-empty line with fewer
    # than `min_indent` leading spaces/tabs (including top-level keys or lower-indented comments
    # outside a `|2` scalar!) may appear between the start and end markers!
    req_ind = max(1, min_indent)
    end_match = re.search(
        rf"^[ \t]{{{req_ind},}}" + re.escape(END_MARKER) + r"[ \t]*(?:\r?\n|$)",
        tail,
        re.MULTILINE,
    )
    if end_match:
        between = tail[: end_match.start()]
        has_nested_start = MARKER in between
        has_dedented_line = any(
            ln.strip() and (len(ln) - len(ln.lstrip(" \t"))) < req_ind
            for ln in between.splitlines()
        )
        if not has_nested_start and not has_dedented_line:
            return tail[end_match.end() :]
    # Legacy blocks written before END_MARKER existed: consume ONLY exact known managed lines
    # so obsolete managed text is replaced cleanly while any user-added lines (even ones
    # sharing a heading/prefix like `- Source search/read: retain my custom policy`) are preserved.
    lines = tail.splitlines(keepends=True)
    idx = 0
    if idx < len(lines):
        # Remainder of the start-marker line itself
        idx += 1
    while idx < len(lines) and lines[idx].rstrip("\r\n") in _EXACT_LEGACY_YAML_LINES:
        idx += 1
    return "".join(lines[idx:])


def _extract_md_tail_after_managed_block(tail: str) -> str:
    if MD_END_MARKER in tail:
        old, _, rest = tail.partition(MD_END_MARKER)
        if MD_MARKER not in old:
            if rest.startswith("\r\n"):
                rest = rest[2:]
            elif rest.startswith("\n"):
                rest = rest[1:]
            return rest
    # Legacy blocks without MD_END_MARKER: consume ONLY exact known managed lines,
    # stopping before any user-added bullets or custom sections.
    lines = tail.splitlines(keepends=True)
    idx = 0
    if idx < len(lines):
        idx += 1
    while idx < len(lines):
        line = lines[idx]
        if line.strip() == "":
            look = idx + 1
            while look < len(lines) and lines[look].strip() == "":
                look += 1
            if look < len(lines) and lines[look].rstrip("\r\n") in _EXACT_LEGACY_MD_LINES:
                idx = look
                continue
            break
        if line.rstrip("\r\n") in _EXACT_LEGACY_MD_LINES:
            idx += 1
            continue
        break
    return "".join(lines[idx:])


def patch_yaml_agent(path: Path) -> bool:
    if not path.is_file():
        return False
    original = path.read_text(encoding="utf-8")
    updated = original

    # Check top-level `strictToolsList: true` YAML field (supporting unquoted/single/double-quoted
    # keys, optional `!!bool` or full `!<tag:yaml.org,2002:bool>` tag with unquoted or quoted
    # boolean scalar like `!!bool "true"`, and case-insensitive `true`/`True`/`TRUE`!).
    has_strict_tools_list = bool(
        re.search(
            r"""^(?:strictToolsList|"strictToolsList"|'strictToolsList')[ \t]*:[ \t]*(?:(?:!!bool|!<tag:yaml\.org,2002:bool>)[ \t]+(?:true|"true"|'true')|true)(?:[ \t]*(?:#.*)?)?$""",
            updated,
            re.MULTILINE | re.IGNORECASE,
        )
    )
    # Support single-line AND multiline flow-style YAML `tools: [view, lsp]` lists (including
    # inline `# comments` that themselves contain brackets like `# keep [read-only] tools` and
    # escaped quotes/backslashes like `"name\\"`) as well as block-style `tools:\n  - lsp`!
    flow_start_re = re.compile(r"^(tools:[ \t]*)\[", re.MULTILINE)
    flow_start_m = flow_start_re.search(updated)
    if flow_start_m and not has_strict_tools_list:
        scan_i = flow_start_m.end()
        in_sq = False
        in_dq = False
        in_comment = False
        close_idx: int | None = None
        while scan_i < len(updated):
            ch = updated[scan_i]
            if in_comment:
                if ch == "\n":
                    in_comment = False
                scan_i += 1
                continue
            if in_sq:
                if ch == "'":
                    if scan_i + 1 < len(updated) and updated[scan_i + 1] == "'":
                        scan_i += 2
                        continue
                    in_sq = False
                scan_i += 1
                continue
            if in_dq:
                if ch == "\\" and scan_i + 1 < len(updated):
                    scan_i += 2
                    continue
                if ch == '"':
                    in_dq = False
                scan_i += 1
                continue
            if ch == "'":
                in_sq = True
            elif ch == '"':
                in_dq = True
            elif ch == "#" and (scan_i == 0 or updated[scan_i - 1].isspace()):
                in_comment = True
            elif ch == "]":
                close_idx = scan_i
                break
            scan_i += 1
        if close_idx is not None:
            inner_body = updated[flow_start_m.end() : close_idx]
            is_multiline_flow = "\n" in inner_body

            def _tokenize_yaml_flow_items(body: str) -> list[str]:
                items: list[str] = []
                buf: list[str] = []
                c_sq = False
                c_dq = False
                c_comment = False
                idx_c = 0
                while idx_c < len(body):
                    ch_c = body[idx_c]
                    if c_comment:
                        if ch_c == "\n":
                            c_comment = False
                        idx_c += 1
                        continue
                    if c_sq:
                        buf.append(ch_c)
                        if ch_c == "'":
                            if idx_c + 1 < len(body) and body[idx_c + 1] == "'":
                                buf.append("'")
                                idx_c += 2
                                continue
                            c_sq = False
                        idx_c += 1
                        continue
                    if c_dq:
                        buf.append(ch_c)
                        if ch_c == "\\" and idx_c + 1 < len(body):
                            buf.append(body[idx_c + 1])
                            idx_c += 2
                            continue
                        if ch_c == '"':
                            c_dq = False
                        idx_c += 1
                        continue
                    if ch_c == "'":
                        c_sq = True
                        buf.append(ch_c)
                    elif ch_c == '"':
                        c_dq = True
                        buf.append(ch_c)
                    elif ch_c == "#" and (idx_c == 0 or body[idx_c - 1].isspace()):
                        c_comment = True
                    elif ch_c == ",":
                        token = "".join(buf).strip()
                        if token:
                            items.append(token)
                        buf = []
                    else:
                        buf.append(ch_c)
                    idx_c += 1
                token = "".join(buf).strip()
                if token:
                    items.append(token)
                return items

            raw_items = _tokenize_yaml_flow_items(inner_body)
            unquoted_items = {it.strip("'\"") for it in raw_items}
            if "*" not in unquoted_items:
                missing_flow = [t for t in EXPLORE_EXTRA_TOOLS if t not in unquoted_items]
                if missing_flow:
                    use_single = any(it.startswith("'") for it in raw_items)
                    use_double = any(it.startswith('"') for it in raw_items)
                    formatted_new = [
                        f"'{t}'" if use_single else (f'"{t}"' if use_double else t)
                        for t in missing_flow
                    ]
                    merged_items = raw_items + formatted_new
                    if is_multiline_flow:
                        item_indent_m = re.search(r"\n([ \t]+)\S", inner_body)
                        item_indent = item_indent_m.group(1) if item_indent_m else "  "
                        body_rendered = (
                            "\n" + ",\n".join(f"{item_indent}{it}" for it in merged_items) + "\n"
                        )
                        replacement = f"{flow_start_m.group(1)}[{body_rendered}]"
                    else:
                        replacement = f"{flow_start_m.group(1)}[{', '.join(merged_items)}]"
                    updated = (
                        updated[: flow_start_m.start()] + replacement + updated[close_idx + 1 :]
                    )

    has_explicit_tools = (
        bool(re.search(r"^tools:[ \t]*(?:#[^\r\n]*)?\r?$", updated, re.MULTILINE))
        and '  - "*"\n' not in updated
        and '- "*"\n' not in updated
        and not has_strict_tools_list
    )
    if has_explicit_tools:
        lines = updated.splitlines(keepends=True)
        tools_idx = next(
            (
                idx
                for idx, ln in enumerate(lines)
                if re.match(r"^tools:[ \t]*(?:#[^\r\n]*)?$", ln.rstrip("\r\n"))
            ),
            None,
        )
        if tools_idx is not None:
            # Find the extent and existing item indentation of the `tools:` YAML sequence
            # (supporting BOTH indented `  - lsp` AND valid indentless `- lsp` sequences!).
            item_indent: str | None = None
            insert_at = tools_idx + 1
            lsp_insert_at: int | None = None
            existing_tools: set[str] = set()
            while insert_at < len(lines):
                raw_ln = lines[insert_at]
                stripped = raw_ln.strip()
                if not stripped:
                    insert_at += 1
                    continue
                if raw_ln[0] in (" ", "\t"):
                    if stripped.startswith("- "):
                        leading = raw_ln[: len(raw_ln) - len(raw_ln.lstrip(" \t"))]
                        if item_indent is None:
                            item_indent = leading
                        item_val = stripped[2:].strip().strip("'\"")
                        existing_tools.add(item_val)
                        if item_val == "lsp" and lsp_insert_at is None:
                            lsp_insert_at = insert_at + 1
                    insert_at += 1
                    continue
                if stripped.startswith("- "):
                    if item_indent is None:
                        item_indent = ""
                    item_val = stripped[2:].strip().strip("'\"")
                    existing_tools.add(item_val)
                    if item_val == "lsp" and lsp_insert_at is None:
                        lsp_insert_at = insert_at + 1
                    insert_at += 1
                    continue
                if stripped.startswith("#") and (item_indent is None or item_indent == ""):
                    insert_at += 1
                    continue
                break
            indent = "  " if item_indent is None else item_indent
            missing_tools = [t for t in EXPLORE_EXTRA_TOOLS if t not in existing_tools]
            if missing_tools:
                block = f"{indent}# Code Context & Test Context tools (read-only)\n" + "".join(
                    f"{indent}- {t}\n" for t in missing_tools
                )
                if lsp_insert_at is not None:
                    lines.insert(lsp_insert_at, block)
                else:
                    while insert_at > tools_idx + 1 and not lines[insert_at - 1].strip():
                        insert_at -= 1
                    lines.insert(insert_at, block)
                updated = "".join(lines)

    def _reindent_yaml_block(block: str, target_indent: str) -> str:
        if target_indent == "  ":
            return block
        out_lines = []
        for ln in block.splitlines(keepends=True):
            if ln.startswith("  "):
                out_lines.append(target_indent + ln[2:])
            else:
                out_lines.append(ln)
        return "".join(out_lines)

    marker_match = re.search(rf"^(?P<indent>[ \t]+){re.escape(MARKER)}", updated, re.MULTILINE)
    prompt_header_re = re.compile(
        r"^prompt:[ \t]*[|>](?:(?P<chomp1>[-+])?(?P<ind1>[1-9]\d*)?|(?P<ind2>[1-9]\d*)(?P<chomp2>[-+])?)[ \t]*(?:#.*)?$"
    )
    if marker_match:
        existing_indent = marker_match.group("indent")
        head = updated[: marker_match.start()]
        tail = updated[marker_match.end() :]
        after_block = _extract_yaml_tail_after_managed_block(tail, min_indent=len(existing_indent))
        rendered = _reindent_yaml_block(YAML_PROMPT_BLOCK, existing_indent)
        updated = head.rstrip() + "\n\n" + rendered + after_block
    elif any(prompt_header_re.match(ln.rstrip("\r\n")) for ln in updated.splitlines()):
        # Insert the managed block INSIDE the `prompt:` block scalar (`|`, `|-`, `|2-  `, `|4`,
        # `>2+`, etc.) right before the next top-level YAML key, preserving existing body
        # indentation OR honoring the header's explicit indentation indicator for empty blocks!
        lines = updated.splitlines(keepends=True)
        prompt_idx: int | None = None
        header_indent: str | None = None
        for idx, ln in enumerate(lines):
            m = prompt_header_re.match(ln.rstrip("\r\n"))
            if m:
                prompt_idx = idx
                ind_digits = m.group("ind1") or m.group("ind2")
                if ind_digits:
                    header_indent = " " * int(ind_digits)
                break
        if prompt_idx is not None:
            end_idx = prompt_idx + 1
            body_indent: str | None = None
            min_scalar_indent = len(header_indent) if header_indent else 1
            while end_idx < len(lines):
                raw_ln = lines[end_idx]
                if not raw_ln.strip():
                    end_idx += 1
                    continue
                line_indent_len = len(raw_ln) - len(raw_ln.lstrip(" \t"))
                if line_indent_len >= min_scalar_indent:
                    if body_indent is None:
                        body_indent = raw_ln[:line_indent_len]
                        if not header_indent:
                            min_scalar_indent = line_indent_len
                    end_idx += 1
                    continue
                break
            insert_idx = end_idx
            while insert_idx > prompt_idx + 1 and not lines[insert_idx - 1].strip():
                insert_idx -= 1
            prefix = "".join(lines[:insert_idx]).rstrip()
            suffix = "".join(lines[end_idx:])
            effective_indent = body_indent or header_indent or "  "
            rendered = _reindent_yaml_block(YAML_PROMPT_BLOCK, effective_indent)
            updated = prefix + "\n\n" + rendered + suffix

    if updated != original:
        atomic_write(path, updated)
        return True
    return False


def patch_explore_agent_md(path: Path) -> bool:
    if not path.is_file():
        return False
    original = path.read_text(encoding="utf-8")
    updated = original
    # Only inspect and patch the YAML front-matter `tools` list so mentions of tool names inside
    # the Markdown prompt body never prevent missing tools from being added to `tools`!
    fm_match = re.match(r"^---\r?\n(.*?)\r?\n---(?:\r?\n|$)", updated, re.DOTALL)
    if fm_match:
        fm_start, fm_end = fm_match.start(1), fm_match.end(1)
        fm = updated[fm_start:fm_end]
        flow_m = re.search(r"^(tools:[ \t]*)\[([^\]]*)\]", fm, re.MULTILINE)
        if flow_m:
            raw_items = flow_m.group(2)
            use_quotes = "'" in raw_items or '"' in raw_items
            quote_ch = '"' if '"' in raw_items and "'" not in raw_items else "'"
            # Strip inline `# comments` line-by-line before extracting existing tool names so
            # comment text never looks like an item and trailing comments keep their newline!
            no_comment_items = "\n".join(
                re.sub(r"(?:^|[ \t])#.*$", "", ln) for ln in raw_items.splitlines()
            )
            existing_tools = {
                tok.strip().strip("'\"")
                for tok in no_comment_items.split(",")
                if tok.strip().strip("'\"")
            }
            missing = [t for t in MD_EXTRA_TOOLS if t not in existing_tools]
            if missing:
                if "\n" in raw_items:
                    line_indent_m = re.search(r"\n([ \t]+)\S", raw_items)
                    item_indent = line_indent_m.group(1) if line_indent_m else "  "
                    add_lines = "".join(
                        f"{item_indent}{quote_ch}{t}{quote_ch},\n"
                        if use_quotes
                        else f"{item_indent}{t},\n"
                        for t in missing
                    )
                    if raw_items.startswith("\r\n"):
                        new_inner = "\r\n" + add_lines + raw_items[2:]
                    elif raw_items.startswith("\n"):
                        new_inner = "\n" + add_lines + raw_items[1:]
                    else:
                        new_inner = "\n" + add_lines + raw_items
                else:
                    add_str = ", ".join(
                        f"{quote_ch}{t}{quote_ch}" if use_quotes else t for t in missing
                    )
                    inner = raw_items.strip()
                    new_inner = f"{add_str}, {inner}" if inner else add_str
                fm = fm[: flow_m.start()] + f"{flow_m.group(1)}[{new_inner}]" + fm[flow_m.end() :]
                updated = updated[:fm_start] + fm + updated[fm_end:]
        else:
            block_m = re.search(
                r"^tools:[ \t]*(?:#[^\r\n]*)?\r?\n((?:[ \t]*-[ \t]*[^\r\n]+\r?\n?)*)",
                fm,
                re.MULTILINE,
            )
            if block_m:
                block_body = block_m.group(1)
                existing_tools = {
                    m.group(1).strip().strip("'\"")
                    for m in re.finditer(r"^[ \t]*-[ \t]*([^\r\n#]+)", block_body, re.MULTILINE)
                }
                if "*" not in existing_tools:
                    indent_m = re.search(r"^([ \t]*)-[ \t]*", block_body, re.MULTILINE)
                    item_indent = indent_m.group(1) if indent_m else "  "
                    missing = [t for t in MD_EXTRA_TOOLS if t not in existing_tools]
                    if missing:
                        insert_lines = "".join(f"{item_indent}- {t}\n" for t in missing)
                        insert_pos = block_m.start(1)
                        fm = fm[:insert_pos] + insert_lines + fm[insert_pos:]
                        updated = updated[:fm_start] + fm + updated[fm_end:]
    if MD_MARKER in updated:
        head, _sep, tail = updated.partition(MD_MARKER)
        after_block = _extract_md_tail_after_managed_block(tail)
        updated = head.rstrip() + "\n" + MD_PROMPT_BLOCK + after_block
    else:
        updated = updated.rstrip() + "\n" + MD_PROMPT_BLOCK
    if updated != original:
        atomic_write(path, updated)
        return True
    return False


def discover_definition_dirs(home: Path) -> list[Path]:
    patterns = (
        ".vscode-server/bin/*/node_modules/@github/copilot-sdk-*/definitions",
        ".vscode-server/bin/*/extensions/copilot/node_modules/@github/copilot/sdk/definitions",
        ".vscode-server/cli/servers/*/server/node_modules/@github/copilot-sdk-*/definitions",
        ".vscode-server/cli/servers/*/server/extensions/copilot/node_modules/@github/copilot/sdk/definitions",
        ".cache/copilot/pkg/*/*/definitions",
        ".cache/github-copilot-sdk/*/*/definitions",
        ".cache/github-copilot-sdk/*/*/*/definitions",
    )
    found: list[Path] = []
    seen: set[Path] = set()
    for pattern in patterns:
        for candidate in sorted(home.glob(pattern)):
            if candidate.is_dir() and candidate not in seen:
                seen.add(candidate)
                found.append(candidate)
    return found


def discover_explore_md_files(home: Path) -> list[Path]:
    patterns = (
        ".vscode-server/data/User/globalStorage/github.copilot-chat/*-agent/*.agent.md",
        ".config/Code/User/globalStorage/github.copilot-chat/*-agent/*.agent.md",
        ".vscode-server/data/agentPlugins/**/agents/*.agent.md",
    )
    found: list[Path] = []
    seen: set[Path] = set()
    for pattern in patterns:
        for candidate in sorted(home.glob(pattern)):
            if candidate.is_file() and candidate not in seen:
                seen.add(candidate)
                found.append(candidate)
    return found


def configure_all(home: Path, extra_dirs: list[Path] | None = None) -> dict[str, int]:
    dirs = list(extra_dirs or []) + discover_definition_dirs(home)
    patched_yaml = 0
    checked_yaml = 0
    for def_dir in dirs:
        for name in TARGET_YAML_AGENTS:
            target = def_dir / name
            if target.is_file():
                checked_yaml += 1
                if patch_yaml_agent(target):
                    patched_yaml += 1
    patched_md = 0
    checked_md = 0
    for md_path in discover_explore_md_files(home):
        checked_md += 1
        if patch_explore_agent_md(md_path):
            patched_md += 1
    return {
        "definition_dirs": len(dirs),
        "checked_yaml": checked_yaml,
        "patched_yaml": patched_yaml,
        "checked_md": checked_md,
        "patched_md": patched_md,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=Path.home())
    parser.add_argument("--definitions-dir", type=Path, action="append", default=[])
    args = parser.parse_args()
    summary = configure_all(args.home, args.definitions_dir)
    print(
        f"Subagent definitions configured: dirs={summary['definition_dirs']} "
        f"yaml={summary['patched_yaml']}/{summary['checked_yaml']} "
        f"md={summary['patched_md']}/{summary['checked_md']}"
    )


if __name__ == "__main__":
    main()
