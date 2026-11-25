use super::*;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

pub(super) fn build_node_hash_bytes(dep_path: &str, dep_node: &FileNode, is_self_test: bool) -> Vec<u8> {
    let effective_sha = if !is_self_test && !dep_node.api_semantic_sha256.is_empty() {
        &dep_node.api_semantic_sha256
    } else if dep_node.semantic_sha256.is_empty() {
        &dep_node.sha256
    } else {
        &dep_node.semantic_sha256
    };
    let mut out = Vec::with_capacity(
        5 + dep_path.len()
            + effective_sha.len()
            + dep_node.parsed.env_vars_referenced.len() * 80,
    );
    out.extend_from_slice(b"py:");
    out.extend_from_slice(dep_path.as_bytes());
    out.push(b':');
    out.extend_from_slice(effective_sha.as_bytes());
    out.push(b'\n');
    for env_key in &dep_node.parsed.env_vars_referenced {
        out.extend_from_slice(b"env_ref:");
        out.extend_from_slice(env_key.as_bytes());
        if let Some(val) = std::env::var_os(env_key) {
            out.extend_from_slice(b"=S:");
            out.extend_from_slice(sha256_hex(val.as_encoded_bytes()).as_bytes());
        } else {
            out.extend_from_slice(b"=U");
        }
        out.push(b'\n');
    }
    out
}

/// Token-level detector for Rust source-location macros (`line!()`, `column!()`, `file!()`),
/// handling arbitrary whitespace/comments BOTH before and after `!` and any delimiter (`()`, `[]`, `{}`).
pub(super) fn rust_source_has_location_macro(source: &str) -> bool {
    // Also treat Rust panic-hook location inspection (`PanicHookInfo::location()` /
    // `PanicInfo::location()` / `panic::set_hook`) as location-sensitive so comment/blank-line
    // shifts before `panic!()` always invalidate cached tests.
    if (source.contains("set_hook") || source.contains("PanicHookInfo") || source.contains("PanicInfo"))
        && source.contains(".location(")
    {
        return true;
    }
    let bytes = source.as_bytes();
    let len = bytes.len();
    let skip_rust_ws_comments = |pos: &mut usize| {
        while *pos < len {
            if bytes[*pos].is_ascii_whitespace() {
                *pos += 1;
            } else if bytes[*pos] == b'/' && *pos + 1 < len && bytes[*pos + 1] == b'/' {
                *pos += 2;
                while *pos < len && bytes[*pos] != b'\n' {
                    *pos += 1;
                }
            } else if bytes[*pos] == b'/' && *pos + 1 < len && bytes[*pos + 1] == b'*' {
                *pos += 2;
                let mut depth = 1usize;
                while *pos + 1 < len && depth > 0 {
                    if bytes[*pos] == b'/' && bytes[*pos + 1] == b'*' {
                        depth += 1;
                        *pos += 2;
                    } else if bytes[*pos] == b'*' && bytes[*pos + 1] == b'/' {
                        depth -= 1;
                        *pos += 2;
                    } else {
                        *pos += 1;
                    }
                }
            } else {
                break;
            }
        }
    };
    let mut i = 0usize;
    while i < len {
        let b = bytes[i];
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &source[start..i];
            if matches!(ident, "line" | "column" | "file") {
                let mut probe = i;
                skip_rust_ws_comments(&mut probe);
                if probe < len && bytes[probe] == b'!' {
                    probe += 1;
                    skip_rust_ws_comments(&mut probe);
                    if probe < len && matches!(bytes[probe], b'(' | b'[' | b'{') {
                        return true;
                    }
                }
                // Conservatively treat ANY `use ...::line as ...` OR grouped
                // `use std::{line as here};` macro aliasing as location-sensitive so `here!()`
                // never loses raw hashing.
                let mut after = i;
                skip_rust_ws_comments(&mut after);
                if source[after..].starts_with("as")
                    && bytes
                        .get(after + 2)
                        .is_some_and(u8::is_ascii_whitespace)
                {
                    let mut prev = start;
                    while prev > 0 && bytes[prev - 1].is_ascii_whitespace() {
                        prev -= 1;
                    }
                    if (prev >= 2 && bytes[prev - 2] == b':' && bytes[prev - 1] == b':')
                        || (prev >= 1 && matches!(bytes[prev - 1], b'{' | b','))
                    {
                        return true;
                    }
                }
            } else if ident == "track_caller" {
                return true;
            } else if ident == "caller" {
                // Match `Location::caller()`, aliased `Loc::caller()`, or `std::panic::Location::caller`
                // regardless of how `Location` was imported/aliased.
                let mut prev = start;
                while prev > 0 && bytes[prev - 1].is_ascii_whitespace() {
                    prev -= 1;
                }
                if prev >= 2 && bytes[prev - 2] == b':' && bytes[prev - 1] == b':' {
                    let mut probe = i;
                    skip_rust_ws_comments(&mut probe);
                    if probe < len && bytes[probe] == b'(' {
                        return true;
                    }
                }
            }
            continue;
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        i += ch.len_utf8();
    }
    false
}

/// Token-level detector for Go call-stack / PC-to-line inspection (`runtime.Caller`,
/// `runtime.Callers`, `runtime.CallersFrames`, `runtime.FuncForPC`, `FileLine`, `runtime.Stack`,
/// `debug.Stack`, `debug.PrintStack`, aliased or dot-imported, calls or function values).
pub(super) fn go_source_has_caller_inspection(source: &str) -> bool {
    if !source.contains("Caller")
        && !source.contains("Stack")
        && !source.contains("FileLine")
        && !source.contains("FuncForPC")
    {
        return false;
    }
    let bytes = source.as_bytes();
    let len = bytes.len();
    let skip_go_ws_comments = |pos: &mut usize| {
        while *pos < len {
            if bytes[*pos].is_ascii_whitespace() {
                *pos += 1;
            } else if bytes[*pos] == b'/' && *pos + 1 < len && bytes[*pos + 1] == b'/' {
                *pos += 2;
                while *pos < len && bytes[*pos] != b'\n' {
                    *pos += 1;
                }
            } else if bytes[*pos] == b'/' && *pos + 1 < len && bytes[*pos + 1] == b'*' {
                *pos += 2;
                while *pos + 1 < len && !(bytes[*pos] == b'*' && bytes[*pos + 1] == b'/') {
                    *pos += 1;
                }
                if *pos + 1 < len {
                    *pos += 2;
                } else {
                    *pos = len;
                }
            } else {
                break;
            }
        }
    };
    // Detect `runtime` or `runtime/debug` imports using decoded Go string literals so escaped
    // imports like `import . "runtime\x2fdebug"` are recognized accurately.
    let mut go_imps = BTreeSet::new();
    let mut go_envs = BTreeSet::new();
    let mut go_refs = BTreeSet::new();
    let mut go_syms = BTreeSet::new();
    let mut go_test_syms = BTreeSet::new();
    let _ = extract_go_imports_and_env_lexically(
        source,
        false,
        &mut go_imps,
        &mut go_envs,
        &mut go_refs,
        &mut go_syms,
        &mut go_test_syms,
    );
    let imports_runtime_or_debug = go_imps
        .iter()
        .any(|imp| imp.module == "runtime" || imp.module == "runtime/debug");
    let mut i = 0usize;
    let mut prev_was_dot = false;
    while i < len {
        skip_go_ws_comments(&mut i);
        if i >= len {
            break;
        }
        let b = bytes[i];
        let after_dot = prev_was_dot;
        prev_was_dot = false;
        if b == b'"' || b == b'`' || b == b'\'' {
            let q = b;
            i += 1;
            while i < len && bytes[i] != q {
                if q != b'`' && bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    let esc_ch = source[i..].chars().next().unwrap_or(' ');
                    i += esc_ch.len_utf8();
                    continue;
                }
                let ch = source[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
            }
            if i < len {
                i += 1;
            }
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &source[start..i];
            if matches!(
                ident,
                "Caller" | "Callers" | "CallersFrames" | "FuncForPC" | "FileLine"
            ) {
                let mut probe = i;
                skip_go_ws_comments(&mut probe);
                if (probe < len && bytes[probe] == b'(') || after_dot || imports_runtime_or_debug {
                    return true;
                }
            } else if matches!(ident, "Stack" | "PrintStack") && imports_runtime_or_debug {
                // Match both direct calls (`debug.Stack()`) AND function-value references
                // (`import . "runtime/debug"; f := Stack; f()`).
                return true;
            }
            continue;
        }
        if b == b'.' {
            prev_was_dot = true;
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        i += ch.len_utf8();
    }
    false
}

pub fn rust_api_semantic_sha256(source: &str, fallback_semantic_sha: &str) -> String {
    // Preserve exact line/column numbers in the API hash too when location macros are present.
    if rust_source_has_location_macro(source) {
        return sha256_hex(source.as_bytes());
    }
    let stripped = strip_rust_cfg_test_items(source);
    if stripped.len() == source.len() {
        return fallback_semantic_sha.to_string();
    }
    let mut hasher = Sha256::new();
    hash_c_style_semantic_into(&stripped, false, false, false, true, false, &mut hasher);
    hex_encode(&hasher.finalize())
}

pub(super) fn strip_rust_cfg_test_items(source: &str) -> String {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    let mut pending_test_item = false;

    while i < len {
        let b = bytes[i];
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            let end = memchr::memchr(b'\n', &bytes[i + 2..])
                .map(|off| i + 2 + off + 1)
                .unwrap_or(len);
            if !pending_test_item {
                out.push_str(&source[i..end]);
            }
            i = end;
            continue;
        }
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if !pending_test_item {
                out.push_str(&source[start..i]);
            }
            continue;
        }
        if b == b'"' {
            let (_, next_i) = scan_rust_string(source, i);
            if !pending_test_item {
                out.push_str(&source[i..next_i]);
            }
            i = next_i;
            continue;
        }
        if b == b'\'' {
            let start = i;
            if i + 1 < len && bytes[i + 1] == b'\\' {
                i += 2;
                while i < len && bytes[i] != b'\'' && bytes[i] != b'\n' {
                    i += 1;
                }
                if i < len && bytes[i] == b'\'' {
                    i += 1;
                }
            } else if i + 1 < len
                && let Some(ch) = source[i + 1..].chars().next()
                && i + 1 + ch.len_utf8() < len
                && bytes[i + 1 + ch.len_utf8()] == b'\''
            {
                i += 1 + ch.len_utf8() + 1;
            } else {
                i += 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
            }
            if !pending_test_item {
                out.push_str(&source[start..i]);
            }
            continue;
        }
        if is_ident_start(b)
            && let Some((_, next_i)) = try_scan_rust_raw_or_byte_string(source, i)
        {
            if !pending_test_item {
                out.push_str(&source[i..next_i]);
            }
            i = next_i;
            continue;
        }
        if b == b'#' {
            let mut k = i + 1;
            while k < len && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            let is_inner = k < len && bytes[k] == b'!';
            if is_inner {
                k += 1;
                while k < len && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
            }
            if k < len && bytes[k] == b'[' {
                let mut depth = 1usize;
                let mut j = k + 1;
                while j < len && depth > 0 {
                    match bytes[j] {
                        b'"' => {
                            let (_, next_j) = scan_rust_string(source, j);
                            j = next_j;
                        }
                        b'[' => {
                            depth += 1;
                            j += 1;
                        }
                        b']' => {
                            depth -= 1;
                            j += 1;
                        }
                        _ => j += 1,
                    }
                }
                let attr_slice = &source[k + 1..j.saturating_sub(1)];
                let attr_toks = tokenize_rust(attr_slice);
                // Extract only the attribute *path* identifiers before `(` or `=` so `#[cfg(not(test))]`
                // has path `["cfg"]` and is NEVER misclassified as a `#[test]` attribute.
                let mut attr_path_idents: Vec<&str> = Vec::new();
                for t in &attr_toks {
                    match t {
                        RsToken::Ident(id) => attr_path_idents.push(*id),
                        RsToken::ColonColon => {}
                        _ => break,
                    }
                }
                let attr_idents: Vec<&str> = attr_toks
                    .iter()
                    .filter_map(|t| match t {
                        RsToken::Ident(id) => Some(*id),
                        _ => None,
                    })
                    .collect();
                let path_first = attr_path_idents.first().copied().unwrap_or("");
                // Only strip `#[cfg(test)]` or `#[cfg(all(..., test, ...))]` — never strip
                // `#[cfg(any(test, feature = "..."))]` or `#[cfg(not(test))]` which can compile
                // into production code.
                let is_cfg_test = !is_inner
                    && path_first == "cfg"
                    && attr_idents.contains(&"test")
                    && !attr_idents.contains(&"not")
                    && !attr_idents.contains(&"any");
                // Only strip well-known test-only attributes, never unknown proc macros like
                // `#[some_crate::test]` that may expand to production items.
                let is_test_fn = !is_inner
                    && matches!(
                        attr_path_idents.as_slice(),
                        ["test"]
                            | ["core", "prelude", "v1", "test"]
                            | ["std", "prelude", "v1", "test"]
                            | ["tokio", "test"]
                            | ["async_std", "test"]
                            | ["actix_rt", "test"]
                            | ["rstest"]
                            | ["rstest", "rstest"]
                            | ["test_case"]
                            | ["test_case", "test_case"]
                            | ["wasm_bindgen_test"]
                            | ["wasm_bindgen_test", "wasm_bindgen_test"]
                    );
                // Only strip `#[cfg(test)]` / `#[test]` when it decorates a `mod` or `fn` item,
                // never a struct field (`#[cfg(test)] field: u32,`) or enum variant.
                let followed_by_mod_or_fn = (is_cfg_test || is_test_fn)
                    && rust_next_item_is_mod_or_fn(&source[j..]);
                if followed_by_mod_or_fn {
                    pending_test_item = true;
                    i = j;
                    continue;
                }
                if !pending_test_item {
                    out.push_str(&source[i..j]);
                }
                i = j;
                continue;
            }
        }
        if pending_test_item {
            if b == b';' {
                pending_test_item = false;
                i += 1;
                continue;
            }
            if b == b'{' {
                let next_i = skip_rust_balanced_braces(source, i);
                // Preserve `#[cfg(test)] mod helper { ... }` when it contains `#[macro_export]`
                // OR when the surrounding file re-exports items (`pub use` / `pub(` `use`) that
                // could expose a private `#[cfg(test)]` helper module to other test modules.
                let block = &source[i..next_i];
                if block.contains("macro_export") || rust_source_has_pub_use(source) {
                    out.push_str(block);
                }
                pending_test_item = false;
                i = next_i;
                continue;
            }
            i += source[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Token-aware check for `pub use` / `pub(...) use` (tolerating newlines/comments between
/// `pub(crate)` and `use`), so formatting across lines never hides re-exported test helpers.
pub(super) fn rust_source_has_pub_use(source: &str) -> bool {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1usize;
            while i + 1 < len && depth > 0 {
                if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if source.is_char_boundary(i)
            && source[i..].starts_with("pub")
            && (i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_'))
            && bytes.get(i + 3).is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
        {
            let mut j = i + 3;
            while j < len && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < len && bytes[j] == b'(' {
                let mut pdepth = 1usize;
                j += 1;
                while j < len && pdepth > 0 {
                    if bytes[j] == b'(' {
                        pdepth += 1;
                    } else if bytes[j] == b')' {
                        pdepth -= 1;
                    }
                    j += 1;
                }
                while j < len && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
            }
            if j < len
                && source.is_char_boundary(j)
                && source[j..].starts_with("use")
                && bytes.get(j + 3).is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
            {
                return true;
            }
        }
        i += source[i..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

pub(super) fn rust_next_item_is_mod_or_fn(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    loop {
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            return false;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            i = memchr::memchr(b'\n', &bytes[i..])
                .map(|off| i + off + 1)
                .unwrap_or(len);
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1usize;
            while i + 1 < len && depth > 0 {
                if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // Skip stacked outer attributes `#[...]`
        if bytes[i] == b'#' && i + 1 < len && bytes[i + 1] == b'[' {
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                match bytes[i] {
                    b'"' => {
                        let (_, next_i) = scan_rust_string(rest, i);
                        i = next_i;
                    }
                    b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b']' => {
                        depth -= 1;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            continue;
        }
        if !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            return false;
        }
        let start = i;
        while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let kw = &rest[start..i];
        match kw {
            "mod" | "fn" => return true,
            // Public/crate-visible `#[cfg(test)] pub(crate) fn helper()` items can be called by
            // unit tests in other modules; never strip them from `api_semantic_sha256`.
            "pub" => return false,
            "async" | "unsafe" | "const" | "default" => {}
            "extern" => {
                while i < len && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < len && bytes[i] == b'"' {
                    let (_, next_i) = scan_rust_string(rest, i);
                    i = next_i;
                }
            }
            _ => return false,
        }
    }
}

pub(super) fn skip_rust_balanced_braces(source: &str, open_brace_idx: usize) -> usize {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = open_brace_idx + 1;
    let mut depth = 1usize;
    while i < len && depth > 0 {
        let b = bytes[i];
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            i = memchr::memchr(b'\n', &bytes[i + 2..])
                .map(|off| i + 2 + off + 1)
                .unwrap_or(len);
        } else if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            let mut cdepth = 1usize;
            while i < len && cdepth > 0 {
                if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    cdepth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    cdepth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if b == b'"' {
            let (_, next_i) = scan_rust_string(source, i);
            i = next_i;
        } else if b == b'\'' {
            if i + 1 < len && bytes[i + 1] == b'\\' {
                i += 2;
                while i < len && bytes[i] != b'\'' && bytes[i] != b'\n' {
                    i += 1;
                }
                if i < len && bytes[i] == b'\'' {
                    i += 1;
                }
            } else if i + 1 < len
                && let Some(ch) = source[i + 1..].chars().next()
                && i + 1 + ch.len_utf8() < len
                && bytes[i + 1 + ch.len_utf8()] == b'\''
            {
                i += 1 + ch.len_utf8() + 1;
            } else {
                i += 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
            }
        } else if is_ident_start(b)
            && let Some((_, next_i)) = try_scan_rust_raw_or_byte_string(source, i)
        {
            i = next_i;
        } else if b == b'{' {
            depth += 1;
            i += 1;
        } else if b == b'}' {
            depth -= 1;
            i += 1;
        } else {
            i += source[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    i
}

pub(super) fn build_file_node_from_bytes(
    rel_str: &str,
    bytes: &[u8],
    stamp: Option<FileStamp>,
    test_file_patterns: &[String],
    test_function_patterns: &[String],
    testpaths: &[String],
) -> FileNode {
    let sha = sha256_hex(bytes);
    let semantic_sha = semantic_sha256_for_source(rel_str, bytes);
    let file_name = Path::new(rel_str)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if PYTEST_CONFIG_FILES.contains(&file_name)
        || RUST_CONFIG_FILES.contains(&file_name)
        || crate::workspace::is_cargo_dot_config_rel(Path::new(rel_str))
        || crate::workspace::is_frontend_config_filename(file_name)
        || POLYGLOT_CONFIG_FILES.contains(&file_name)
        || file_name.ends_with(".csproj")
    {
        let mut parsed = if matches!(
            file_name,
            "Cargo.toml" | "pyproject.toml" | "package.json" | "go.mod" | "go.work"
        ) || ((file_name.starts_with("tsconfig") || file_name.starts_with("jsconfig"))
            && (file_name.ends_with(".json") || file_name.ends_with(".jsonc")))
            || file_name.starts_with("vite.config.")
            || file_name.starts_with("vitest.config.")
            || file_name.starts_with("vitest.workspace.")
            || file_name.starts_with("jest.config.")
        {
            parse_manifest_metadata(&String::from_utf8_lossy(bytes), file_name)
        } else {
            ParsedPythonFile::default()
        };
        // JS/TS frontend config files (`vite.config.ts`, `vitest.config.ts`, `jest.config.ts`,
        // `playwright.config.ts`, etc.) can import shared config modules (`./settings.ts`):
        // parse their ES/CJS imports unconditionally by JS/TS file extension (independent of
        // `is_frontend_source_path`, which excludes config filenames) so config helper edits
        // always propagate into `dep_hash` and `affected_tests`.
        if is_js_or_ts_file_ext(rel_str) {
            let text = String::from_utf8_lossy(bytes);
            let fe_parsed = parse_frontend_source(&text);
            for imp in fe_parsed.imports {
                if !parsed.imports.contains(&imp) {
                    parsed.imports.push(imp);
                }
            }
            for dr in fe_parsed.data_refs {
                if !parsed.data_refs.contains(&dr) {
                    parsed.data_refs.push(dr);
                }
            }
            for ev in fe_parsed.env_vars_referenced {
                if !parsed.env_vars_referenced.contains(&ev) {
                    parsed.env_vars_referenced.push(ev);
                }
            }
            for sym in fe_parsed.defined_symbols {
                if (sym == "fe_dynamic_import"
                    || sym == "fe_dynamic_env"
                    || sym.starts_with("fe_raw_import:")
                    || sym.starts_with("fe_raw_fs:")
                    || sym.starts_with("fe_raw_url_fs:"))
                    && !parsed.defined_symbols.contains(&sym)
                {
                    parsed.defined_symbols.push(sym);
                }
            }
        }
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: String::new(),
            api_semantic_sha256: String::new(),
            stamp,
            is_test: false,
            is_conftest: false,
            is_config: true,
            parsed,
        };
    }
    let compact_semantic = if semantic_sha == sha {
        String::new()
    } else {
        semantic_sha
    };
    if file_name.ends_with(".rs") {
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_rust_source_for_path(rel_str, &source);
        let is_conftest = is_rust_common_test_helper(rel_str);
        // Flat `tests/common.rs` is BOTH a Cargo integration-test target (`--test common`) and
        // a potential shared helper (`mod common;`), whereas `tests/common/mod.rs` is helper-only.
        let is_flat_rust_common =
            rel_str == "tests/common.rs" || rel_str.ends_with("/tests/common.rs");
        let is_test = (!is_conftest || is_flat_rust_common)
            && (!parsed.test_symbols.is_empty() || is_rust_integration_test_path(rel_str));
        let eff_semantic = if compact_semantic.is_empty() {
            sha.as_str()
        } else {
            compact_semantic.as_str()
        };
        // Child submodules of `lib.rs` / `mod.rs` / `main.rs` OR any Rust file that declares
        // external child modules (`mod child;` in `parsed.rust_mods` with `!is_inline`) can call
        // private `#[cfg(test)]` helpers in their parent module via `super::` / `crate::`;
        // only strip private inline `#[cfg(test)]` modules/functions in true leaf modules.
        let has_child_mods = parsed.rust_mods.iter().any(|m| !m.is_inline);
        let api_sha = if is_test
            && !is_rust_integration_test_path(rel_str)
            && !matches!(file_name, "lib.rs" | "mod.rs" | "main.rs")
            && !has_child_mods
        {
            let computed = rust_api_semantic_sha256(&source, eff_semantic);
            if computed == eff_semantic {
                String::new()
            } else {
                computed
            }
        } else {
            String::new()
        };
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: compact_semantic,
            api_semantic_sha256: api_sha,
            stamp,
            is_test,
            is_conftest,
            is_config: false,
            parsed,
        };
    }
    if is_frontend_source_path(rel_str) {
        let source = String::from_utf8_lossy(bytes);
        let mut parsed = parse_frontend_source(&source);
        if is_playwright_e2e_path(rel_str) {
            if !parsed.markers.iter().any(|m| m == "playwright") {
                parsed.markers.push("playwright".to_string());
            }
            if !parsed.module_markers.iter().any(|m| m == "e2e") {
                parsed.module_markers.push("e2e".to_string());
            }
        } else if is_cypress_test_path(rel_str) {
            if !parsed.markers.iter().any(|m| m == "cypress") {
                parsed.markers.push("cypress".to_string());
            }
            if !parsed.module_markers.iter().any(|m| m == "e2e") {
                parsed.module_markers.push("e2e".to_string());
            }
        }
        let is_conftest = is_frontend_setup_file(rel_str);
        let is_test = !is_conftest
            && (is_frontend_test_path(rel_str)
                || parsed.markers.iter().any(|m| m == "in_source_vitest")
                || (is_storybook_story_path(rel_str)
                    && parsed.markers.iter().any(|m| m == "storybook_play")));
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: compact_semantic,
            api_semantic_sha256: String::new(),
            stamp,
            is_test,
            is_conftest,
            is_config: false,
            parsed,
        };
    }
    if is_stylesheet_path(rel_str) {
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_css_source_for_path(rel_str, &source);
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: compact_semantic,
            api_semantic_sha256: String::new(),
            stamp,
            is_test: false,
            is_conftest: false,
            is_config: false,
            parsed,
        };
    }
    if is_polyglot_source_path(rel_str) {
        let source = String::from_utf8_lossy(bytes);
        let parsed = parse_polyglot_source(rel_str, &source);
        let is_test = is_polyglot_test_path(rel_str, &parsed);
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: compact_semantic,
            api_semantic_sha256: String::new(),
            stamp,
            is_test,
            is_conftest: false,
            is_config: false,
            parsed,
        };
    }
    if !file_name.ends_with(".py") {
        // Arbitrarily named base tsconfig files (e.g. `configs/base.json` or `configs/base.jsonc`)
        // may be referenced via `extends`; structurally parse JSON/JSONC files with `compilerOptions` or `extends`.
        // Also parse C `#include` / `__has_include` AND Fortran `INCLUDE '...'` directives in
        // nonstandard/extensionless header files (e.g. `config.inc`, `myhdr`) on every update.
        let parsed = if file_name.ends_with(".json") || file_name.ends_with(".jsonc") {
            let text = String::from_utf8_lossy(bytes);
            if text.contains("\"compilerOptions\"") || text.contains("\"extends\"") {
                parse_tsconfig_jsonc(&text)
            } else {
                ParsedPythonFile::default()
            }
        } else if let Ok(text) = std::str::from_utf8(bytes) {
            let mut imps = BTreeSet::new();
            let mut syms = BTreeSet::new();
            if text.contains('#') || text.contains("__has_include") {
                for (hdr, is_opt) in extract_c_includes_from_source(text) {
                    syms.insert(if is_opt {
                        format!("c_include_opt:{hdr}")
                    } else {
                        format!("c_include:{hdr}")
                    });
                    imps.insert(ImportSpec {
                        level: 0,
                        module: hdr,
                        symbols: Vec::new(),
                    });
                }
            }
            if c_source_has_runtime_io(text) {
                syms.insert("cgo_dynamic_io".to_string());
            }
            // Included Fortran/header fragments (`.inc`, `.fh`, `.finc`, `.h`, etc.) may contain
            // continued `INCLUDE`, `USE`, or runtime I/O/env/subprocess calls in either free-form
            // or fixed-form Fortran syntax: reconstruct continuations and scan both forms!
            let free_joined = join_fortran_continuations(text, false);
            scan_fortran_logical_source(&free_joined, &mut imps, &mut syms);
            let fixed_joined = join_fortran_continuations(text, true);
            if fixed_joined != free_joined {
                scan_fortran_logical_source(&fixed_joined, &mut imps, &mut syms);
            }
            ParsedPythonFile {
                imports: imps.into_iter().collect(),
                defined_symbols: syms.into_iter().collect(),
                ..ParsedPythonFile::default()
            }
        } else {
            ParsedPythonFile::default()
        };
        return FileNode {
            path: rel_str.to_string(),
            sha256: sha,
            semantic_sha256: String::new(),
            api_semantic_sha256: String::new(),
            stamp,
            is_test: false,
            is_conftest: false,
            is_config: false,
            parsed,
        };
    }
    let source = String::from_utf8_lossy(bytes);
    let parsed = parse_python_source_with_patterns(&source, test_function_patterns);
    let is_conftest = file_name == "conftest.py";
    let is_test = !is_conftest
        && matches_test_filename(file_name, test_file_patterns)
        && matches_test_path(rel_str, testpaths);
    FileNode {
        path: rel_str.to_string(),
        sha256: sha,
        semantic_sha256: compact_semantic,
        api_semantic_sha256: String::new(),
        stamp,
        is_test,
        is_conftest,
        is_config: false,
        parsed,
    }
}

pub(super) fn js_source_has_stack_inspection(source: &str) -> bool {
    (source.contains("Error") && source.contains(".stack"))
        || source.contains("captureStackTrace")
        || source.contains("prepareStackTrace")
}

/// Computes a comment- and non-semantic-whitespace-normalized SHA-256 hash for source files
/// so comment-only or blank-line formatting edits do not trigger false test cache invalidations.
/// Preserves pragma/directive comments (`noqa`, `type:`, `pragma`, `eslint`, `@ts-`, `leaf_delta_nonce`, `bg_spec_nonce`, Rust doctest fences).
pub fn semantic_sha256_for_source(rel_path: &str, bytes: &[u8]) -> String {
    let is_py = rel_path.ends_with(".py");
    // Vue/Svelte SFCs, JSX/TSX markup files, and indented `.sass` files (where `//` comments out
    // indented child blocks) cannot be safely line-comment-stripped without a full parser; hash
    // them verbatim by raw SHA-256.
    let is_raw_hashed_frontend = rel_path.ends_with(".vue")
        || rel_path.ends_with(".svelte")
        || rel_path.ends_with(".tsx")
        || rel_path.ends_with(".jsx")
        || rel_path.ends_with(".sass")
        || rel_path.ends_with(".styl")
        || rel_path.ends_with(".stylus");
    // C/C++/ObjC files support preprocessor line splicing (`// comment \`) that alters whether
    // the following line is active code; hash them verbatim by raw SHA-256.
    let is_c_style = !is_raw_hashed_frontend
        && (rel_path.ends_with(".rs")
            || is_frontend_source_path(rel_path)
            || is_stylesheet_path(rel_path)
            || rel_path.ends_with(".go")
            || rel_path.ends_with(".cs")
            || rel_path.ends_with(".java")
            || rel_path.ends_with(".kt"));
    if !is_py && !is_c_style {
        return sha256_hex(bytes);
    }
    let Ok(source) = std::str::from_utf8(bytes) else {
        return sha256_hex(bytes);
    };
    // Go files using cgo (`import "C"` or raw-string ``import `C` ``) compile preceding `/* ... */`
    // or `// ...` comments as C preambles; hash them verbatim by raw SHA-256 so preamble edits
    // always invalidate cache.
    if rel_path.ends_with(".go") && (source.contains("\"C\"") || source.contains("`C`")) {
        return sha256_hex(bytes);
    }
    // Preserve exact line/column numbers for source files that inspect their own source location
    // at compile/runtime (Rust `line!()`/`column!()`/`file!()`, Go `Caller`/`Callers`, or
    // Python `inspect.currentframe`/`stack`/`getsource`/`sys._getframe`).
    if (rel_path.ends_with(".rs") && rust_source_has_location_macro(source))
        || (rel_path.ends_with(".go") && go_source_has_caller_inspection(source))
        || (is_py && python_source_has_location_inspection(source))
        || (is_frontend_source_path(rel_path) && js_source_has_stack_inspection(source))
    {
        return sha256_hex(bytes);
    }
    let mut hasher = Sha256::new();
    if is_py {
        hash_python_semantic_into(source, &mut hasher);
    } else {
        hash_c_style_semantic_into(
            source,
            rel_path.ends_with(".css"),
            is_stylesheet_path(rel_path),
            is_frontend_source_path(rel_path),
            rel_path.ends_with(".rs"),
            rel_path.ends_with(".go"),
            &mut hasher,
        );
    }
    hex_encode(&hasher.finalize())
}

pub(super) fn is_preserved_comment_directive(comment: &str) -> bool {
    let c = comment.trim();
    c.contains("noqa")
        || c.contains("type:")
        || c.contains("pragma")
        || c.contains("eslint")
        || c.contains("@ts-")
        || c.contains("biome-")
        || c.contains("webpack")
        || c.contains("vite-ignore")
        || c.contains("@__PURE__")
        || c.contains("#__PURE__")
        || c.contains("@__NO_SIDE_EFFECTS__")
        || c.contains("#__NO_SIDE_EFFECTS__")
        || c.contains("istanbul")
        || c.contains("c8 ignore")
        || c.contains("v8 ignore")
        || c.contains("turbotrace")
        || c.contains("deno-")
        || c.contains("//go:")
        || c.contains("go:embed")
        || c.contains("go:build")
        || c.contains("+build")
        || c.contains("#cgo")
        || c.contains("#include")
        || c.starts_with("//export ")
        || c.starts_with("//line ")
        || c.starts_with("/*line ")
        || c.contains("leaf_delta_nonce")
        || c.contains("bg_spec_nonce")
        || c.contains("```")
}

/// Splits Python source text on `\r\n`, `\n`, and bare `\r` (Python universal newlines)
/// so bare-CR files (`x=1 # note\ry=2`) never swallow code on the line following `#`.
pub(super) fn split_python_lines(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\r' {
            lines.push(&source[start..i]);
            i += if i + 1 < len && bytes[i + 1] == b'\n' { 2 } else { 1 };
            start = i;
        } else if bytes[i] == b'\n' {
            lines.push(&source[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < len {
        lines.push(&source[start..len]);
    }
    lines
}

pub(super) fn hash_python_semantic_into(source: &str, hasher: &mut Sha256) {
    let mut line_out = String::with_capacity(256);
    let mut in_triple: Option<&str> = None;
    let mut in_single: Option<u8> = None;
    let mut verbatim_tail = false;
    let mut prev_ended_with_continuation = false;
    for (line_idx, raw_line) in split_python_lines(source).into_iter().enumerate() {
        if verbatim_tail {
            hasher.update(raw_line.as_bytes());
            hasher.update(b"\n");
            continue;
        }
        // Preserve PEP 263 encoding declarations on comment lines 1–2 (e.g. `# coding: latin-1`),
        // never bypassing lexical state on non-comment code lines like `coding = """...`.
        if line_idx < 2
            && in_triple.is_none()
            && in_single.is_none()
            && raw_line.trim_start().starts_with('#')
            && raw_line.contains("coding")
        {
            hasher.update(raw_line.as_bytes());
            hasher.update(b"\n");
            prev_ended_with_continuation = false;
            continue;
        }
        let must_preserve_continuation_target = prev_ended_with_continuation;
        let started_in_literal = in_triple.is_some() || in_single.is_some();
        let bytes = raw_line.as_bytes();
        if !started_in_literal && memchr::memchr3(b'#', b'"', b'\'', bytes).is_none() {
            // Trim ONLY Python-permitted horizontal ASCII whitespace so invalid Unicode whitespace
            // (e.g. U+00A0 NBSP causing SyntaxError) is never stripped and collided with valid code.
            let trimmed_end = raw_line.trim_end_matches([' ', '\t', '\x0c', '\r']);
            let ends_with_backslash = trimmed_end.ends_with('\\');
            if must_preserve_continuation_target
                || !trimmed_end.trim_matches([' ', '\t', '\x0c', '\r']).is_empty()
            {
                // In Python, trailing whitespace after a line-continuation `\` is a SyntaxError,
                // and a blank/comment line immediately after `\` also changes syntax validity.
                let emit = if ends_with_backslash || must_preserve_continuation_target {
                    raw_line
                } else {
                    trimmed_end
                };
                hasher.update(emit.as_bytes());
                hasher.update(b"\n");
            }
            prev_ended_with_continuation = ends_with_backslash && raw_line.ends_with('\\');
            continue;
        }
        line_out.clear();
        let mut i = 0usize;
        let mut line_continued_in_single = false;
        while i < bytes.len() {
            if let Some(delim) = in_triple {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    line_out.push('\\');
                    if let Some(esc_ch) = raw_line[i + 1..].chars().next() {
                        line_out.push(esc_ch);
                        i += 1 + esc_ch.len_utf8();
                        continue;
                    }
                }
                if raw_line[i..].starts_with(delim) {
                    line_out.push_str(delim);
                    i += 3;
                    in_triple = None;
                } else {
                    let ch = raw_line[i..].chars().next().unwrap_or(' ');
                    line_out.push(ch);
                    i += ch.len_utf8();
                }
                continue;
            }
            if let Some(q) = in_single {
                if bytes[i] == b'\\' {
                    line_out.push('\\');
                    if i + 1 < bytes.len() {
                        if let Some(esc_ch) = raw_line[i + 1..].chars().next() {
                            line_out.push(esc_ch);
                            i += 1 + esc_ch.len_utf8();
                            continue;
                        }
                    } else {
                        // Trailing unescaped backslash continues a single-quoted string across newline.
                        line_continued_in_single = true;
                        i += 1;
                        continue;
                    }
                }
                if bytes[i] == q {
                    in_single = None;
                }
                let ch = raw_line[i..].chars().next().unwrap_or(' ');
                line_out.push(ch);
                i += ch.len_utf8();
                continue;
            }
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                // Python 3.12+ (PEP 701) allows arbitrary multiline expressions, comments, and
                // same-quote nesting inside both single- and triple-quoted f-strings (`f"..."`,
                // `f"""..."""`, `rf"..."`, `fr"..."`). Switch to verbatim hashing for the rest
                // of the file upon encountering any f-string prefix so no collision is possible.
                let is_fstring_prefix = i > 0
                    && ((matches!(bytes[i - 1], b'f' | b'F')
                        && (i == 1
                            || !(bytes[i - 2].is_ascii_alphanumeric() || bytes[i - 2] == b'_')
                            || (matches!(bytes[i - 2], b'r' | b'R')
                                && (i == 2
                                    || !(bytes[i - 3].is_ascii_alphanumeric()
                                        || bytes[i - 3] == b'_')))))
                        || (i > 1
                            && matches!(bytes[i - 1], b'r' | b'R')
                            && matches!(bytes[i - 2], b'f' | b'F')
                            && (i == 2
                                || !(bytes[i - 3].is_ascii_alphanumeric()
                                    || bytes[i - 3] == b'_'))));
                if is_fstring_prefix {
                    line_out.push_str(&raw_line[i..]);
                    verbatim_tail = true;
                    break;
                }
            }
            if raw_line[i..].starts_with("\"\"\"") {
                in_triple = Some("\"\"\"");
                line_out.push_str("\"\"\"");
                i += 3;
                continue;
            }
            if raw_line[i..].starts_with("'''") {
                in_triple = Some("'''");
                line_out.push_str("'''");
                i += 3;
                continue;
            }
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                in_single = Some(bytes[i]);
                line_out.push(bytes[i] as char);
                i += 1;
                continue;
            }
            if bytes[i] == b'#' {
                let comment = &raw_line[i..];
                // In Python, a comment immediately following a continuation `\` (e.g. `x = 1 \#c`)
                // or on a continuation-target line changes syntax validity; preserve it verbatim.
                if must_preserve_continuation_target
                    || line_out
                        .trim_end_matches([' ', '\t', '\x0c', '\r'])
                        .ends_with('\\')
                    || is_preserved_comment_directive(comment)
                {
                    line_out.push_str(comment.trim_end_matches([' ', '\t', '\x0c', '\r']));
                }
                break;
            }
            let ch = raw_line[i..].chars().next().unwrap_or(' ');
            line_out.push(ch);
            i += ch.len_utf8();
        }
        // Only preserve `in_single` across a line boundary if the line ended with an unescaped `\`.
        if in_single.is_some() && !line_continued_in_single {
            in_single = None;
        }
        let ends_with_cont =
            in_triple.is_none() && in_single.is_none() && line_out.ends_with('\\');
        // Inside multiline string literals, continuation targets, or verbatim f-string tail,
        // preserve every byte verbatim.
        if verbatim_tail
            || must_preserve_continuation_target
            || started_in_literal
            || in_triple.is_some()
            || in_single.is_some()
        {
            hasher.update(line_out.as_bytes());
            hasher.update(b"\n");
        } else {
            let trimmed_end = line_out.trim_end_matches([' ', '\t', '\x0c', '\r']);
            if !trimmed_end.trim_matches([' ', '\t', '\x0c', '\r']).is_empty() {
                let emit = if trimmed_end.ends_with('\\') {
                    line_out.as_str()
                } else {
                    trimmed_end
                };
                hasher.update(emit.as_bytes());
                hasher.update(b"\n");
            }
        }
        prev_ended_with_continuation = ends_with_cont;
    }
}

pub(super) fn flush_semantic_line(line_buf: &mut String, hasher: &mut Sha256) {
    // Trim ONLY ASCII space/tab/CR so invalid Unicode whitespace (e.g. U+0085 NEL in JS/Rust/CSS)
    // is never stripped and collided with valid syntax.
    let trimmed_end = line_buf.trim_end_matches([' ', '\t', '\r']);
    if !trimmed_end.trim_matches([' ', '\t', '\r']).is_empty() {
        hasher.update(trimmed_end.as_bytes());
        hasher.update(b"\n");
    }
    line_buf.clear();
}

pub(super) fn flush_literal_line(line_buf: &mut String, hasher: &mut Sha256) {
    hasher.update(line_buf.as_bytes());
    hasher.update(b"\n");
    line_buf.clear();
}

pub(super) fn match_rust_raw_string_start(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    // Matches `r"`, `r#"`, `br"`, `br#"`, `cr"`, `cr#"` at an identifier boundary
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        return None;
    }
    let len = bytes.len();
    let r_pos = if bytes[i] == b'r' {
        i
    } else if matches!(bytes[i], b'b' | b'c') && i + 1 < len && bytes[i + 1] == b'r' {
        i + 1
    } else {
        return None;
    };
    let mut p = r_pos + 1;
    while p < len && bytes[p] == b'#' {
        p += 1;
    }
    if p < len && bytes[p] == b'"' {
        Some((p + 1, p - (r_pos + 1)))
    } else {
        None
    }
}

pub(super) fn match_rust_char_literal(source: &str, i: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    if i >= len || bytes[i] != b'\'' {
        return None;
    }
    let mut p = i + 1;
    if p >= len {
        return None;
    }
    if bytes[p] == b'\\' {
        p += 1;
        if p >= len {
            return None;
        }
        if bytes[p] == b'u' && p + 1 < len && bytes[p + 1] == b'{' {
            p += 2;
            while p < len && bytes[p] != b'}' && bytes[p] != b'\n' {
                p += 1;
            }
            if p < len && bytes[p] == b'}' {
                p += 1;
            }
        } else if bytes[p] == b'x' && p + 2 < len {
            p += 3;
        } else {
            p += source[p..].chars().next().map_or(1, char::len_utf8);
        }
    } else if bytes[p] != b'\'' && bytes[p] != b'\n' && bytes[p] != b'\r' {
        p += source[p..].chars().next().map_or(1, char::len_utf8);
    } else {
        return None;
    }
    if p < len && bytes[p] == b'\'' {
        Some(p + 1)
    } else {
        None
    }
}

pub(super) fn hash_c_style_semantic_into(
    source: &str,
    is_plain_css: bool,
    is_stylesheet: bool,
    is_js_like: bool,
    is_rust: bool,
    is_go: bool,
    hasher: &mut Sha256,
) {
    let mut line_buf = String::with_capacity(256);
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut in_quote: Option<u8> = None;
    let mut in_rust_raw: Option<usize> = None;
    let mut block_comment_depth = 0usize;
    let mut block_comment_is_doc = false;
    let mut in_css_url = false;

    while i < len {
        if block_comment_depth > 0 {
            if is_rust && bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
                block_comment_depth += 1;
                if block_comment_is_doc {
                    line_buf.push_str("/*");
                }
                i += 2;
            } else if bytes[i] == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                block_comment_depth -= 1;
                if block_comment_is_doc {
                    line_buf.push_str("*/");
                }
                i += 2;
                if block_comment_depth == 0 {
                    block_comment_is_doc = false;
                    // Emit a token separator when closing a block comment so `return/**/foo`
                    // never collides with `returnfoo`. In CSS/SCSS, emit a non-whitespace sentinel
                    // (`\x1f`) so `.a/**/.b` (compound selector) NEVER collides with `.a .b`
                    // (descendant combinator).
                    if !line_buf.is_empty() && !line_buf.ends_with(char::is_whitespace) {
                        line_buf.push(if is_stylesheet { '\x1f' } else { ' ' });
                    }
                }
            } else if bytes[i] == b'\n' {
                if block_comment_is_doc || (is_stylesheet && line_buf.contains('\\')) {
                    flush_literal_line(&mut line_buf, hasher);
                } else {
                    flush_semantic_line(&mut line_buf, hasher);
                }
                i += 1;
            } else if bytes[i] == b'\r' && i + 1 < len && bytes[i + 1] == b'\n' {
                if block_comment_is_doc || (is_stylesheet && line_buf.contains('\\')) {
                    flush_literal_line(&mut line_buf, hasher);
                } else {
                    flush_semantic_line(&mut line_buf, hasher);
                }
                i += 2;
            } else if is_js_like && bytes[i] == b'\r' {
                flush_semantic_line(&mut line_buf, hasher);
                i += 1;
            } else if is_js_like
                && bytes[i] == 0xE2
                && i + 2 < len
                && bytes[i + 1] == 0x80
                && matches!(bytes[i + 2], 0xA8 | 0xA9)
            {
                flush_semantic_line(&mut line_buf, hasher);
                i += 3;
            } else {
                let ch = source[i..].chars().next().unwrap_or(' ');
                if block_comment_is_doc {
                    line_buf.push(ch);
                }
                i += ch.len_utf8();
            }
            continue;
        }
        if let Some(hashes) = in_rust_raw {
            if bytes[i] == b'\n' {
                flush_literal_line(&mut line_buf, hasher);
                i += 1;
                continue;
            }
            if bytes[i] == b'\r' && i + 1 < len && bytes[i + 1] == b'\n' {
                flush_literal_line(&mut line_buf, hasher);
                i += 2;
                continue;
            }
            if bytes[i] == b'"'
                && i + 1 + hashes <= len
                && bytes[i + 1..i + 1 + hashes].iter().all(|&b| b == b'#')
            {
                line_buf.push_str(&source[i..i + 1 + hashes]);
                i += 1 + hashes;
                in_rust_raw = None;
                continue;
            }
            let ch = source[i..].chars().next().unwrap_or(' ');
            line_buf.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if let Some(q) = in_quote {
            if is_js_like && q == b'`' && bytes[i] == b'$' && i + 1 < len && bytes[i + 1] == b'{' {
                // JS/TS template interpolation `${...}` can contain arbitrary nested comments,
                // regex literals, and templates; hash the remainder verbatim so zero collisions occur.
                hasher.update(line_buf.as_bytes());
                hasher.update(&bytes[i..]);
                return;
            }
            // Go backtick raw strings (`\`) have NO backslash escape sequences.
            if !(is_go && q == b'`') && bytes[i] == b'\\' && i + 1 < len {
                line_buf.push('\\');
                if let Some(esc_ch) = source[i + 1..].chars().next() {
                    if esc_ch == '\n' {
                        flush_literal_line(&mut line_buf, hasher);
                    } else if esc_ch == '\r' && i + 2 < len && bytes[i + 2] == b'\n' {
                        flush_literal_line(&mut line_buf, hasher);
                        i += 3;
                        continue;
                    } else {
                        line_buf.push(esc_ch);
                    }
                    i += 1 + esc_ch.len_utf8();
                    continue;
                }
            }
            if bytes[i] == b'\n' {
                flush_literal_line(&mut line_buf, hasher);
                i += 1;
                continue;
            }
            if bytes[i] == b'\r' && i + 1 < len && bytes[i + 1] == b'\n' {
                flush_literal_line(&mut line_buf, hasher);
                i += 2;
                continue;
            }
            if bytes[i] == q {
                in_quote = None;
            }
            let ch = source[i..].chars().next().unwrap_or(' ');
            line_buf.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if is_stylesheet && in_css_url {
            if bytes[i] == b'\\' && i + 1 < len {
                // Consume CSS escapes inside unquoted `url(...)` so `\)` never closes the URL prematurely.
                line_buf.push('\\');
                if let Some(esc_ch) = source[i + 1..].chars().next() {
                    if esc_ch == '\n' {
                        flush_literal_line(&mut line_buf, hasher);
                    } else if esc_ch == '\r' && i + 2 < len && bytes[i + 2] == b'\n' {
                        flush_literal_line(&mut line_buf, hasher);
                        i += 3;
                        continue;
                    } else {
                        line_buf.push(esc_ch);
                    }
                    i += 1 + esc_ch.len_utf8();
                    continue;
                }
            }
            if bytes[i] == b')' {
                in_css_url = false;
                line_buf.push(')');
                i += 1;
                continue;
            }
        }
        if is_stylesheet
            && !in_css_url
            && (bytes[i] == b'u' || bytes[i] == b'U')
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'-'
                    || bytes[i - 1] >= 0x80))
            && i + 4 <= len
            && bytes[i..i + 4].eq_ignore_ascii_case(b"url(")
        {
            in_css_url = true;
            line_buf.push_str(&source[i..i + 4]);
            i += 4;
            continue;
        }
        if is_rust
            && matches!(bytes[i], b'r' | b'b' | b'c')
            && let Some((next_i, hashes)) = match_rust_raw_string_start(bytes, i)
        {
            line_buf.push_str(&source[i..next_i]);
            i = next_i;
            in_rust_raw = Some(hashes);
            continue;
        }
        if is_rust && bytes[i] == b'\'' {
            if let Some(next_i) = match_rust_char_literal(source, i) {
                line_buf.push_str(&source[i..next_i]);
                i = next_i;
            } else {
                // Rust lifetime token (e.g. `'a`, `'static`): never enter string-quote mode.
                line_buf.push('\'');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'\n' {
            // In CSS/SCSS, a trailing space after a hex escape (`.\31 \n.foo`) terminates the
            // escape while the newline acts as a descendant combinator; preserve lines containing `\`.
            if is_stylesheet && line_buf.contains('\\') {
                flush_literal_line(&mut line_buf, hasher);
            } else {
                flush_semantic_line(&mut line_buf, hasher);
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'\r' && i + 1 < len && bytes[i + 1] == b'\n' {
            if is_stylesheet && line_buf.contains('\\') {
                flush_literal_line(&mut line_buf, hasher);
            } else {
                flush_semantic_line(&mut line_buf, hasher);
            }
            i += 2;
            continue;
        }
        if is_js_like && bytes[i] == b'\r' {
            flush_semantic_line(&mut line_buf, hasher);
            i += 1;
            continue;
        }
        if is_js_like
            && bytes[i] == 0xE2
            && i + 2 < len
            && bytes[i + 1] == 0x80
            && matches!(bytes[i + 2], 0xA8 | 0xA9)
        {
            flush_semantic_line(&mut line_buf, hasher);
            i += 3;
            continue;
        }
        // Consume CSS `\` escapes outside strings/urls BEFORE checking `/*` or `//` so escaped
        // slashes like `--v: \/*a*/red;` are hashed literally instead of starting a comment.
        if is_stylesheet && !in_css_url && bytes[i] == b'\\' && i + 1 < len {
            line_buf.push('\\');
            if let Some(esc_ch) = source[i + 1..].chars().next() {
                if esc_ch == '\n' {
                    flush_literal_line(&mut line_buf, hasher);
                } else if esc_ch == '\r' && i + 2 < len && bytes[i + 2] == b'\n' {
                    flush_literal_line(&mut line_buf, hasher);
                    i += 3;
                    continue;
                } else {
                    line_buf.push(esc_ch);
                }
                i += 1 + esc_ch.len_utf8();
                continue;
            }
        }
        // Plain CSS has no `//` comments, and SCSS/LESS `url(http://...)` must not treat `//` as a comment.
        if !is_plain_css
            && !in_css_url
            && bytes[i] == b'/'
            && i + 1 < len
            && bytes[i + 1] == b'/'
        {
            // ECMAScript line comments end at LF, CR, U+2028 (LS), or U+2029 (PS); other C-style
            // languages end at LF.
            let line_end = if is_js_like {
                find_js_line_terminator(bytes, i + 2)
            } else {
                memchr::memchr(b'\n', &bytes[i..])
                    .map(|off| i + off)
                    .unwrap_or(len)
            };
            let comment = &source[i..line_end];
            // In Rust, `///` (not `////`) and `//!` are semantic `#[doc = ...]` attributes consumed
            // at runtime by derive macros (e.g. `clap::Parser` help text) and doctests; preserve
            // them verbatim including trailing spaces (which are meaningful Markdown hard breaks).
            let is_rust_line_doc = is_rust
                && i + 2 < line_end
                && ((bytes[i + 2] == b'!')
                    || (bytes[i + 2] == b'/' && bytes.get(i + 3) != Some(&b'/')));
            if is_rust_line_doc {
                line_buf.push_str(comment.trim_end_matches('\r'));
                flush_literal_line(&mut line_buf, hasher);
                i = if line_end < len && bytes[line_end] == b'\n' {
                    line_end + 1
                } else {
                    line_end
                };
                continue;
            }
            if is_preserved_comment_directive(comment) {
                line_buf.push_str(comment.trim_end());
            }
            i = line_end;
            continue;
        }
        // Unquoted CSS `url(foo/*bar*/.png)` treats `/*` as literal URL characters, not a block comment.
        if !in_css_url && bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            // Preserve semantic bundler/linter block-comment directives (e.g. `/* webpackIgnore: true */`,
            // `/* @vite-ignore */`, `/* @__PURE__ */`, `/* eslint-disable */`) verbatim.
            if !is_rust
                && let Some(close_rel) = source[i + 2..].find("*/")
            {
                let close_end = i + 2 + close_rel + 2;
                let comment_slice = &source[i..close_end];
                if is_preserved_comment_directive(comment_slice) {
                    line_buf.push_str(comment_slice);
                    i = close_end;
                    continue;
                }
            }
            block_comment_depth = 1;
            // In Rust, `/*!` and `/**` (excluding `/***` and empty `/**/`) are semantic `#[doc = ...]`.
            block_comment_is_doc = is_rust
                && i + 2 < len
                && (bytes[i + 2] == b'!'
                    || (bytes[i + 2] == b'*'
                        && i + 3 < len
                        && bytes[i + 3] != b'*'
                        && bytes[i + 3] != b'/'));
            if block_comment_is_doc {
                line_buf.push_str("/*");
            }
            i += 2;
            continue;
        }
        // In JS/TS, any non-comment `/` (potential regex literal containing quotes like `/"/` or
        // character classes like `/[//]/`) can shift lexer state without a full ECMAScript parser.
        // Hash the remainder of the file verbatim so zero semantic-hash collisions can occur.
        if is_js_like && bytes[i] == b'/' {
            hasher.update(line_buf.as_bytes());
            hasher.update(&bytes[i..]);
            return;
        }
        if matches!(bytes[i], b'"' | b'\'' | b'`') {
            in_quote = Some(bytes[i]);
            line_buf.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let next_special = bytes[i..]
            .iter()
            .position(|&b| {
                matches!(b, b'/' | b'"' | b'\'' | b'`' | b'\n' | b'\r')
                    || (is_stylesheet && matches!(b, b'u' | b'U' | b')' | b'\\'))
                    || (is_rust && matches!(b, b'r' | b'b' | b'c'))
            })
            .map(|off| i + off)
            .unwrap_or(len);
        if next_special > i {
            line_buf.push_str(&source[i..next_special]);
            i = next_special;
        } else {
            let ch = source[i..].chars().next().unwrap_or(' ');
            line_buf.push(ch);
            i += ch.len_utf8();
        }
    }
    flush_semantic_line(&mut line_buf, hasher);
}

pub(super) fn find_js_line_terminator(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let len = bytes.len();
    while i < len {
        match bytes[i] {
            b'\n' | b'\r' => return i,
            0xE2 if i + 2 < len
                && bytes[i + 1] == 0x80
                && matches!(bytes[i + 2], 0xA8 | 0xA9) =>
            {
                return i;
            }
            _ => i += 1,
        }
    }
    len
}

pub(super) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![0u8; bytes.len() * 2];
    for (idx, &b) in bytes.iter().enumerate() {
        out[idx * 2] = HEX[(b >> 4) as usize];
        out[idx * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    String::from_utf8(out).unwrap_or_default()
}


