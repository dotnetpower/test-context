use super::*;

/// Decodes standard TOML basic-string escape sequences (`\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`,
/// `\uXXXX`, `\UXXXXXXXX`) so escaped target/feature names like `"f\u006f\u006f"` match `"foo"`.
pub(super) fn decode_toml_basic_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' && i + 1 < len {
            match bytes[i + 1] {
                b'"' => {
                    out.push('"');
                    i += 2;
                    continue;
                }
                b'\\' => {
                    out.push('\\');
                    i += 2;
                    continue;
                }
                b'b' => {
                    out.push('\x08');
                    i += 2;
                    continue;
                }
                b'f' => {
                    out.push('\x0c');
                    i += 2;
                    continue;
                }
                b'n' => {
                    out.push('\n');
                    i += 2;
                    continue;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                    continue;
                }
                b'u' if i + 5 < len
                    && bytes[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit) =>
                {
                    if let Some(ch) = u32::from_str_radix(&raw[i + 2..i + 6], 16)
                        .ok()
                        .and_then(char::from_u32)
                    {
                        out.push(ch);
                        i += 6;
                        continue;
                    }
                }
                b'U' if i + 9 < len
                    && bytes[i + 2..i + 10].iter().all(u8::is_ascii_hexdigit) =>
                {
                    if let Some(ch) = u32::from_str_radix(&raw[i + 2..i + 10], 16)
                        .ok()
                        .and_then(char::from_u32)
                    {
                        out.push(ch);
                        i += 10;
                        continue;
                    }
                }
                b' ' | b'\t' | b'\r' | b'\n' => {
                    // TOML 1.0 line-ending backslash inside multiline basic strings: `\` followed
                    // by optional spaces/tabs and a newline trims ALL subsequent whitespace and
                    // newlines up to the next non-whitespace character!
                    let mut j = i + 1;
                    while j < len && matches!(bytes[j], b' ' | b'\t') {
                        j += 1;
                    }
                    if j < len && matches!(bytes[j], b'\r' | b'\n') {
                        while j < len && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
                            j += 1;
                        }
                        i = j;
                        continue;
                    }
                }
                _ => {}
            }
        }
        let ch = raw[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Extracts quoted TOML string elements from `raw_slice` (stripping `#` comments outside quotes and
/// decoding TOML escapes in double-quoted basic strings).
pub(super) fn parse_toml_string_array_items(raw_slice: &str, dest: &mut Vec<String>) {
    let slice = strip_toml_comment_outside_quotes(raw_slice);
    let bytes = slice.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            let q = b;
            let start = i + 1;
            i = start;
            while i < len {
                if q == b'"' && bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                    continue;
                }
                if bytes[i] == q {
                    let inner = &slice[start..i];
                    let decoded = if q == b'"' {
                        decode_toml_basic_string(inner)
                    } else {
                        inner.to_string()
                    };
                    let val = decoded.trim();
                    if !val.is_empty() {
                        dest.push(val.to_string());
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
}

pub(super) fn unquote_toml_scalar(raw: &str) -> Option<String> {
    let val = strip_toml_comment_outside_quotes(raw).trim();
    if val.len() >= 6 && val.starts_with("\"\"\"") && val.ends_with("\"\"\"") {
        let mut inner = &val[3..val.len() - 3];
        if let Some(rest) = inner.strip_prefix("\r\n").or_else(|| inner.strip_prefix('\n')) {
            inner = rest;
        }
        Some(decode_toml_basic_string(inner))
    } else if val.len() >= 6 && val.starts_with("'''") && val.ends_with("'''") {
        let mut inner = &val[3..val.len() - 3];
        if let Some(rest) = inner.strip_prefix("\r\n").or_else(|| inner.strip_prefix('\n')) {
            inner = rest;
        }
        Some(inner.to_string())
    } else if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
        Some(decode_toml_basic_string(&val[1..val.len() - 1]))
    } else if val.len() >= 2 && val.starts_with('\'') && val.ends_with('\'') {
        Some(val[1..val.len() - 1].to_string())
    } else {
        None
    }
}

/// Splits a TOML `key = value` line at the FIRST `=` outside single/double quotes so dotted keys
/// containing quoted `=` (e.g. `target.'cfg(target_os = "linux")'.dependencies.b = { ... }`) are
/// never split inside the quoted segment!
pub(super) fn split_toml_kv_outside_quotes(line: &str) -> Option<(&str, &str)> {
    let mut in_dq = false;
    let mut in_sq = false;
    let mut esc = false;
    for (idx, ch) in line.char_indices() {
        if in_dq {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_dq = false;
            }
            continue;
        }
        if in_sq {
            if ch == '\'' {
                in_sq = false;
            }
            continue;
        }
        match ch {
            '"' => in_dq = true,
            '\'' => in_sq = true,
            '=' => return Some((&line[..idx], &line[idx + 1..])),
            _ => {}
        }
    }
    None
}

/// Splits a TOML dotted key (`features . "f\u006f\u006f"`, `dependencies.b.features`) outside quotes
/// into decoded unquoted key segments (decoding `\uXXXX` / `\UXXXXXXXX` TOML escapes in basic strings!).
pub(super) fn split_toml_dotted_key(raw: &str) -> Vec<String> {
    let mut segs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_dq = false;
    let mut in_sq = false;
    let mut esc = false;
    let mut cur_was_dq = false;
    for ch in raw.trim().chars() {
        if in_dq {
            if esc {
                cur.push('\\');
                cur.push(ch);
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_dq = false;
            } else {
                cur.push(ch);
            }
            continue;
        }
        if in_sq {
            if ch == '\'' {
                in_sq = false;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                in_dq = true;
                cur_was_dq = true;
            }
            '\'' => in_sq = true,
            '.' => {
                let decoded = if cur_was_dq {
                    decode_toml_basic_string(&cur)
                } else {
                    cur.trim().to_string()
                };
                if !decoded.is_empty() {
                    segs.push(decoded);
                }
                cur.clear();
                cur_was_dq = false;
            }
            c if c.is_whitespace() => {}
            _ => cur.push(ch),
        }
    }
    let last = if cur_was_dq {
        decode_toml_basic_string(&cur)
    } else {
        cur.trim().to_string()
    };
    if !last.is_empty() {
        segs.push(last);
    }
    segs
}

/// Normalizes a TOML table or array-of-tables header line (already comment-stripped & trimmed)
/// such as `[ features ]`, `[ "features" ]`, `[[ test ]]`, `[ workspace . dependencies ]`, or
/// `[ target . 'cfg(unix)' . dependencies ]` into a canonical bracketed path (`[features]`,
/// `[[test]]`, `[workspace.dependencies]`, `[target.cfg(unix).dependencies]`).
pub(super) fn normalize_toml_table_header(line: &str) -> Option<String> {
    let t = line.trim();
    let (is_array, inner) = if let Some(s) = t.strip_prefix("[[").and_then(|s| s.strip_suffix("]]"))
    {
        (true, s.trim())
    } else {
        let s = t.strip_prefix('[').and_then(|s| s.strip_suffix(']'))?;
        (false, s.trim())
    };
    if inner.is_empty() {
        return None;
    }
    let mut segs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_dq = false;
    let mut in_sq = false;
    let mut esc = false;
    let mut cur_was_dq = false;
    for ch in inner.chars() {
        if in_dq {
            if esc {
                cur.push('\\');
                cur.push(ch);
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_dq = false;
            } else {
                cur.push(ch);
            }
            continue;
        }
        if in_sq {
            if ch == '\'' {
                in_sq = false;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                in_dq = true;
                cur_was_dq = true;
            }
            '\'' => in_sq = true,
            '.' => {
                let s = if cur_was_dq {
                    decode_toml_basic_string(&cur)
                } else {
                    cur.trim().to_string()
                };
                if s.is_empty() {
                    return None;
                }
                segs.push(s);
                cur.clear();
                cur_was_dq = false;
            }
            c if c.is_whitespace() => {}
            _ => cur.push(ch),
        }
    }
    if in_dq || in_sq {
        return None;
    }
    let last = if cur_was_dq {
        decode_toml_basic_string(&cur)
    } else {
        cur.trim().to_string()
    };
    if last.is_empty() {
        return None;
    }
    segs.push(last);
    let joined = segs.join(".");
    if is_array {
        Some(format!("[[{joined}]]"))
    } else {
        Some(format!("[{joined}]"))
    }
}

/// Splits top-level comma-separated items inside a TOML inline `{ ... }` table or `[ ... ]` array,
/// respecting quotes and nested `{}` / `[]`.
pub(super) fn split_toml_top_level_commas(body: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0usize;
    let mut in_dq = false;
    let mut in_sq = false;
    let mut esc = false;
    let mut brack_d = 0usize;
    let mut brace_d = 0usize;
    for (idx, ch) in body.char_indices() {
        if in_dq {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_dq = false;
            }
            continue;
        }
        if in_sq {
            if ch == '\'' {
                in_sq = false;
            }
            continue;
        }
        match ch {
            '"' => in_dq = true,
            '\'' => in_sq = true,
            '[' => brack_d += 1,
            ']' => brack_d = brack_d.saturating_sub(1),
            '{' => brace_d += 1,
            '}' => brace_d = brace_d.saturating_sub(1),
            ',' if brack_d == 0 && brace_d == 0 => {
                items.push(&body[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    items.push(&body[start..]);
    items
}

/// Expands valid TOML root-level dotted keys (`package.name = "demo"`, `lib.path = "custom.rs"`,
/// `features.default = ["x"]`, `dependencies.b = { ... }`, `target.'cfg(...)'.dependencies.b = ...`,
/// `workspace.members = [...]`) AND root/workspace inline tables (`package = { ... }`,
/// `lib = { ... }`, `features = { ... }`, `dependencies = { b = { ... } }`, `workspace = { ... }`,
/// `bin = [{ ... }]`, `test = [{ ... }]`) into canonical `[section]\nkey = value` TOML text so
/// EVERY `Cargo.toml` parser handles all valid TOML syntax uniformly!
pub(super) fn expand_cargo_toml_root_syntaxes(text: &str) -> String {
    pub(super) fn emit_inline_table(section: &str, inline_src: &str, out: &mut Vec<String>) {
        let trimmed = inline_src.trim();
        let body = trimmed
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(trimmed);
        let mut direct_kvs: Vec<(String, String)> = Vec::new();
        let mut sub_tables: Vec<(String, String)> = Vec::new();
        for ent in split_toml_top_level_commas(body) {
            if let Some((ek, ev)) = split_toml_kv_outside_quotes(ent) {
                let kt = ek.trim();
                let vt = ev.trim();
                let ksegs = split_toml_dotted_key(kt);
                if section == "[workspace]"
                    && ksegs.len() == 1
                    && matches!(ksegs[0].as_str(), "dependencies" | "package")
                    && vt.starts_with('{')
                    && vt.ends_with('}')
                {
                    sub_tables.push((format!("[workspace.{}]", ksegs[0]), vt.to_string()));
                } else if section == "[workspace]"
                    && ksegs.len() >= 2
                    && matches!(ksegs[0].as_str(), "dependencies" | "package")
                {
                    sub_tables.push((
                        format!("[workspace.{}]", ksegs[0]),
                        format!("{} = {vt}", ksegs[1..].join(".")),
                    ));
                } else if !kt.is_empty() {
                    direct_kvs.push((kt.to_string(), vt.to_string()));
                }
            }
        }
        out.push(section.to_string());
        for (k, v) in direct_kvs {
            out.push(format!("{k} = {v}"));
        }
        for (sub_sec, payload) in sub_tables {
            if payload.starts_with('{') && payload.ends_with('}') {
                emit_inline_table(&sub_sec, &payload, out);
            } else {
                out.push(sub_sec);
                out.push(payload);
            }
        }
    }

    // First join multi-line `[...]` arrays, `{...}` inline tables, AND decode multi-line
    // `"""..."""` / `'''...'''` strings (preserving TOML 1.0 newlines & line-ending `\` trimming)
    // into canonical single-line `"..."` basic strings so EVERY downstream `.lines()` parser sees
    // exact decoded values and an unmatched `[` in a description NEVER swallows `[lib]`!
    let mut stmts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut brack_d = 0usize;
    let mut brace_d = 0usize;
    let mut in_ml_dq = false;
    let mut in_ml_sq = false;
    let mut ml_buf = String::new();
    let encode_toml_single_basic = |s: &str| -> String {
        serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
    };
    for raw in text.lines() {
        let mut clean_line = String::with_capacity(raw.len());
        let mut in_dq = false;
        let mut in_sq = false;
        let mut esc = false;
        let bytes = raw.as_bytes();
        let mut idx = 0usize;
        if in_ml_dq || in_ml_sq {
            ml_buf.push('\n');
        }
        while idx < bytes.len() {
            if in_ml_dq {
                if esc {
                    let ch = raw[idx..].chars().next().unwrap_or('\0');
                    ml_buf.push(ch);
                    idx += ch.len_utf8();
                    esc = false;
                    continue;
                }
                if bytes[idx] == b'\\' {
                    ml_buf.push('\\');
                    idx += 1;
                    esc = true;
                    continue;
                }
                if raw[idx..].starts_with("\"\"\"") {
                    idx += 3;
                    // Per TOML 1.0, up to 2 extra `"` immediately before closing `"""` are part of content.
                    while idx < bytes.len() && bytes[idx] == b'"' {
                        ml_buf.push('"');
                        idx += 1;
                    }
                    let inner = ml_buf
                        .strip_prefix("\r\n")
                        .or_else(|| ml_buf.strip_prefix('\n'))
                        .unwrap_or(&ml_buf);
                    let decoded = decode_toml_basic_string(inner);
                    clean_line.push_str(&encode_toml_single_basic(&decoded));
                    ml_buf.clear();
                    in_ml_dq = false;
                    continue;
                }
                let ch = raw[idx..].chars().next().unwrap_or('\0');
                ml_buf.push(ch);
                idx += ch.len_utf8();
                continue;
            }
            if in_ml_sq {
                if raw[idx..].starts_with("'''") {
                    idx += 3;
                    while idx < bytes.len() && bytes[idx] == b'\'' {
                        ml_buf.push('\'');
                        idx += 1;
                    }
                    let inner = ml_buf
                        .strip_prefix("\r\n")
                        .or_else(|| ml_buf.strip_prefix('\n'))
                        .unwrap_or(&ml_buf);
                    clean_line.push_str(&encode_toml_single_basic(inner));
                    ml_buf.clear();
                    in_ml_sq = false;
                    continue;
                }
                let ch = raw[idx..].chars().next().unwrap_or('\0');
                ml_buf.push(ch);
                idx += ch.len_utf8();
                continue;
            }
            if in_dq {
                let ch = raw[idx..].chars().next().unwrap_or('\0');
                clean_line.push(ch);
                idx += ch.len_utf8();
                if esc {
                    esc = false;
                } else if ch == '\\' {
                    esc = true;
                } else if ch == '"' {
                    in_dq = false;
                }
                continue;
            }
            if in_sq {
                let ch = raw[idx..].chars().next().unwrap_or('\0');
                clean_line.push(ch);
                idx += ch.len_utf8();
                if ch == '\'' {
                    in_sq = false;
                }
                continue;
            }
            if bytes[idx] == b'#' {
                break;
            }
            if raw[idx..].starts_with("\"\"\"") {
                idx += 3;
                ml_buf.clear();
                in_ml_dq = true;
                continue;
            }
            if raw[idx..].starts_with("'''") {
                idx += 3;
                ml_buf.clear();
                in_ml_sq = true;
                continue;
            }
            let ch = raw[idx..].chars().next().unwrap_or('\0');
            clean_line.push(ch);
            idx += ch.len_utf8();
            match ch {
                '"' => in_dq = true,
                '\'' => in_sq = true,
                '[' => brack_d += 1,
                ']' => brack_d = brack_d.saturating_sub(1),
                '{' => brace_d += 1,
                '}' => brace_d = brace_d.saturating_sub(1),
                _ => {}
            }
        }
        let code = clean_line.trim();
        if !code.is_empty() {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(code);
        }
        if brack_d == 0 && brace_d == 0 && !in_ml_dq && !in_ml_sq && !cur.is_empty() {
            stmts.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        stmts.push(cur);
    }

    let mut out: Vec<String> = Vec::new();
    let mut cur_sec = String::new();
    for stmt in stmts {
        if let Some(h) = normalize_toml_table_header(&stmt) {
            cur_sec = h.clone();
            out.push(h);
            continue;
        }
        if cur_sec.is_empty()
            && let Some((k, v)) = split_toml_kv_outside_quotes(&stmt)
        {
            let segs = split_toml_dotted_key(k);
            let vt = v.trim();
            if segs.len() == 1 && vt.starts_with('{') && vt.ends_with('}') {
                emit_inline_table(&format!("[{}]", segs[0]), vt, &mut out);
                continue;
            }
            if segs.len() == 1
                && matches!(segs[0].as_str(), "bin" | "test" | "example" | "bench")
                && vt.starts_with('[')
                && vt.ends_with(']')
            {
                let sec = format!("[[{}]]", segs[0]);
                for elem in split_toml_top_level_commas(&vt[1..vt.len() - 1]) {
                    let et = elem.trim();
                    if et.starts_with('{') && et.ends_with('}') {
                        emit_inline_table(&sec, et, &mut out);
                    }
                }
                continue;
            }
            if segs.len() >= 2 {
                if segs[0] == "target"
                    && segs.len() >= 3
                    && matches!(
                        segs[2].as_str(),
                        "dependencies" | "dev-dependencies" | "build-dependencies"
                    )
                {
                    let sec = format!("[target.{}.{}]", segs[1], segs[2]);
                    if segs.len() == 3 && vt.starts_with('{') && vt.ends_with('}') {
                        emit_inline_table(&sec, vt, &mut out);
                    } else if segs.len() >= 4 {
                        out.push(sec);
                        out.push(format!("{} = {vt}", segs[3..].join(".")));
                    }
                    continue;
                }
                if segs[0] == "workspace"
                    && segs.len() >= 2
                    && matches!(segs[1].as_str(), "dependencies" | "package")
                {
                    let sec = format!("[workspace.{}]", segs[1]);
                    if segs.len() == 2 && vt.starts_with('{') && vt.ends_with('}') {
                        emit_inline_table(&sec, vt, &mut out);
                    } else if segs.len() >= 3 {
                        out.push(sec);
                        out.push(format!("{} = {vt}", segs[2..].join(".")));
                    }
                    continue;
                }
                out.push(format!("[{}]", segs[0]));
                out.push(format!("{} = {vt}", segs[1..].join(".")));
                continue;
            }
        } else if cur_sec == "[workspace]"
            && let Some((k, v)) = split_toml_kv_outside_quotes(&stmt)
        {
            let segs = split_toml_dotted_key(k);
            let vt = v.trim();
            if segs.len() == 1
                && matches!(segs[0].as_str(), "dependencies" | "package")
                && vt.starts_with('{')
                && vt.ends_with('}')
            {
                emit_inline_table(&format!("[workspace.{}]", segs[0]), vt, &mut out);
                out.push("[workspace]".to_string());
                continue;
            }
        }
        out.push(stmt);
    }
    out.join("\n")
}

/// Extracts directory paths from a `rustup` `settings.toml` `[overrides]` section (including
/// root-level dotted `overrides."/workspace/b#tools" = "stable"` and inline `overrides = { ... }`)
/// using direct structural TOML key/comment parsing so quoted paths containing `#` or `=` never
/// lose quoting or get truncated!
pub fn parse_rustup_override_dirs(toml_text: &str) -> Vec<String> {
    let mut in_root = true;
    let mut in_overrides = false;
    let mut dirs = Vec::new();
    for raw in toml_text.lines() {
        let line = strip_toml_comment_outside_quotes(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(h) = normalize_toml_table_header(line) {
            in_root = false;
            in_overrides = h == "[overrides]";
            continue;
        }
        if in_overrides && let Some((k, _)) = split_toml_kv_outside_quotes(line) {
            let decoded = unquote_toml_scalar(k.trim()).unwrap_or_else(|| k.trim().to_string());
            if !decoded.is_empty() {
                dirs.push(decoded);
            }
        } else if in_root && let Some((k, v)) = split_toml_kv_outside_quotes(line) {
            let segs = split_toml_dotted_key(k);
            if segs.len() == 2 && segs[0] == "overrides" && !segs[1].is_empty() {
                dirs.push(segs[1].clone());
            } else if segs.len() == 1
                && segs[0] == "overrides"
                && let Some(body) = v.trim().strip_prefix('{').and_then(|s| s.strip_suffix('}'))
            {
                for ent in split_toml_top_level_commas(body) {
                    if let Some((ek, _)) = split_toml_kv_outside_quotes(ent) {
                        let decoded =
                            unquote_toml_scalar(ek.trim()).unwrap_or_else(|| ek.trim().to_string());
                        if !decoded.is_empty() {
                            dirs.push(decoded);
                        }
                    }
                }
            }
        }
    }
    dirs
}

pub(super) fn read_normalized_cargo_toml(crate_root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(crate_root.join("Cargo.toml")).ok()?;
    Some(expand_cargo_toml_root_syntaxes(&raw))
}

pub(super) fn is_cargo_workspace_root(root: &Path) -> bool {
    let Some(text) = read_normalized_cargo_toml(root) else {
        return false;
    };
    text.lines().any(|line| {
        normalize_toml_table_header(line.trim())
            .is_some_and(|h| h == "[workspace]" || h.starts_with("[workspace."))
    })
}

#[derive(Debug, Clone)]
pub(super) struct CargoManifestTargets {
    pub(super) pkg_name: Option<String>,
    pub(super) main_bin: Option<String>,
    pub(super) autobins: bool,
    pub(super) autotests: bool,
    /// Crate-relative path of the library target (explicit `[lib] path = "..."` or default `"src/lib.rs"`).
    pub(super) lib_path: Option<String>,
    /// Whether `[lib]` has default `cargo test` execution enabled (`test = false` sets this false).
    pub(super) lib_test_enabled: bool,
    /// Explicit `[[bin]]` targets (normalized crate-relative path -> all declared bin names).
    pub(super) custom_bins: BTreeMap<String, Vec<String>>,
    /// `[[bin]]` target names explicitly disabled from default `cargo test` via `test = false`.
    pub(super) bin_test_disabled: BTreeSet<String>,
    /// `required-features` declared on `[[bin]]` targets (bin target name -> required features).
    pub(super) custom_bin_required_features: BTreeMap<String, Vec<String>>,
    /// Explicit `[[test]]` targets (normalized crate-relative path -> all declared test names).
    pub(super) custom_tests: BTreeMap<String, Vec<String>>,
    /// `[[test]]` target names explicitly disabled from default `cargo test` via `test = false`.
    pub(super) test_disabled: BTreeSet<String>,
    /// `required-features` declared on `[[test]]` targets (test target name -> required features).
    pub(super) custom_test_required_features: BTreeMap<String, Vec<String>>,
    /// Targets declared with `harness = false` (`"lib"`, `"bin:<name>"`, `"test:<name>"`) whose
    /// custom harness may not emit libtest `: test` lines under `-- --list`.
    pub(super) harness_false_targets: BTreeSet<String>,
}

impl Default for CargoManifestTargets {
    fn default() -> Self {
        Self {
            pkg_name: None,
            main_bin: None,
            autobins: true,
            autotests: true,
            lib_path: None,
            lib_test_enabled: true,
            custom_bins: BTreeMap::new(),
            bin_test_disabled: BTreeSet::new(),
            custom_bin_required_features: BTreeMap::new(),
            custom_tests: BTreeMap::new(),
            test_disabled: BTreeSet::new(),
            custom_test_required_features: BTreeMap::new(),
            harness_false_targets: BTreeSet::new(),
        }
    }
}

/// Strips a TOML `# ...` comment occurring outside single- or double-quoted strings so quoted
/// words inside comments (e.g. `required-features = ["fast"] # "nightly" optional`) are never
/// parsed as TOML array elements.
pub(super) fn strip_toml_comment_outside_quotes(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_double = false;
    let mut in_single = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_double {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else if in_single {
            if b == b'\'' {
                in_single = false;
            }
        } else if b == b'"' {
            in_double = true;
        } else if b == b'\'' {
            in_single = true;
        } else if b == b'#' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

pub(super) fn rel_path_in_crate<'a>(crate_dir: &str, test_file: &'a str) -> &'a str {
    if crate_dir.is_empty() {
        test_file
    } else {
        test_file
            .strip_prefix(&format!("{crate_dir}/"))
            .unwrap_or(test_file)
    }
}

pub(super) fn parse_cargo_manifest_targets(workdir: &Path) -> CargoManifestTargets {
    let mut out = CargoManifestTargets::default();
    let Some(text) = read_normalized_cargo_toml(workdir) else {
        if workdir.join("src/lib.rs").is_file() {
            out.lib_path = Some("src/lib.rs".to_string());
        }
        return out;
    };
    let mut section = "";
    let mut pkg_name: Option<String> = None;
    let mut autobins = true;
    let mut autotests = true;
    let mut lib_explicit_path: Option<String> = None;
    let mut lib_test_enabled = true;
    let mut lib_harness_false = false;
    type ParsedTargetEntry = (Option<String>, Option<String>, Vec<String>, bool, bool);
    let mut bins: Vec<ParsedTargetEntry> = Vec::new();
    let mut cur_bin: ParsedTargetEntry = (None, None, Vec::new(), true, false);
    let mut collecting_bin_req_feats = false;
    let mut tests: Vec<ParsedTargetEntry> = Vec::new();
    let mut cur_test: ParsedTargetEntry = (None, None, Vec::new(), true, false);
    let mut collecting_test_req_feats = false;
    let parse_toml_str_array = parse_toml_string_array_items;

    let norm_manifest_path = |raw: String| -> String {
        raw.trim()
            .trim_start_matches("./")
            .replace('\\', "/")
    };

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if section == "[[bin]]" && collecting_bin_req_feats {
            let code = strip_toml_comment_outside_quotes(line).trim();
            parse_toml_str_array(code, &mut cur_bin.2);
            if code.contains(']') {
                collecting_bin_req_feats = false;
            }
            continue;
        }
        if section == "[[test]]" && collecting_test_req_feats {
            let code = strip_toml_comment_outside_quotes(line).trim();
            parse_toml_str_array(code, &mut cur_test.2);
            if code.contains(']') {
                collecting_test_req_feats = false;
            }
            continue;
        }
        if line.starts_with('[') {
            if section == "[[bin]]" && (cur_bin.0.is_some() || cur_bin.1.is_some()) {
                bins.push(std::mem::replace(
                    &mut cur_bin,
                    (None, None, Vec::new(), true, false),
                ));
            } else if section == "[[bin]]" {
                cur_bin = (None, None, Vec::new(), true, false);
            }
            if section == "[[test]]" && (cur_test.0.is_some() || cur_test.1.is_some()) {
                tests.push(std::mem::replace(
                    &mut cur_test,
                    (None, None, Vec::new(), true, false),
                ));
            } else if section == "[[test]]" {
                cur_test = (None, None, Vec::new(), true, false);
            }
            let header_raw = strip_toml_comment_outside_quotes(line).trim();
            let header_norm = normalize_toml_table_header(header_raw);
            section = match header_norm.as_deref() {
                Some("[package]") => "package",
                Some("[lib]") => "[lib]",
                Some("[[bin]]") => "[[bin]]",
                Some("[[test]]") => "[[test]]",
                _ => "other",
            };
            continue;
        }
        let Some((k, v)) = split_toml_kv_outside_quotes(line) else {
            continue;
        };
        let key_owned = unquote_toml_scalar(k.trim()).unwrap_or_else(|| k.trim().to_string());
        let key = key_owned.as_str();
        match section {
            "package" => {
                if key == "name" {
                    pkg_name = unquote_toml_scalar(v);
                } else if key == "autobins" {
                    let b = v.split('#').next().unwrap_or(v).trim();
                    if b == "false" {
                        autobins = false;
                    }
                } else if key == "autotests" {
                    let b = v.split('#').next().unwrap_or(v).trim();
                    if b == "false" {
                        autotests = false;
                    }
                }
            }
            "[lib]" => {
                if key == "path"
                    && let Some(p) = unquote_toml_scalar(v)
                {
                    let clean = norm_manifest_path(p);
                    if !clean.is_empty() {
                        lib_explicit_path = Some(clean);
                    }
                } else if key == "test"
                    && strip_toml_comment_outside_quotes(v).trim() == "false"
                {
                    lib_test_enabled = false;
                } else if key == "harness"
                    && strip_toml_comment_outside_quotes(v).trim() == "false"
                {
                    lib_harness_false = true;
                }
            }
            "[[bin]]" => {
                if key == "name" {
                    cur_bin.0 = unquote_toml_scalar(v);
                } else if key == "path" {
                    cur_bin.1 = unquote_toml_scalar(v);
                } else if key == "test"
                    && strip_toml_comment_outside_quotes(v).trim() == "false"
                {
                    cur_bin.3 = false;
                } else if key == "harness"
                    && strip_toml_comment_outside_quotes(v).trim() == "false"
                {
                    cur_bin.4 = true;
                } else if key == "required-features" {
                    let v_clean = strip_toml_comment_outside_quotes(v);
                    parse_toml_str_array(v_clean, &mut cur_bin.2);
                    if v_clean.contains('[') && !v_clean.contains(']') {
                        collecting_bin_req_feats = true;
                    }
                }
            }
            "[[test]]" => {
                if key == "name" {
                    cur_test.0 = unquote_toml_scalar(v);
                } else if key == "path" {
                    cur_test.1 = unquote_toml_scalar(v);
                } else if key == "test"
                    && strip_toml_comment_outside_quotes(v).trim() == "false"
                {
                    cur_test.3 = false;
                } else if key == "harness"
                    && strip_toml_comment_outside_quotes(v).trim() == "false"
                {
                    cur_test.4 = true;
                } else if key == "required-features" {
                    let v_clean = strip_toml_comment_outside_quotes(v);
                    parse_toml_str_array(v_clean, &mut cur_test.2);
                    if v_clean.contains('[') && !v_clean.contains(']') {
                        collecting_test_req_feats = true;
                    }
                }
            }
            _ => {}
        }
    }
    if section == "[[bin]]" && (cur_bin.0.is_some() || cur_bin.1.is_some()) {
        bins.push(cur_bin);
    }
    if section == "[[test]]" && (cur_test.0.is_some() || cur_test.1.is_some()) {
        tests.push(cur_test);
    }

    out.lib_test_enabled = lib_test_enabled;
    if lib_harness_false {
        out.harness_false_targets.insert("lib".to_string());
    }
    for (name_opt, path_opt, req_feats, test_enabled, harness_false) in &bins {
        if let Some(raw_p) = path_opt {
            let clean_p = norm_manifest_path(raw_p.clone());
            if !clean_p.is_empty() {
                let bin_name = name_opt.clone().or_else(|| {
                    Path::new(&clean_p)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(ToOwned::to_owned)
                });
                if let Some(bn) = bin_name {
                    if !*test_enabled {
                        out.bin_test_disabled.insert(bn.clone());
                    }
                    if *harness_false {
                        out.harness_false_targets.insert(format!("bin:{bn}"));
                    }
                    if !req_feats.is_empty() {
                        out.custom_bin_required_features
                            .insert(bn.clone(), req_feats.clone());
                    }
                    let entry = out.custom_bins.entry(clean_p).or_default();
                    if !entry.contains(&bn) {
                        entry.push(bn);
                    }
                }
            }
        } else if let Some(bn) = name_opt {
            if !*test_enabled {
                out.bin_test_disabled.insert(bn.clone());
            }
            if *harness_false {
                out.harness_false_targets.insert(format!("bin:{bn}"));
            }
            if !req_feats.is_empty() {
                out.custom_bin_required_features
                    .insert(bn.clone(), req_feats.clone());
            }
            // Explicit `[[bin]] name = "cli"` without `path = "..."` validly infers
            // `src/bin/cli.rs`, `src/bin/cli/main.rs`, or `src/main.rs` (if `name == pkg_name`)
            // even when `autobins = false`.
            let cand_rs = format!("src/bin/{bn}.rs");
            let cand_dir = format!("src/bin/{bn}/main.rs");
            let inferred = if workdir.join(&cand_rs).is_file() {
                Some(cand_rs)
            } else if workdir.join(&cand_dir).is_file() {
                Some(cand_dir)
            } else if pkg_name.as_deref() == Some(bn.as_str())
                && workdir.join("src/main.rs").is_file()
            {
                Some("src/main.rs".to_string())
            } else {
                None
            };
            if let Some(p) = inferred {
                let entry = out.custom_bins.entry(p).or_default();
                if !entry.contains(bn) {
                    entry.push(bn.clone());
                }
            }
        }
    }

    for (name_opt, path_opt, req_feats, test_enabled, harness_false) in &tests {
        if let Some(raw_p) = path_opt {
            let clean_p = norm_manifest_path(raw_p.clone());
            if !clean_p.is_empty() {
                let test_name = name_opt.clone().or_else(|| {
                    Path::new(&clean_p)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(ToOwned::to_owned)
                });
                if let Some(tn) = test_name {
                    if !*test_enabled {
                        out.test_disabled.insert(tn.clone());
                    }
                    if *harness_false {
                        out.harness_false_targets.insert(format!("test:{tn}"));
                    }
                    if !req_feats.is_empty() {
                        out.custom_test_required_features
                            .insert(tn.clone(), req_feats.clone());
                    }
                    let entry = out.custom_tests.entry(clean_p).or_default();
                    if !entry.contains(&tn) {
                        entry.push(tn);
                    }
                }
            }
        } else if let Some(tn) = name_opt {
            if !*test_enabled {
                out.test_disabled.insert(tn.clone());
            }
            if *harness_false {
                out.harness_false_targets.insert(format!("test:{tn}"));
            }
            if !req_feats.is_empty() {
                out.custom_test_required_features
                    .insert(tn.clone(), req_feats.clone());
            }
            // Explicit `[[test]] name = "smoke"` without `path = "..."` validly infers
            // `tests/smoke.rs` or `tests/smoke/main.rs` even when `autotests = false`.
            let flat = format!("tests/{tn}.rs");
            let nested = format!("tests/{tn}/main.rs");
            let inferred = if workdir.join(&flat).is_file() {
                Some(flat)
            } else if workdir.join(&nested).is_file() {
                Some(nested)
            } else {
                None
            };
            if let Some(p) = inferred {
                let entry = out.custom_tests.entry(p).or_default();
                if !entry.contains(tn) {
                    entry.push(tn.clone());
                }
            }
        }
    }

    out.lib_path = if let Some(lp) = lib_explicit_path {
        workdir.join(&lp).is_file().then_some(lp)
    } else if workdir.join("src/lib.rs").is_file() {
        Some("src/lib.rs".to_string())
    } else {
        None
    };

    out.main_bin = if workdir.join("src/main.rs").is_file() {
        if let Some((Some(name), _, _, _, _)) = bins.iter().find(|(_, p, _, _, _)| {
            p.as_deref()
                .is_some_and(|path| path.trim_start_matches("./") == "src/main.rs")
        }) {
            Some(name.clone())
        } else if let Some(ref p) = pkg_name
            && let Some((Some(name), None, _, _, _)) = bins
                .iter()
                .find(|(n, path, _, _, _)| n.as_deref() == Some(p.as_str()) && path.is_none())
        {
            Some(name.clone())
        } else if autobins
            && let Some(ref p) = pkg_name
            && !bins.iter().any(|(n, path, _, _, _)| {
                n.as_deref() == Some(p.as_str())
                    && path
                        .as_deref()
                        .is_some_and(|bp| bp.trim_start_matches("./") != "src/main.rs")
            })
        {
            Some(p.clone())
        } else {
            None
        }
    } else {
        None
    };
    out.pkg_name = pkg_name;
    out.autobins = autobins;
    out.autotests = autotests;
    out
}


