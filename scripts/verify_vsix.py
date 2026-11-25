"""Validate the installable VSIX artifact using only the Python standard library."""

import json
from pathlib import Path
import re
import struct
import sys
import xml.etree.ElementTree as ET
import zipfile


def _decode_js_escape(src: str, i: int) -> tuple[str, int]:
    """Decode a JavaScript escape sequence starting at `src[i] == '\\\\'`."""
    n = len(src)
    if i + 1 >= n:
        return "", i + 1
    nxt = src[i + 1]
    if nxt == "\r":
        return "", i + 3 if i + 2 < n and src[i + 2] == "\n" else i + 2
    if nxt in ("\n", "\u2028", "\u2029"):
        return "", i + 2
    simple = {
        "n": "\n",
        "r": "\r",
        "t": "\t",
        "b": "\b",
        "f": "\f",
        "v": "\v",
        "\\": "\\",
        "'": "'",
        '"': '"',
        "`": "`",
        "/": "/",
    }
    if nxt in simple:
        return simple[nxt], i + 2
    # Legacy octal escapes (`\0`..`\377`, e.g. CommonJS `require("\56\57missing")`)
    if nxt in "01234567":
        max_len = 3 if nxt in "0123" else 2
        end_oct = i + 2
        while end_oct < n and (end_oct - (i + 1)) < max_len and src[end_oct] in "01234567":
            end_oct += 1
        return chr(int(src[i + 1 : end_oct], 8)), end_oct
    if nxt == "x" and i + 3 < n:
        hex_str = src[i + 2 : i + 4]
        try:
            return chr(int(hex_str, 16)), i + 4
        except ValueError:
            pass
    if nxt == "u":
        if i + 2 < n and src[i + 2] == "{":
            close = src.find("}", i + 3)
            if close > i + 3:
                try:
                    return chr(int(src[i + 3 : close], 16)), close + 1
                except ValueError:
                    pass
        elif i + 5 < n:
            hex_str = src[i + 2 : i + 6]
            try:
                return chr(int(hex_str, 16)), i + 6
            except ValueError:
                pass
    return nxt, i + 2


def _tokenize_js_inner(
    src: str, start: int = 0, stop_on_rbrace: bool = False
) -> tuple[list[tuple[str, str]], int]:
    """Lexically tokenize JS source, skipping comments/regexes while properly distinguishing
    division (`1 / require(...)`, `x++ / require(...)`) from regexes (including at start of
    input/`${...}` and after `if (...)`/`while (...)`/`for (...)`/`with (...)`), decoding JS
    string escapes, and recursively tokenizing `${...}` template interpolations."""
    tokens: list[tuple[str, str]] = []
    i = start
    n = len(src)
    brace_depth = 0
    bracket_depth = 0
    brace_is_expr: list[bool] = []
    paren_is_stmt_cond: list[bool] = []
    fn_class_expr_stack: list[tuple[int, int, int]] = []
    prev_async_was_expr = False
    prev_tok_kind = "start"
    prev_tok_val = ""
    regex_prefix_punct = set("([{,;:!=?&|^~+-*%<>") | {"=>"}
    expr_brace_prefix_punct = set("([,=?:!&|^~+-*%<>")
    regex_prefix_kw = {
        "return", "case", "throw", "in", "of", "typeof", "void", "delete", "new", "yield", "await",
        "else", "do",
    }
    expr_brace_prefix_kw = {
        "return", "case", "throw", "in", "of", "typeof", "void", "delete", "new", "yield", "await",
    }
    stmt_cond_kw = {"if", "while", "for", "with"}
    while i < n:
        ch = src[i]
        if ch in " \t\r\n":
            i += 1
            continue
        if ch == "/" and i + 1 < n and src[i + 1] == "/":
            i += 2
            while i < n and src[i] != "\n":
                i += 1
            continue
        if ch == "/" and i + 1 < n and src[i + 1] == "*":
            end = src.find("*/", i + 2)
            i = n if end < 0 else end + 2
            continue
        if ch == "/" and (
            prev_tok_kind in ("start", "stmt_cond_rparen", "block_rbrace")
            or (prev_tok_kind == "punct" and prev_tok_val in regex_prefix_punct)
            or (prev_tok_kind == "ident" and prev_tok_val in regex_prefix_kw)
        ):
            i += 1
            in_class = False
            while i < n:
                rc = src[i]
                if rc == "\\" and i + 1 < n:
                    i += 2
                    continue
                if rc == "[":
                    in_class = True
                elif rc == "]":
                    in_class = False
                elif rc == "/" and not in_class:
                    i += 1
                    while i < n and src[i].isalpha():
                        i += 1
                    break
                elif rc == "\n":
                    break
                i += 1
            prev_tok_kind, prev_tok_val = "other", ""
            continue
        if ch in ("'", '"'):
            q = ch
            i += 1
            buf: list[str] = []
            while i < n:
                sc = src[i]
                if sc == "\\":
                    dec, i = _decode_js_escape(src, i)
                    buf.append(dec)
                    continue
                if sc == q:
                    i += 1
                    break
                buf.append(sc)
                i += 1
            tokens.append(("str", "".join(buf)))
            prev_tok_kind, prev_tok_val = "str", ""
            continue
        if ch == "`":
            i += 1
            buf = []
            has_interp = False
            const_ok = True
            tpl_sub_tokens: list[tuple[str, str]] = []
            while i < n:
                sc = src[i]
                if sc == "\\":
                    dec, i = _decode_js_escape(src, i)
                    buf.append(dec)
                    continue
                if sc == "$" and i + 1 < n and src[i + 1] == "{":
                    has_interp = True
                    sub_tokens, i = _tokenize_js_inner(src, i + 2, stop_on_rbrace=True)
                    tpl_sub_tokens.extend(sub_tokens)
                    if const_ok:
                        ev = _eval_const_str_expr(sub_tokens, 0)
                        if ev is not None and ev[1] == len(sub_tokens):
                            buf.append(ev[0])
                        else:
                            const_ok = False
                    continue
                if sc == "`":
                    i += 1
                    break
                buf.append(sc)
                i += 1
            if not has_interp:
                tokens.append(("str", "".join(buf)))
                prev_tok_kind, prev_tok_val = "str", ""
            elif const_ok:
                tokens.append(("str", "".join(buf)))
                prev_tok_kind, prev_tok_val = "str", ""
            else:
                tokens.append(("other", ""))
                tokens.extend(tpl_sub_tokens)
                tokens.append(("other", ""))
                prev_tok_kind, prev_tok_val = "other", ""
            continue
        if ch.isdigit():
            i += 1
            while i < n and (src[i].isalnum() or src[i] in (".", "_")):
                i += 1
            tokens.append(("num", ""))
            prev_tok_kind, prev_tok_val = "num", ""
            continue
        if ch.isalpha() or ch in ("_", "$"):
            start_ident = i
            i += 1
            while i < n and (src[i].isalnum() or src[i] in ("_", "$")):
                i += 1
            ident = src[start_ident:i]
            in_expr_pos = (
                (prev_tok_kind == "punct" and (prev_tok_val in expr_brace_prefix_punct or prev_tok_val == "=>"))
                or (prev_tok_kind == "ident" and prev_tok_val in expr_brace_prefix_kw)
            )
            in_fn_class_expr_pos = in_expr_pos or (
                prev_tok_kind == "ident" and prev_tok_val == "extends"
            )
            if ident == "async":
                prev_async_was_expr = in_fn_class_expr_pos
            else:
                if ident in ("function", "class"):
                    if in_fn_class_expr_pos or (
                        ident == "function"
                        and prev_tok_kind == "ident"
                        and prev_tok_val == "async"
                        and prev_async_was_expr
                    ):
                        fn_class_expr_stack.append(
                            (len(paren_is_stmt_cond), bracket_depth, brace_depth)
                        )
                prev_async_was_expr = False
            tokens.append(("ident", ident))
            prev_tok_kind, prev_tok_val = "ident", ident
            continue
        if ch in ("+", "-") and i + 1 < n and src[i + 1] == ch:
            op = ch * 2
            tokens.append(("punct", op))
            prev_tok_kind, prev_tok_val = "punct", op
            i += 2
            continue
        if ch == "(":
            paren_is_stmt_cond.append(
                prev_tok_kind == "ident" and prev_tok_val in stmt_cond_kw
            )
            tokens.append(("punct", "("))
            prev_tok_kind, prev_tok_val = "punct", "("
            i += 1
            continue
        if ch == ")":
            is_cond = paren_is_stmt_cond.pop() if paren_is_stmt_cond else False
            tokens.append(("punct", ")"))
            prev_tok_kind = "stmt_cond_rparen" if is_cond else "punct"
            prev_tok_val = ")"
            i += 1
            continue
        if ch == "[":
            bracket_depth += 1
            tokens.append(("punct", "["))
            prev_tok_kind, prev_tok_val = "punct", "["
            i += 1
            continue
        if ch == "]":
            bracket_depth = max(0, bracket_depth - 1)
            tokens.append(("punct", "]"))
            prev_tok_kind, prev_tok_val = "punct", "]"
            i += 1
            continue
        if ch == "?" and i + 1 < n and src[i + 1] == "." and not (i + 2 < n and src[i + 2].isdigit()):
            tokens.append(("punct", "?."))
            prev_tok_kind, prev_tok_val = "punct", "?."
            i += 2
            continue
        if ch == "=" and i + 1 < n and src[i + 1] == ">":
            tokens.append(("punct", "=>"))
            prev_tok_kind, prev_tok_val = "punct", "=>"
            i += 2
            continue
        if ch == "{":
            is_fn_class_expr_body = (
                bool(fn_class_expr_stack)
                and fn_class_expr_stack[-1]
                == (len(paren_is_stmt_cond), bracket_depth, brace_depth)
            )
            if is_fn_class_expr_body:
                fn_class_expr_stack.pop()
            is_expr = (
                is_fn_class_expr_body
                or (prev_tok_kind == "punct" and prev_tok_val in expr_brace_prefix_punct)
                or (prev_tok_kind == "ident" and prev_tok_val in expr_brace_prefix_kw)
            )
            brace_is_expr.append(is_expr)
            brace_depth += 1
            tokens.append(("punct", "{"))
            prev_tok_kind, prev_tok_val = "punct", "{"
            i += 1
            continue
        if ch == "}":
            if stop_on_rbrace and brace_depth == 0:
                return tokens, i + 1
            brace_depth = max(0, brace_depth - 1)
            was_expr = brace_is_expr.pop() if brace_is_expr else True
            tokens.append(("punct", "}"))
            prev_tok_kind = "punct" if was_expr else "block_rbrace"
            prev_tok_val = "}"
            i += 1
            continue
        if ch == ";":
            cur_pd = len(paren_is_stmt_cond)
            while (
                fn_class_expr_stack
                and fn_class_expr_stack[-1][2] >= brace_depth
                and fn_class_expr_stack[-1][1] >= bracket_depth
                and fn_class_expr_stack[-1][0] >= cur_pd
            ):
                fn_class_expr_stack.pop()
        tokens.append(("punct", ch))
        prev_tok_kind, prev_tok_val = "punct", ch
        i += 1
    return tokens, i


def _tokenize_js(src: str) -> list[tuple[str, str]]:
    tokens, _ = _tokenize_js_inner(src, 0, stop_on_rbrace=False)
    return tokens


def _eval_const_str_expr(
    tokens: list[tuple[str, str]],
    start_idx: int,
) -> tuple[str, int] | None:
    """Evaluate a constant string expression consisting of string literals, balanced parentheses,
    and `+` concatenations (e.g. `('./sub/' + ('entry' + '.js'))`), returning `(value, next_idx)`."""
    tlen = len(tokens)

    def parse_atom(pos: int) -> tuple[str, int] | None:
        if pos >= tlen:
            return None
        if tokens[pos][0] == "str":
            return tokens[pos][1], pos + 1
        if tokens[pos] == ("punct", "("):
            inner = parse_concat(pos + 1)
            if inner is None:
                return None
            val, nxt = inner
            if nxt < tlen and tokens[nxt] == ("punct", ")"):
                return val, nxt + 1
        return None

    def parse_concat(pos: int) -> tuple[str, int] | None:
        first = parse_atom(pos)
        if first is None:
            return None
        parts = [first[0]]
        cur = first[1]
        while cur < tlen and tokens[cur] == ("punct", "+"):
            nxt_atom = parse_atom(cur + 1)
            if nxt_atom is None:
                return None
            parts.append(nxt_atom[0])
            cur = nxt_atom[1]
        return "".join(parts), cur

    return parse_concat(start_idx)


def _extract_call_str_arg(
    tokens: list[tuple[str, str]],
    lparen_idx: int,
    allow_options: bool,
) -> tuple[str, bool] | None:
    """Given `tokens[lparen_idx] == ('punct', '(')`, evaluate a constant string expression
    (including parenthesized literals and `+` concatenations like `require('./sub/' + 'entry.js')`)
    and return `(spec, has_options_arg)`."""
    tlen = len(tokens)
    if lparen_idx >= tlen or tokens[lparen_idx] != ("punct", "("):
        return None
    parsed = _eval_const_str_expr(tokens, lparen_idx + 1)
    if parsed is None:
        return None
    spec, after = parsed
    if after >= tlen:
        return None
    if tokens[after] == ("punct", ")"):
        return spec, False
    if tokens[after] == ("punct", ","):
        if after + 1 < tlen and tokens[after + 1] == ("punct", ")"):
            return spec, False
        # Extra arguments after the first specifier:
        # - For `require.resolve(spec, options)` (`allow_options=True`), flag `has_options_arg=True`
        #   so callers reject custom lookup `paths` that cannot be statically verified.
        # - For `require(spec, ...)` / `module.require(spec, ...)` (`allow_options=False`), Node
        #   still evaluates and loads `spec` (ignoring extra arguments), so still extract `spec`!
        return spec, allow_options
    return None


_NODE_BUILTIN_MODULES = frozenset(
    {
        "assert",
        "assert/strict",
        "async_hooks",
        "buffer",
        "child_process",
        "cluster",
        "console",
        "constants",
        "crypto",
        "dgram",
        "diagnostics_channel",
        "dns",
        "dns/promises",
        "domain",
        "events",
        "fs",
        "fs/promises",
        "http",
        "http2",
        "https",
        "inspector",
        "inspector/promises",
        "module",
        "net",
        "os",
        "path",
        "path/posix",
        "path/win32",
        "perf_hooks",
        "process",
        "punycode",
        "querystring",
        "readline",
        "readline/promises",
        "repl",
        "stream",
        "stream/consumers",
        "stream/promises",
        "stream/web",
        "string_decoder",
        "sys",
        "timers",
        "timers/promises",
        "tls",
        "trace_events",
        "tty",
        "url",
        "util",
        "util/types",
        "v8",
        "vm",
        "wasi",
        "worker_threads",
        "zlib",
    }
)

_NODE_PREFIX_ONLY_BUILTINS = frozenset(
    {
        "node:sea",
        "node:sqlite",
        "node:test",
        "node:test/reporters",
    }
)


def _is_allowed_host_or_node_module(spec: str) -> bool:
    if spec == "vscode":
        return True
    if spec in _NODE_PREFIX_ONLY_BUILTINS:
        return True
    if spec.startswith("node:"):
        return spec[5:] in _NODE_BUILTIN_MODULES
    return spec in _NODE_BUILTIN_MODULES


_GROUPING_PAREN_KEYWORDS = frozenset(
    {
        "return",
        "throw",
        "yield",
        "await",
        "typeof",
        "void",
        "delete",
        "new",
        "case",
        "in",
        "of",
        "instanceof",
        "else",
        "do",
        "export",
        "default",
    }
)


def _skip_balanced_callee_parens(
    tokens: list[tuple[str, str]],
    callee_start: int,
    after_callee: int,
) -> int:
    """Advance past closing `)` tokens ONLY when each `)` matches a grouping `(` wrapping
    `callee` (such as `(require)('./a')`, `((0, require))('./a')`, or `(require).resolve('./a')`),
    and NOT a function-call argument list like `handler(require)('message')`!"""
    idx = after_callee
    left = callee_start - 1
    tlen = len(tokens)
    while idx < tlen and tokens[idx] == ("punct", ")"):
        # Walk backwards from `left` to find the matching `(` for this `)`.
        depth = 0
        scan = left
        open_idx: int | None = None
        while scan >= 0:
            tok = tokens[scan]
            if tok == ("punct", ")"):
                depth += 1
            elif tok == ("punct", "("):
                if depth == 0:
                    open_idx = scan
                    break
                depth -= 1
            elif tok in (("punct", ";"), ("punct", "{"), ("punct", "}")) and depth == 0:
                break
            scan -= 1
        if open_idx is None:
            break
        # Determine whether `tokens[open_idx]` is a grouping paren or a call paren (`fn(...)`).
        if open_idx > 0:
            prev_kind, prev_val = tokens[open_idx - 1]
            # Check if the matching `(` is preceded by a call/member expression (e.g. `handler(require)('msg')`)
            # vs a control-flow statement header (e.g. `if (true) (require)('./missing')`).
            is_control_header_rparen = False
            if prev_kind == "punct" and prev_val == ")":
                # Walk backwards from `open_idx - 1` to find the `(` matching that `)`.
                p_depth = 0
                p_scan = open_idx - 2
                p_open: int | None = None
                while p_scan >= 0:
                    pt = tokens[p_scan]
                    if pt == ("punct", ")"):
                        p_depth += 1
                    elif pt == ("punct", "("):
                        if p_depth == 0:
                            p_open = p_scan
                            break
                        p_depth -= 1
                    p_scan -= 1
                if p_open is not None and p_open > 0:
                    hk, hv = tokens[p_open - 1]
                    if hk == "ident" and hv in ("if", "while", "for", "with", "switch", "catch"):
                        is_control_header_rparen = True
                    elif (
                        hk == "ident"
                        and hv == "await"
                        and p_open >= 2
                        and tokens[p_open - 2] == ("ident", "for")
                    ):
                        is_control_header_rparen = True
            is_call_paren = (
                (prev_kind == "ident" and prev_val not in _GROUPING_PAREN_KEYWORDS)
                or (
                    prev_kind == "punct"
                    and (
                        prev_val in ("]", "?.")
                        or (prev_val == ")" and not is_control_header_rparen)
                    )
                )
                or prev_kind in ("str", "num")
            )
            if is_call_paren:
                break
        idx += 1
        left = open_idx - 1
    return idx


def _record_extracted_spec(
    specs: list[tuple[str, bool]],
    spec: str,
    is_esm: bool,
    allow_win_rel: bool,
) -> None:
    is_rel = (
        spec.startswith(("./", "../", ".\\", "..\\"))
        if allow_win_rel
        else spec.startswith(("./", "../"))
    )
    if is_rel:
        specs.append((spec, is_esm))
        return
    if _is_allowed_host_or_node_module(spec):
        return
    raise ValueError(
        f"Unbundled or unsupported runtime dependency '{spec}' (VSIX packages forbid node_modules; only relative modules, 'vscode', and Node built-ins are allowed)"
    )


def _extract_js_relative_imports_with_kind(src: str) -> list[tuple[str, bool]]:
    """Extract relative `(specifier, is_esm)` pairs for CommonJS `require(...)` /
    `module.require(...)` / `module["require"](...)` / `require.resolve(...)` (including
    optional calls, parenthesized literals, constant `+` concatenation, and constant template
    interpolations), dynamic `import(...)`, `from '...'`, and side-effect `import '...'` from
    tokenized JS source. Rejects unbundled bare packages and non-constant dynamic imports."""
    tokens = _tokenize_js(src)
    specs: list[tuple[str, bool]] = []
    tlen = len(tokens)
    # Track simple aliases of `require` / `module.require` / `require.resolve`
    # (e.g. `const r = require; r('./missing')` or `const res = require.resolve; res('./a')`).
    require_aliases: set[str] = set()
    resolve_aliases: set[str] = set()
    for i in range(tlen - 2):
        if tokens[i][0] == "ident" and tokens[i + 1] == ("punct", "="):
            alias_name = tokens[i][1]
            rhs = i + 2
            while rhs < tlen and tokens[rhs] == ("punct", "("):
                rhs += 1
            if rhs < tlen and tokens[rhs] == ("ident", "require"):
                after_r = _skip_balanced_callee_parens(tokens, rhs, rhs + 1)
                if (
                    after_r + 1 < tlen
                    and tokens[after_r] in (("punct", "."), ("punct", "?."))
                    and tokens[after_r + 1] == ("ident", "resolve")
                ):
                    resolve_aliases.add(alias_name)
                elif after_r >= tlen or tokens[after_r] not in (("punct", "("), ("punct", "."), ("punct", "[")):
                    require_aliases.add(alias_name)
            elif (
                rhs + 2 < tlen
                and tokens[rhs] == ("ident", "module")
                and tokens[rhs + 1] in (("punct", "."), ("punct", "?."))
                and tokens[rhs + 2] == ("ident", "require")
            ):
                after_r = _skip_balanced_callee_parens(tokens, rhs, rhs + 3)
                if (
                    after_r + 1 < tlen
                    and tokens[after_r] in (("punct", "."), ("punct", "?."))
                    and tokens[after_r + 1] == ("ident", "resolve")
                ):
                    resolve_aliases.add(alias_name)
                elif after_r >= tlen or tokens[after_r] not in (("punct", "("), ("punct", "."), ("punct", "[")):
                    require_aliases.add(alias_name)
    for idx, (kind, val) in enumerate(tokens):
        if kind != "ident":
            continue
        if val in resolve_aliases:
            prev_is_dot = idx > 0 and tokens[idx - 1] in (("punct", "."), ("punct", "?."))
            prev_is_decl = idx > 0 and tokens[idx - 1] in (("ident", "function"), ("ident", "class"))
            after_al = _skip_balanced_callee_parens(tokens, idx, idx + 1)
            lp_al = after_al + 1 if after_al < tlen and tokens[after_al] == ("punct", "?.") else after_al
            if not prev_is_dot and not prev_is_decl and lp_al < tlen and tokens[lp_al] == ("punct", "("):
                parsed_res = _extract_call_str_arg(tokens, lp_al, allow_options=True)
                if parsed_res is None:
                    raise ValueError(
                        f"Dynamic non-constant {val}(...) call (require.resolve alias) cannot be statically verified"
                    )
                spec, has_options_arg = parsed_res
                if has_options_arg:
                    raise ValueError(
                        f"Unsupported options argument in {val}('{spec}', ...): custom lookup paths cannot be statically verified"
                    )
                _record_extracted_spec(specs, spec, is_esm=False, allow_win_rel=True)
                continue
        if val in require_aliases:
            prev_is_dot = idx > 0 and tokens[idx - 1] in (("punct", "."), ("punct", "?."))
            prev_is_decl = idx > 0 and tokens[idx - 1] in (("ident", "function"), ("ident", "class"))
            after_al = _skip_balanced_callee_parens(tokens, idx, idx + 1)
            lp_al = after_al + 1 if after_al < tlen and tokens[after_al] == ("punct", "?.") else after_al
            if not prev_is_dot and not prev_is_decl and lp_al < tlen and tokens[lp_al] == ("punct", "("):
                parsed_call = _extract_call_str_arg(tokens, lp_al, allow_options=False)
                if parsed_call is None:
                    raise ValueError(
                        f"Dynamic non-constant {val}(...) call (require alias) cannot be statically verified"
                    )
                spec, _ = parsed_call
                _record_extracted_spec(specs, spec, is_esm=False, allow_win_rel=True)
                continue
        if val == "module":
            prev_is_dot = idx > 0 and tokens[idx - 1] in (("punct", "."), ("punct", "?."))
            if not prev_is_dot:
                after_req: int | None = None
                if (
                    idx + 3 < tlen
                    and tokens[idx + 1] == ("punct", "[")
                    and tokens[idx + 2] == ("str", "require")
                    and tokens[idx + 3] == ("punct", "]")
                ):
                    after_req = idx + 4
                elif (
                    idx + 4 < tlen
                    and tokens[idx + 1] == ("punct", "?.")
                    and tokens[idx + 2] == ("punct", "[")
                    and tokens[idx + 3] == ("str", "require")
                    and tokens[idx + 4] == ("punct", "]")
                ):
                    after_req = idx + 5
                if after_req is not None:
                    after_req = _skip_balanced_callee_parens(tokens, idx, after_req)
                    lp_idx = (
                        after_req + 1
                        if after_req < tlen and tokens[after_req] == ("punct", "?.")
                        else after_req
                    )
                    if lp_idx < tlen and tokens[lp_idx] == ("punct", "("):
                        parsed_call = _extract_call_str_arg(tokens, lp_idx, allow_options=False)
                        if parsed_call is None:
                            raise ValueError(
                                "Dynamic non-constant module['require'](...) call cannot be statically verified"
                            )
                        spec, _ = parsed_call
                        _record_extracted_spec(specs, spec, is_esm=False, allow_win_rel=True)
                        continue
                    res_call: int | None = None
                    if (
                        after_req + 1 < tlen
                        and tokens[after_req] in (("punct", "."), ("punct", "?."))
                        and tokens[after_req + 1] == ("ident", "resolve")
                    ):
                        res_call = after_req + 2
                    elif (
                        after_req + 2 < tlen
                        and tokens[after_req] == ("punct", "[")
                        and tokens[after_req + 1] == ("str", "resolve")
                        and tokens[after_req + 2] == ("punct", "]")
                    ):
                        res_call = after_req + 3
                    elif (
                        after_req + 3 < tlen
                        and tokens[after_req] == ("punct", "?.")
                        and tokens[after_req + 1] == ("punct", "[")
                        and tokens[after_req + 2] == ("str", "resolve")
                        and tokens[after_req + 3] == ("punct", "]")
                    ):
                        res_call = after_req + 4
                    if res_call is not None:
                        res_call = _skip_balanced_callee_parens(tokens, idx, res_call)
                        if res_call < tlen and tokens[res_call] == ("punct", "?."):
                            res_call += 1
                        if res_call < tlen and tokens[res_call] == ("punct", "("):
                            parsed_res = _extract_call_str_arg(tokens, res_call, allow_options=True)
                            if parsed_res is None:
                                raise ValueError(
                                    "Dynamic non-constant module['require'].resolve(...) call cannot be statically verified"
                                )
                            spec, has_options_arg = parsed_res
                            if has_options_arg:
                                raise ValueError(
                                    f"Unsupported options argument in require.resolve('{spec}', ...): custom lookup paths cannot be statically verified"
                                )
                            _record_extracted_spec(specs, spec, is_esm=False, allow_win_rel=True)
                            continue
        if val == "require":
            prev_is_dot = idx > 0 and tokens[idx - 1] in (("punct", "."), ("punct", "?."))
            is_module_require = (
                idx >= 2
                and tokens[idx - 1] in (("punct", "."), ("punct", "?."))
                and tokens[idx - 2] == ("ident", "module")
                and not (idx >= 3 and tokens[idx - 3] in (("punct", "."), ("punct", "?.")))
            )
            if not prev_is_dot or is_module_require:
                callee_start = idx - 2 if is_module_require else idx
                after_req_ident = _skip_balanced_callee_parens(tokens, callee_start, idx + 1)
                call_idx: int | None = None
                if (
                    after_req_ident + 1 < tlen
                    and tokens[after_req_ident] in (("punct", "."), ("punct", "?."))
                    and tokens[after_req_ident + 1] == ("ident", "resolve")
                ):
                    call_idx = after_req_ident + 2
                elif (
                    after_req_ident + 2 < tlen
                    and tokens[after_req_ident] == ("punct", "[")
                    and tokens[after_req_ident + 1] == ("str", "resolve")
                    and tokens[after_req_ident + 2] == ("punct", "]")
                ):
                    call_idx = after_req_ident + 3
                elif (
                    after_req_ident + 3 < tlen
                    and tokens[after_req_ident] == ("punct", "?.")
                    and tokens[after_req_ident + 1] == ("punct", "[")
                    and tokens[after_req_ident + 2] == ("str", "resolve")
                    and tokens[after_req_ident + 3] == ("punct", "]")
                ):
                    call_idx = after_req_ident + 4
                if call_idx is not None:
                    call_idx = _skip_balanced_callee_parens(tokens, callee_start, call_idx)
                    if call_idx < tlen and tokens[call_idx] == ("punct", "?."):
                        call_idx += 1
                    if call_idx < tlen and tokens[call_idx] == ("punct", "("):
                        parsed_res = _extract_call_str_arg(tokens, call_idx, allow_options=True)
                        if parsed_res is None:
                            raise ValueError(
                                "Dynamic non-constant require.resolve(...) call cannot be statically verified"
                            )
                        spec, has_options_arg = parsed_res
                        if has_options_arg:
                            raise ValueError(
                                f"Unsupported options argument in require.resolve('{spec}', ...): custom lookup paths cannot be statically verified"
                            )
                        _record_extracted_spec(specs, spec, is_esm=False, allow_win_rel=True)
                        continue
        if val in ("require", "import") and idx + 1 < tlen:
            prev_is_dot = idx > 0 and tokens[idx - 1] in (("punct", "."), ("punct", "?."))
            # Skip `function require(...)` or `method require(...)` declarations
            prev_is_decl = idx > 0 and tokens[idx - 1] in (("ident", "function"), ("ident", "class"))
            is_module_require = (
                val == "require"
                and idx >= 2
                and tokens[idx - 1] in (("punct", "."), ("punct", "?."))
                and tokens[idx - 2] == ("ident", "module")
                and not (idx >= 3 and tokens[idx - 3] in (("punct", "."), ("punct", "?.")))
            )
            callee_start = idx - 2 if is_module_require else idx
            after_ident = (
                _skip_balanced_callee_parens(tokens, callee_start, idx + 1)
                if val == "require"
                else idx + 1
            )
            lparen_idx = (
                after_ident + 1
                if val == "require"
                and after_ident < tlen
                and tokens[after_ident] == ("punct", "?.")
                else after_ident
            )
            if (not prev_is_dot or is_module_require) and not prev_is_decl and lparen_idx < tlen and tokens[lparen_idx] == ("punct", "("):
                parsed_call = _extract_call_str_arg(
                    tokens, lparen_idx, allow_options=(val == "import")
                )
                if parsed_call is None:
                    raise ValueError(
                        f"Dynamic non-constant {val}(...) call cannot be statically verified"
                    )
                spec, _ = parsed_call
                _record_extracted_spec(
                    specs, spec, is_esm=(val == "import"), allow_win_rel=(val != "import")
                )
                continue
        if val in ("from", "import") and idx + 1 < tlen and tokens[idx + 1][0] == "str":
            spec = tokens[idx + 1][1]
            _record_extracted_spec(specs, spec, is_esm=True, allow_win_rel=False)
    return specs


def _extract_js_relative_imports(src: str) -> list[str]:
    return [spec for spec, _ in _extract_js_relative_imports_with_kind(src)]


def _normalize_extension_relpath(base_dir: Path, spec: str, cur_js: str) -> str:
    norm_target = (base_dir / spec.replace("\\", "/")).as_posix()
    resolved_parts: list[str] = []
    for p in norm_target.split("/"):
        if p in ("", "."):
            continue
        if p == "..":
            if not resolved_parts:
                raise ValueError(
                    f"Runtime JS dependency '{spec}' in {cur_js} escapes extension/ root"
                )
            resolved_parts.pop()
        else:
            resolved_parts.append(p)
    if not resolved_parts or resolved_parts[0] != "extension":
        raise ValueError(
            f"Runtime JS dependency '{spec}' in {cur_js} resolves outside extension/: {'/'.join(resolved_parts)}"
        )
    return "/".join(resolved_parts)


def _resolve_node_module(
    archive: zipfile.ZipFile,
    names: set[str],
    base_entry: str,
    cur_js: str,
    spec: str,
    is_esm: bool = False,
    force_dir: bool = False,
) -> tuple[str | None, list[str]]:
    """Resolve a local Node module: Node ESM requires explicit file extensions (no implicit
    `.js` or directory index resolution), while CommonJS follows LOAD_AS_FILE -> LOAD_AS_DIRECTORY
    (`package.json` `main` before `index.*`). Trailing slashes (`require('./dir/')`) skip
    LOAD_AS_FILE and resolve ONLY as a directory (matching Node's `MODULE_NOT_FOUND` behavior!)."""
    if is_esm:
        return (base_entry if base_entry in names else None), [base_entry]
    tried: list[str] = []
    if not force_dir:
        file_cands = (
            [base_entry]
            if base_entry.endswith((".js", ".json", ".node", ".cjs", ".mjs"))
            else [
                base_entry,
                f"{base_entry}.js",
                f"{base_entry}.json",
                f"{base_entry}.node",
            ]
        )
        for c in file_cands:
            tried.append(c)
            if c in names:
                return c, tried
    # LOAD_AS_DIRECTORY: check `<dir>/package.json`'s `main` field FIRST before `index.*`
    pkg_json_entry = f"{base_entry}/package.json"
    if pkg_json_entry in names:
        try:
            sub_pkg = json.loads(archive.read(pkg_json_entry))
        except Exception as exc:
            raise ValueError(f"Invalid package.json at {pkg_json_entry}: {exc}") from exc
        raw_sub_main = sub_pkg.get("main") if isinstance(sub_pkg, dict) else None
        if isinstance(raw_sub_main, str) and raw_sub_main.strip():
            sub_main_clean = raw_sub_main.strip().replace("\\", "/")
            if sub_main_clean.startswith("/") or re.match(r"^[A-Za-z]:", sub_main_clean):
                raise ValueError(
                    f"Unsafe package.json main '{raw_sub_main}' in {pkg_json_entry}"
                )
            sub_target = _normalize_extension_relpath(Path(base_entry), sub_main_clean, pkg_json_entry)
            sub_cands = (
                [sub_target]
                if sub_target.endswith((".js", ".json", ".node", ".cjs", ".mjs"))
                else [
                    sub_target,
                    f"{sub_target}.js",
                    f"{sub_target}.json",
                    f"{sub_target}.node",
                    f"{sub_target}/index.js",
                    f"{sub_target}/index.json",
                    f"{sub_target}/index.node",
                ]
            )
            for c in sub_cands:
                tried.append(c)
                if c in names:
                    return c, tried
    index_cands = [
        f"{base_entry}/index.js",
        f"{base_entry}/index.json",
        f"{base_entry}/index.node",
    ]
    for c in index_cands:
        tried.append(c)
        if c in names:
            return c, tried
    return None, tried


def verify(path: Path) -> dict:
    with zipfile.ZipFile(path) as archive:
        names = set(archive.namelist())
        for name in names:
            parts = Path(name).parts
            if name.startswith("/") or ".." in parts:
                raise ValueError(f"Unsafe package path: {name}")
            if any(part in {"node_modules", "test", "tests", ".git", "__pycache__"} for part in parts):
                raise ValueError(f"Development-only package entry: {name}")
            if name.endswith((".ts", ".map", ".svg", ".pyc")):
                raise ValueError(f"Unexpected source/build entry: {name}")
        required = {
            "extension/package.json",
            "extension/readme.md",
            "extension/LICENSE.txt",
            "extension/changelog.md",
            "extension/docs/PUBLISHING.md",
            "extension/resources/icon.png",
            "extension/resources/banner.png",
            "extension/resources/benchmark.png",
            "extension/out/src/extension.js",
            "extension/prompts/test-context.instructions.md",
        }
        missing = required - names
        if missing:
            raise ValueError(f"Missing entries: {sorted(missing)}")
        manifest = json.loads(archive.read("extension/package.json"))
        # Resolve and validate `manifest["main"]` BEFORE stripping `./` so escaping paths like
        # `../out/src/extension.js` or `/out/src/extension.js` are rejected immediately, then
        # traverse the complete local JS runtime dependency closure using lexical JS scanning!
        raw_main = manifest.get("main")
        if not isinstance(raw_main, str) or not raw_main.strip():
            raise ValueError("Missing or invalid 'main' entry in extension/package.json")
        main_raw_norm = raw_main.strip().replace("\\", "/")
        if (
            main_raw_norm.startswith("/")
            or re.match(r"^[A-Za-z]:", main_raw_norm)
            or ".." in Path(main_raw_norm).parts
        ):
            raise ValueError(f"Unsafe manifest main path: {raw_main}")
        main_rel = re.sub(r"^(?:\./)+", "", main_raw_norm)
        main_parts = [p for p in ("extension/" + main_rel).split("/") if p not in ("", ".")]
        main_base = "/".join(main_parts)
        main_force_dir = main_raw_norm.endswith("/")
        main_entry, _ = _resolve_node_module(
            archive,
            names,
            main_base,
            "extension/package.json",
            raw_main,
            is_esm=False,
            force_dir=main_force_dir,
        )
        if main_entry is None:
            raise ValueError(f"Missing manifest main entrypoint in VSIX: {raw_main}")
        visited_js: set[str] = set()
        js_queue: list[str] = [main_entry]
        while js_queue:
            cur_js = js_queue.pop()
            if cur_js in visited_js:
                continue
            if cur_js not in names:
                raise ValueError(f"Missing runtime JS module required by extension: {cur_js}")
            visited_js.add(cur_js)
            src_text = archive.read(cur_js).decode("utf-8", errors="replace")
            cur_dir = Path(cur_js).parent
            for spec, is_esm in _extract_js_relative_imports_with_kind(src_text):
                base_entry = _normalize_extension_relpath(cur_dir, spec, cur_js)
                spec_force_dir = spec.replace("\\", "/").endswith("/")
                matched, tried = _resolve_node_module(
                    archive,
                    names,
                    base_entry,
                    cur_js,
                    spec,
                    is_esm=is_esm,
                    force_dir=spec_force_dir,
                )
                if matched is None:
                    raise ValueError(
                        f"Missing runtime JS dependency '{spec}' imported by {cur_js} (tried {tried})"
                    )
                if not matched.endswith((".json", ".node", ".wasm")) and matched not in visited_js:
                    js_queue.append(matched)
        if manifest.get("enabledApiProposals") or manifest["contributes"].get("languageModelToolSets"):
            raise ValueError("Distributable must use stable tool contributions only")
        expected = {
            "tctxNativeRun",
            "tctxNativeAffected",
            "tctxNativeGraph",
            "tctxNativeStatus",
            "tctxNativeSync",
            "tctxNativeInvalidate",
            "tctxNativeWorkspaces",
        }
        tools = manifest["contributes"]["languageModelTools"]
        if len(tools) != 7 or {tool["toolReferenceName"] for tool in tools} != expected:
            raise ValueError("Native tool references changed")
        if manifest["publisher"] != "dotnetpower" or manifest["name"] != "test-context-native":
            raise ValueError("Extension identity changed")
        if manifest["displayName"] != "Test Context by dotnetpower":
            raise ValueError("Use the publisher-qualified Marketplace display name")
        if manifest["repository"]["url"] != "https://github.com/dotnetpower/test-context.git":
            raise ValueError("Unexpected source repository")
        vsix = ET.fromstring(archive.read("extension.vsixmanifest"))
        identity = next(element for element in vsix.iter() if element.tag.endswith("}Identity"))
        if any(
            identity.attrib[key] != manifest[field]
            for key, field in (("Publisher", "publisher"), ("Id", "name"), ("Version", "version"))
        ):
            raise ValueError("VSIX identity differs from the publisher manifest")
        display_name = next(element.text for element in vsix.iter() if element.tag.endswith("}DisplayName"))
        if display_name != manifest["displayName"]:
            raise ValueError("VSIX display name differs from package metadata")
        for name in names:
            if name.lower().endswith(".md"):
                text = archive.read(name).decode("utf-8")
                if re.search(r"(?:/home/|/Users/|[A-Z]:\\Users\\)[^<\s/\\]+", text):
                    raise ValueError(f"Machine-specific documentation in package: {name}")
        for walkthrough in manifest["contributes"]["walkthroughs"]:
            for step in walkthrough["steps"]:
                media = step.get("media", {}).get("markdown")
                if not media or not media.startswith("docs/walkthrough/") or f"extension/{media}" not in names:
                    raise ValueError(f"Missing packaged walkthrough media: {step['id']}")
        for name, expected_size in [("icon", (256, 256)), ("banner", (1440, 480))]:
            image = archive.read(f"extension/resources/{name}.png")
            if image[:8] != b"\x89PNG\r\n\x1a\n" or struct.unpack(">II", image[16:24]) != expected_size:
                raise ValueError(f"Invalid {name} dimensions")
        chart = archive.read("extension/resources/benchmark.png")
        if chart[:8] != b"\x89PNG\r\n\x1a\n" or min(struct.unpack(">II", chart[16:24])) < 600:
            raise ValueError("Invalid or undersized benchmark figure")
        return {
            "artifact": path.name,
            "publisher": manifest["publisher"],
            "name": manifest["name"],
            "displayName": manifest["displayName"],
            "version": manifest["version"],
            "entries": len(names),
            "native_tools": len(tools),
            "stable_api_only": True,
            "bytes": path.stat().st_size,
        }


if __name__ == "__main__":
    print(json.dumps(verify(Path(sys.argv[1])), indent=2))
