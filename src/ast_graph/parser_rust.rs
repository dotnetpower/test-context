use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RsToken<'a> {
    Ident(&'a str),
    StringLit(String),
    InvalidEscapeString,
    ColonColon,
    Dot,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Eq,
    Hash,
    Bang,
    Star,
    Other,
}

pub(super) fn push_rs_other(tokens: &mut Vec<RsToken<'_>>) {
    if !matches!(tokens.last(), Some(RsToken::Other)) {
        tokens.push(RsToken::Other);
    }
}

pub(super) fn tokenize_rust(source: &str) -> Vec<RsToken<'_>> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::with_capacity((len / 6).clamp(16, 65_536));

    while i < len {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                i = memchr::memchr(b'\n', &bytes[i + 2..])
                    .map(|off| i + 2 + off)
                    .unwrap_or(len);
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
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
            }
            b':' if i + 1 < len && bytes[i + 1] == b':' => {
                tokens.push(RsToken::ColonColon);
                i += 2;
            }
            b'{' => {
                tokens.push(RsToken::LBrace);
                i += 1;
            }
            b'}' => {
                tokens.push(RsToken::RBrace);
                i += 1;
            }
            b'(' => {
                tokens.push(RsToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(RsToken::RParen);
                i += 1;
            }
            b'[' => {
                tokens.push(RsToken::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(RsToken::RBracket);
                i += 1;
            }
            b';' => {
                tokens.push(RsToken::Semi);
                i += 1;
            }
            b',' => {
                tokens.push(RsToken::Comma);
                i += 1;
            }
            b'=' => {
                if i + 1 < len && (bytes[i + 1] == b'=' || bytes[i + 1] == b'>') {
                    push_rs_other(&mut tokens);
                    i += 2;
                } else {
                    tokens.push(RsToken::Eq);
                    i += 1;
                }
            }
            b'#' => {
                tokens.push(RsToken::Hash);
                i += 1;
            }
            b'!' => {
                if i + 1 < len && bytes[i + 1] == b'=' {
                    push_rs_other(&mut tokens);
                    i += 2;
                } else {
                    tokens.push(RsToken::Bang);
                    i += 1;
                }
            }
            b'.' => {
                tokens.push(RsToken::Dot);
                i += 1;
            }
            b'*' => {
                tokens.push(RsToken::Star);
                i += 1;
            }
            b'"' => {
                let (raw_lit, next_i) = scan_rust_string(source, i);
                match decode_rust_string_lit(&raw_lit) {
                    Ok(decoded) => tokens.push(RsToken::StringLit(decoded)),
                    Err(()) => tokens.push(RsToken::InvalidEscapeString),
                }
                i = next_i;
            }
            b'\'' => {
                // Distinguish character literals ('a', '\n', '¥') from lifetimes ('a, 'static)
                if i + 1 < len && bytes[i + 1] == b'\\' {
                    i += 2;
                    while i < len && bytes[i] != b'\'' && bytes[i] != b'\n' {
                        i += 1;
                    }
                    if i < len && bytes[i] == b'\'' {
                        i += 1;
                    }
                    push_rs_other(&mut tokens);
                } else if i + 1 < len
                    && let Some(ch) = source[i + 1..].chars().next()
                    && i + 1 + ch.len_utf8() < len
                    && bytes[i + 1 + ch.len_utf8()] == b'\''
                {
                    i += 1 + ch.len_utf8() + 1;
                    push_rs_other(&mut tokens);
                } else {
                    i += 1;
                    while i < len && is_ident_continue(bytes[i]) {
                        i += 1;
                    }
                    push_rs_other(&mut tokens);
                }
            }
            _ if is_ident_start(b) => {
                // Check for raw string literal `r"..."`, `r#"..."#`, `br"..."`, `br#"..."#` or byte string `b"..."`
                if let Some((lit, next_i)) = try_scan_rust_raw_or_byte_string(source, i) {
                    tokens.push(RsToken::StringLit(lit));
                    i = next_i;
                    continue;
                }
                let start = i;
                i += 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                tokens.push(RsToken::Ident(&source[start..i]));
            }
            _ => {
                push_rs_other(&mut tokens);
                i += 1;
            }
        }
    }
    tokens
}

pub(super) fn scan_rust_string(source: &str, quote_idx: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let start = quote_idx + 1;
    let mut i = start;
    while i < len {
        let Some(off) = memchr::memchr2(b'"', b'\\', &bytes[i..]) else {
            break;
        };
        i += off;
        if bytes[i] == b'\\' {
            i += 1;
            if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
            continue;
        }
        return (source[start..i].to_string(), i + 1);
    }
    (source[start..len].to_string(), len)
}

pub(super) fn try_scan_rust_raw_or_byte_string(source: &str, start_idx: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = start_idx;
    if bytes[i] == b'b' {
        if i + 1 < len && bytes[i + 1] == b'"' {
            return Some(scan_rust_string(source, i + 1));
        }
        if i + 1 < len && bytes[i + 1] == b'r' {
            i += 2;
        } else {
            return None;
        }
    } else if bytes[i] == b'r' {
        i += 1;
    } else {
        return None;
    }
    let mut hashes = 0usize;
    while i < len && bytes[i] == b'#' {
        hashes += 1;
        i += 1;
    }
    if i >= len || bytes[i] != b'"' {
        return None;
    }
    let content_start = i + 1;
    i = content_start;
    while i < len {
        if bytes[i] == b'"' {
            let mut matched = true;
            for h in 0..hashes {
                if i + 1 + h >= len || bytes[i + 1 + h] != b'#' {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Some((source[content_start..i].to_string(), i + 1 + hashes));
            }
        }
        i += 1;
    }
    Some((source[content_start..len].to_string(), len))
}

/// Decodes standard Rust string literal escape sequences (`\n`, `\r`, `\t`, `\\`, `\0`, `\'`,
/// `\"`, `\xHH` ASCII, `\u{HHHH}`, and `\<newline><ws>` line continuations). Returns `Err(())`
/// for invalid or non-ASCII `\xHH` escapes so callers fail closed instead of tracking wrong keys.
pub(super) fn decode_rust_string_lit(raw: &str) -> std::result::Result<String, ()> {
    if !raw.contains('\\') {
        return Ok(raw.to_string());
    }
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' && i + 1 < len {
            match bytes[i + 1] {
                b'n' => {
                    out.push('\n');
                    i += 2;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                }
                b'0' => {
                    out.push('\0');
                    i += 2;
                }
                b'\\' | b'"' | b'\'' => {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                }
                b'\n' => {
                    i += 2;
                    while i < len && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
                        i += 1;
                    }
                }
                b'\r' if i + 2 < len && bytes[i + 2] == b'\n' => {
                    i += 3;
                    while i < len && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
                        i += 1;
                    }
                }
                b'x' if i + 3 < len => {
                    let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).map_err(|_| ())?;
                    let code = u8::from_str_radix(hex, 16).map_err(|_| ())?;
                    if code > 0x7F {
                        return Err(());
                    }
                    out.push(code as char);
                    i += 4;
                }
                b'u' if i + 2 < len && bytes[i + 2] == b'{' => {
                    let start = i + 3;
                    let rel_end = raw[start..].find('}').ok_or(())?;
                    let hex: String = raw[start..start + rel_end]
                        .chars()
                        .filter(|&c| c != '_')
                        .collect();
                    let code = u32::from_str_radix(&hex, 16).map_err(|_| ())?;
                    let ch = char::from_u32(code).ok_or(())?;
                    out.push(ch);
                    i = start + rel_end + 1;
                }
                _ => return Err(()),
            }
            continue;
        }
        let ch = raw[i..].chars().next().ok_or(())?;
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

pub(super) fn normalize_include_ref(lit: &str) -> Option<String> {
    // On Unix, literal backslashes in `include_str!("data\\file.txt")` are part of the filename,
    // NOT directory separators; fail closed rather than rewriting them to `/`.
    #[cfg(unix)]
    if lit.contains('\\') {
        return None;
    }
    // Preserve literal leading/trailing spaces (e.g. `include_str!(" data.json ")`); only strip
    // leading `./` path prefixes.
    let mut normalized = lit.replace('\\', "/");
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest.to_string();
    }
    if normalized.is_empty()
        || normalized.len() > 260
        || normalized.starts_with('/')
        || normalized.contains(['\n', '\r', '\0'])
    {
        return None;
    }
    Some(normalized)
}

/// Resolves ALL candidate owning Cargo crate directories for a Rust source file (deepest
/// ancestor manifest first) so both conventional nested crates AND custom `[lib] path` targets
/// declared in an outer `Cargo.toml` are discovered.
pub fn owning_rust_crate_dirs(path: &str, files: &BTreeMap<String, FileNode>) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut cur = Path::new(path).parent();
    while let Some(dir) = cur {
        let dir_str = normalize_rel_str(dir);
        let manifest = if dir_str.is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{dir_str}/Cargo.toml")
        };
        if files.contains_key(&manifest) && !dirs.contains(&dir_str) {
            dirs.push(dir_str);
        }
        cur = dir.parent();
    }
    let fallback = rust_crate_dir(path);
    if dirs.is_empty() {
        dirs.push(fallback);
    }
    dirs
}

pub(super) fn is_valid_rust_env_key(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && !name.contains('=') && !name.contains('\0')
}

pub fn is_valid_env_var_name(name: &str) -> bool {
    if name != name.trim() {
        return false;
    }
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parses a Rust source file (`.rs`) to extract `mod` declarations, `use` imports, `#[test]` / `#[tokio::test]` symbols,
/// `#[ignore]` / `#[serial]` markers, `include_str!` / `include_bytes!` / `include!` data refs, and `env!` / `env::var` keys.
pub(super) fn is_explicit_static_fixture_data_ref(clean_ref: &str) -> bool {
    clean_ref.ends_with(".snap")
        || clean_ref.ends_with(".golden")
        || clean_ref.ends_with(".proto")
        || clean_ref.ends_with(".graphql")
        || clean_ref.ends_with(".gql")
        || clean_ref.ends_with(".sql")
        || clean_ref
            .split('/')
            .any(|seg| matches!(seg, "testdata" | "fixtures" | "fixture" | "snapshots" | "__snapshots__"))
}

pub fn parse_rust_source(source: &str) -> ParsedPythonFile {
    parse_rust_source_with_mode(source, true)
}

pub fn parse_rust_source_for_path(rel_path: &str, source: &str) -> ParsedPythonFile {
    let is_dedicated_test_or_build = is_rust_integration_test_path(rel_path)
        || is_rust_common_test_helper(rel_path)
        || rel_path == "build.rs"
        || rel_path.ends_with("/build.rs");
    parse_rust_source_with_mode(source, is_dedicated_test_or_build)
}

pub(super) fn parse_rust_source_with_mode(source: &str, capture_all_string_data_refs: bool) -> ParsedPythonFile {
    let tokens = tokenize_rust(source);
    let mut imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut markers = BTreeSet::new();
    let mut module_markers = BTreeSet::new();
    let mut rust_mods = BTreeSet::new();
    let mut env_vars = BTreeSet::new();
    // Pre-scan `use ...;` statements for:
    // 1. `as alias` renamings of include macros and `std::env` helpers
    // 2. Grouped or direct imports of `std::env::{var, var_os, vars, vars_os}`
    // 3. Re-exports (`pub use`) of include/env macros across modules (fail closed).
    let mut aliased_include_macros: BTreeMap<&str, bool> = BTreeMap::new(); // alias -> is_raw_embed
    let mut aliased_env_macros: BTreeSet<&str> = BTreeSet::new();
    let mut aliased_env_getters: BTreeSet<&str> = BTreeSet::new();
    let mut aliased_env_enums: BTreeSet<&str> = BTreeSet::new();
    let mut aliased_fs_mods: BTreeSet<&str> = BTreeSet::from(["fs", "fs_err"]);
    let mut aliased_file_types: BTreeSet<&str> = BTreeSet::from(["File", "OpenOptions"]);
    let mut aliased_open_options_types: BTreeSet<&str> = BTreeSet::from(["OpenOptions"]);
    let mut aliased_fs_fns: BTreeSet<&str> = BTreeSet::new();
    let mut aliased_fs_meta_fns: BTreeSet<&str> = BTreeSet::new();
    let mut in_use_stmt = false;
    let mut use_is_pub = false;
    let mut use_saw_env = false;
    let mut use_saw_fs = false;
    for idx in 0..tokens.len() {
        match &tokens[idx] {
            RsToken::Ident("use") => {
                in_use_stmt = true;
                use_saw_env = false;
                use_saw_fs = false;
                use_is_pub = idx > 0
                    && matches!(
                        tokens.get(idx - 1),
                        Some(RsToken::Ident("pub") | RsToken::RParen)
                    );
            }
            RsToken::Semi => {
                in_use_stmt = false;
                use_is_pub = false;
                use_saw_env = false;
                use_saw_fs = false;
            }
            RsToken::Ident(orig) if in_use_stmt => {
                if *orig == "env" {
                    use_saw_env = true;
                }
                if matches!(*orig, "fs" | "fs_err") {
                    use_saw_fs = true;
                }
                let next_is_as = matches!(tokens.get(idx + 1), Some(RsToken::Ident("as")));
                if next_is_as
                    && let Some(RsToken::Ident(alias)) = tokens.get(idx + 2)
                {
                    match *orig {
                        "include_str" | "include_bytes" => {
                            aliased_include_macros.insert(alias, true);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_include".to_string());
                            }
                        }
                        "include" => {
                            aliased_include_macros.insert(alias, false);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_include".to_string());
                            }
                        }
                        "env" | "option_env" => {
                            aliased_env_macros.insert(alias);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_env".to_string());
                            }
                        }
                        "var" | "var_os" => {
                            aliased_env_getters.insert(alias);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_env".to_string());
                            }
                        }
                        "vars" | "vars_os" => {
                            aliased_env_enums.insert(alias);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_env".to_string());
                            }
                        }
                        "fs" | "fs_err" => {
                            aliased_fs_mods.insert(alias);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_io".to_string());
                            }
                        }
                        "self" if use_saw_fs => {
                            aliased_fs_mods.insert(alias);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_io".to_string());
                            }
                        }
                        "File" | "OpenOptions" => {
                            aliased_file_types.insert(alias);
                            if *orig == "OpenOptions" {
                                aliased_open_options_types.insert(alias);
                            }
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_io".to_string());
                            }
                        }
                        "Command" => {
                            aliased_open_options_types.insert(alias);
                            defined_symbols.insert("rust_dynamic_io".to_string());
                        }
                        "set_current_dir" | "current_dir" | "current_exe" | "temp_dir" | "home_dir"
                            if use_saw_env =>
                        {
                            aliased_fs_fns.insert(alias);
                            aliased_fs_meta_fns.insert(alias);
                            defined_symbols.insert("rust_dynamic_io".to_string());
                            defined_symbols.insert("rust_dynamic_env".to_string());
                        }
                        "read" | "read_to_string" if use_saw_fs => {
                            aliased_fs_fns.insert(alias);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_io".to_string());
                            }
                        }
                        "read_dir"
                        | "metadata"
                        | "symlink_metadata"
                        | "exists"
                        | "canonicalize"
                        | "read_link"
                        | "copy"
                        | "hard_link"
                            if use_saw_fs =>
                        {
                            aliased_fs_fns.insert(alias);
                            aliased_fs_meta_fns.insert(alias);
                            if use_is_pub {
                                defined_symbols.insert("rust_dynamic_io".to_string());
                            }
                        }
                        _ => {}
                    }
                } else if !next_is_as {
                    if use_is_pub
                        && matches!(*orig, "include_str" | "include_bytes" | "include")
                    {
                        defined_symbols.insert("rust_dynamic_include".to_string());
                    }
                    if use_is_pub
                        && (matches!(*orig, "env" | "option_env")
                            || (use_saw_env
                                && matches!(*orig, "var" | "var_os" | "vars" | "vars_os")))
                    {
                        defined_symbols.insert("rust_dynamic_env".to_string());
                    }
                    if use_is_pub
                        && (matches!(*orig, "fs" | "fs_err" | "File" | "OpenOptions")
                            || (use_saw_fs
                                && matches!(
                                    *orig,
                                    "read"
                                        | "read_to_string"
                                        | "read_dir"
                                        | "exists"
                                        | "metadata"
                                        | "symlink_metadata"
                                        | "canonicalize"
                                        | "read_link"
                                        | "copy"
                                        | "hard_link"
                                )))
                    {
                        defined_symbols.insert("rust_dynamic_io".to_string());
                    }
                    if use_saw_env && matches!(*orig, "var" | "var_os") {
                        aliased_env_getters.insert(orig);
                    } else if use_saw_env && matches!(*orig, "vars" | "vars_os") {
                        aliased_env_enums.insert(orig);
                    }
                    if use_saw_fs
                        && matches!(
                            *orig,
                            "read"
                                | "read_to_string"
                                | "read_dir"
                                | "exists"
                                | "metadata"
                                | "symlink_metadata"
                                | "canonicalize"
                                | "read_link"
                                | "copy"
                                | "hard_link"
                        )
                    {
                        aliased_fs_fns.insert(orig);
                        if matches!(
                            *orig,
                            "read_dir"
                                | "exists"
                                | "metadata"
                                | "symlink_metadata"
                                | "canonicalize"
                                | "read_link"
                                | "copy"
                                | "hard_link"
                        ) {
                            aliased_fs_meta_fns.insert(orig);
                        }
                    }
                }
            }
            RsToken::Star if in_use_stmt && (use_saw_env || use_saw_fs) => {
                if use_saw_env {
                    // Wildcard `use std::env::*;` or `use std::{env::*};` brings `var`/`var_os`/`vars`
                    // into unqualified scope: track all four names.
                    aliased_env_getters.insert("var");
                    aliased_env_getters.insert("var_os");
                    aliased_env_enums.insert("vars");
                    aliased_env_enums.insert("vars_os");
                    if use_is_pub {
                        defined_symbols.insert("rust_dynamic_env".to_string());
                    }
                }
                if use_saw_fs {
                    aliased_fs_fns.insert("read");
                    aliased_fs_fns.insert("read_to_string");
                    aliased_fs_fns.insert("read_dir");
                    aliased_fs_fns.insert("exists");
                    aliased_fs_fns.insert("metadata");
                    aliased_fs_fns.insert("symlink_metadata");
                    aliased_fs_fns.insert("canonicalize");
                    aliased_fs_fns.insert("read_link");
                    aliased_fs_fns.insert("copy");
                    aliased_fs_fns.insert("hard_link");
                    aliased_fs_meta_fns.insert("read_dir");
                    aliased_fs_meta_fns.insert("exists");
                    aliased_fs_meta_fns.insert("metadata");
                    aliased_fs_meta_fns.insert("symlink_metadata");
                    aliased_fs_meta_fns.insert("canonicalize");
                    aliased_fs_meta_fns.insert("read_link");
                    aliased_fs_meta_fns.insert("copy");
                    aliased_fs_meta_fns.insert("hard_link");
                    if use_is_pub {
                        defined_symbols.insert("rust_dynamic_io".to_string());
                    }
                }
            }
            _ => {}
        }
    }

    // Directly mark subprocess execution (`Command::new(...)` / aliased `Command`), cwd mutation
    // (`set_current_dir`), and env-dependent OS dir helpers (`temp_dir`, `home_dir`, `current_exe`,
    // `current_dir`) — including escaped function values (`let f = std::env::temp_dir; f()`) AND
    // unaliased re-exports (`pub use std::env::temp_dir;`) — as untrackable!
    if tokens.iter().enumerate().any(|(i, t)| match t {
        RsToken::Ident("Command")
            if matches!(tokens.get(i + 1), Some(RsToken::ColonColon))
                && matches!(tokens.get(i + 2), Some(RsToken::Ident("new"))) =>
        {
            true
        }
        RsToken::Ident("set_current_dir" | "temp_dir" | "home_dir" | "current_exe" | "current_dir")
            if !matches!(tokens.get(i.saturating_sub(1)), Some(RsToken::Ident("fn") | RsToken::Dot)) =>
        {
            true
        }
        _ => false,
    }) {
        defined_symbols.insert("rust_dynamic_io".to_string());
        defined_symbols.insert("rust_dynamic_env".to_string());
    }
    let uses_open_options = tokens.iter().enumerate().any(|(i, t)| match t {
        RsToken::Ident(id) if aliased_open_options_types.contains(id) => true,
        // Also recognize Rust 1.75+ `File::options()` (including aliased `File` types)!
        RsToken::Ident("options")
            if i >= 2
                && matches!(tokens.get(i - 1), Some(RsToken::ColonColon))
                && matches!(tokens.get(i - 2), Some(RsToken::Ident(ft)) if aliased_file_types.contains(ft)) =>
        {
            true
        }
        _ => false,
    });
    let mut handled_path_method_indices: BTreeSet<usize> = BTreeSet::new();
    let mut scan_in_use = false;
    for (idx, tok) in tokens.iter().enumerate() {
        match tok {
            RsToken::Ident("use") => {
                scan_in_use = true;
            }
            RsToken::Semi => {
                scan_in_use = false;
            }
            _ => {}
        }
        if let RsToken::StringLit(lit) = tok {
            if let Some(rerun) = lit
                .trim()
                .strip_prefix("cargo::rerun-if-changed=")
                .or_else(|| lit.trim().strip_prefix("cargo:rerun-if-changed="))
            {
                let clean = rerun
                    .trim()
                    .trim_start_matches("./")
                    .trim_end_matches('/')
                    .replace('\\', "/");
                if clean.contains('{') || clean.contains('}') {
                    // Dynamic formatted directive (`println!("cargo:rerun-if-changed={}", p)`):
                    // runtime path cannot be statically determined, so fail closed.
                    defined_symbols.insert("rust_dynamic_include".to_string());
                } else if !clean.is_empty() {
                    data_refs.insert(clean);
                }
            } else if lit.starts_with("/api/") {
                defined_symbols.insert(format!("api_route:{}", lit.trim()));
            } else if let Some(clean_ref) = normalize_data_ref(lit)
                && (capture_all_string_data_refs || is_explicit_static_fixture_data_ref(&clean_ref))
            {
                data_refs.insert(clean_ref);
            }
        }
        // Track explicit Rust runtime filesystem read calls (`fs::read("assets/config.json")`,
        // `fs::read_to_string(...)`, `File::open(...)`, `fs::read_dir(...)`, imported aliases,
        // unqualified `use std::fs::read_to_string; read_to_string(...)`, indirect function values
        // like `let load = std::fs::read_to_string;`, chained/stored `OpenOptions::new()` and
        // `File::options()` `.open(...)` calls, plus fail-closed metadata queries `fs::metadata` /
        // `fs::symlink_metadata`) even in production `.rs` files (without capturing inert string
        // constants like `let _state = "live_state.json";`).
        if !scan_in_use
            && let RsToken::Ident(fn_name) = tok
        {
            // Track `Path::new("run/ready.bin").exists()` / `.try_exists()` / `.is_file()` /
            // `.is_dir()` / `.metadata()` / `.symlink_metadata()` (and `PathBuf::from(...)`).
            if matches!(*fn_name, "Path" | "PathBuf")
                && matches!(tokens.get(idx + 1), Some(RsToken::ColonColon))
                && matches!(tokens.get(idx + 2), Some(RsToken::Ident("new" | "from")))
                && matches!(tokens.get(idx + 3), Some(RsToken::LParen))
            {
                if let Some(RsToken::StringLit(path_lit)) = tokens.get(idx + 4)
                    && matches!(tokens.get(idx + 5), Some(RsToken::RParen))
                    && matches!(tokens.get(idx + 6), Some(RsToken::Dot))
                    && let Some(RsToken::Ident(method)) = tokens.get(idx + 7)
                    && matches!(tokens.get(idx + 8), Some(RsToken::LParen))
                {
                    if matches!(
                        *method,
                        "metadata"
                            | "symlink_metadata"
                            | "read_dir"
                            | "is_dir"
                            | "exists"
                            | "try_exists"
                    ) {
                        // Metadata, directory listings, `.is_dir()`, and `.exists()`/`.try_exists()`
                        // (which return true for empty directories that have no file-byte hash node)
                        // cannot be captured by file-byte hashes alone: fail closed.
                        defined_symbols.insert("rust_dynamic_io".to_string());
                    } else if *method == "is_file" {
                        handled_path_method_indices.insert(idx + 7);
                        let has_edge_ws = path_lit != path_lit.trim();
                        let has_posix_backslash = cfg!(unix) && path_lit.contains('\\');
                        let clean = path_lit
                            .trim_start_matches("./")
                            .trim_end_matches('/')
                            .replace('\\', "/");
                        if !has_edge_ws
                            && !has_posix_backslash
                            && !clean.is_empty()
                            && clean != "."
                            && !clean.starts_with('/')
                            && !clean.contains(['\n', '\r', '\0'])
                        {
                            data_refs.insert(clean.clone());
                            defined_symbols.insert(format!("rust_raw_fs:{clean}"));
                        } else {
                            defined_symbols.insert("rust_dynamic_io".to_string());
                        }
                    }
                } else {
                    // Check for dynamic `Path::new(expr).exists()` / `.is_file()` / `.metadata()`
                    let mut k = idx + 4;
                    let mut depth = 1usize;
                    while k < tokens.len() && depth > 0 {
                        match tokens[k] {
                            RsToken::LParen => depth += 1,
                            RsToken::RParen => depth -= 1,
                            _ => {}
                        }
                        k += 1;
                    }
                    if matches!(tokens.get(k), Some(RsToken::Dot))
                        && let Some(RsToken::Ident(method)) = tokens.get(k + 1)
                        && matches!(
                            *method,
                            "exists"
                                | "try_exists"
                                | "is_file"
                                | "is_dir"
                                | "metadata"
                                | "symlink_metadata"
                                | "read_dir"
                        )
                    {
                        defined_symbols.insert("rust_dynamic_io".to_string());
                    }
                }
            }
            // Also catch stored/bound OR indirect `Path`/`PathBuf` filesystem metadata & existence
            // calls (`let p = Path::new("ready"); p.exists()`, `std::env::current_dir()?.join("ready").exists()`,
            // `p.is_file()`, `p.is_dir()`, `p.metadata()`) — all take 0 arguments in `std::path::Path`,
            // so matching `.method()` without requiring a `Path` token avoids false negatives!
            if !handled_path_method_indices.contains(&idx)
                && idx > 0
                && matches!(tokens.get(idx - 1), Some(RsToken::Dot))
                && matches!(tokens.get(idx + 1), Some(RsToken::LParen))
                && matches!(tokens.get(idx + 2), Some(RsToken::RParen))
                && matches!(
                    *fn_name,
                    "exists"
                        | "try_exists"
                        | "is_file"
                        | "is_dir"
                        | "metadata"
                        | "symlink_metadata"
                        | "read_dir"
                        | "canonicalize"
                        | "read_link"
                )
            {
                defined_symbols.insert("rust_dynamic_io".to_string());
            }
            let is_call = matches!(tokens.get(idx + 1), Some(RsToken::LParen));
            let is_fail_closed_fs = matches!(
                *fn_name,
                "read_dir"
                    | "metadata"
                    | "symlink_metadata"
                    | "exists"
                    | "canonicalize"
                    | "read_link"
                    | "copy"
                    | "hard_link"
            ) || aliased_fs_meta_fns.contains(fn_name);
            let is_qualified_fs = idx >= 2
                && matches!(tokens.get(idx - 1), Some(RsToken::ColonColon))
                && ((matches!(
                    *fn_name,
                    "read"
                        | "read_to_string"
                        | "read_dir"
                        | "exists"
                        | "metadata"
                        | "symlink_metadata"
                        | "canonicalize"
                        | "read_link"
                        | "copy"
                        | "hard_link"
                ) && matches!(tokens.get(idx - 2), Some(RsToken::Ident(m)) if aliased_fs_mods.contains(m)))
                    || (*fn_name == "open"
                        && matches!(tokens.get(idx - 2), Some(RsToken::Ident(t)) if aliased_file_types.contains(t))));
            let is_unqualified_fs = aliased_fs_fns.contains(fn_name)
                && !(idx > 0
                    && matches!(
                        tokens.get(idx - 1),
                        Some(
                            RsToken::Dot
                                | RsToken::ColonColon
                                | RsToken::Ident(
                                    "fn" | "struct" | "enum" | "trait" | "type" | "mod" | "const"
                                        | "static" | "let"
                                )
                        )
                    ));
            let is_open_options_dot_open = uses_open_options
                && *fn_name == "open"
                && is_call
                && idx > 0
                && matches!(tokens.get(idx - 1), Some(RsToken::Dot));
            if is_qualified_fs || is_unqualified_fs || is_open_options_dot_open {
                if is_fail_closed_fs {
                    // Directory listings (`fs::read_dir`, which observe empty subdirectories) and
                    // metadata queries (`fs::metadata` / `fs::symlink_metadata`, which observe
                    // permissions/timestamps) cannot be captured by file-byte hashes alone: fail closed.
                    defined_symbols.insert("rust_dynamic_io".to_string());
                } else if is_call {
                    if let Some(RsToken::StringLit(path_lit)) = tokens.get(idx + 2)
                        && matches!(tokens.get(idx + 3), Some(RsToken::RParen))
                    {
                        let has_edge_ws = path_lit != path_lit.trim();
                        let has_posix_backslash = cfg!(unix) && path_lit.contains('\\');
                        let clean = path_lit
                            .trim_start_matches("./")
                            .trim_end_matches('/')
                            .replace('\\', "/");
                        if !has_edge_ws
                            && !has_posix_backslash
                            && !clean.is_empty()
                            && clean != "."
                            && !clean.starts_with('/')
                            && !clean.contains(['\n', '\r', '\0'])
                        {
                            data_refs.insert(clean.clone());
                            defined_symbols.insert(format!("rust_raw_fs:{clean}"));
                        } else {
                            // Normalized empty path (e.g. `"./"`), `"."`, literal whitespace,
                            // POSIX literal backslash, or absolute path: fail closed!
                            defined_symbols.insert("rust_dynamic_io".to_string());
                        }
                    } else if !matches!(tokens.get(idx + 2), Some(RsToken::RParen)) {
                        defined_symbols.insert("rust_dynamic_io".to_string());
                    }
                } else {
                    // Indirect filesystem function value (e.g. `let load = std::fs::read_to_string;`
                    // or `.map(read_to_string)` or turbofish): fail closed.
                    defined_symbols.insert("rust_dynamic_io".to_string());
                }
            }
        }
    }
    if source.lines().any(|l| {
        let t = l.trim_start();
        (t.starts_with("///") || t.starts_with("//!")) && t.contains("```")
    }) {
        markers.insert("doctest".to_string());
    }

    let n = tokens.len();
    let mut i = 0usize;
    let mut pending_test_attr = false;
    let mut pending_pymodule_attr = false;
    let mut pending_wasm_attr = false;
    let mut pending_cfg_test = false;
    let mut pending_pub = false;
    let mut pending_mod_paths: Vec<String> = Vec::new();
    let mut pending_mod_has_cfg_attr_path = false;
    let mut pending_mod_path_rejected = false;
    // Track brace depth + enclosing inline `mod foo { ... }` ancestry so nested
    // `#[path = "a.rs"] mod a;` resolves relative to `foo/` (matching rustc's directory stack).
    let mut brace_depth = 0usize;
    let mut inline_mod_stack: Vec<(usize, String)> = Vec::new();
    let mut pending_inline_mod_dir: Option<String> = None;

    while i < n {
        match &tokens[i] {
            RsToken::Hash => {
                let is_inner = matches!(tokens.get(i + 1), Some(RsToken::Bang));
                let bracket_idx = if is_inner { i + 2 } else { i + 1 };
                if matches!(tokens.get(bracket_idx), Some(RsToken::LBracket)) {
                    let mut depth = 1usize;
                    let mut j = bracket_idx + 1;
                    let mut attr_idents = Vec::new();
                    while j < n && depth > 0 {
                        match &tokens[j] {
                            RsToken::LBracket => {
                                depth += 1;
                                j += 1;
                            }
                            RsToken::RBracket => {
                                depth -= 1;
                                j += 1;
                            }
                            RsToken::Ident(id) => {
                                attr_idents.push(*id);
                                if *id == "path" && matches!(tokens.get(j + 1), Some(RsToken::Eq)) {
                                    if let Some(RsToken::StringLit(p)) = tokens.get(j + 2)
                                        && let Some(clean) = normalize_include_ref(p)
                                    {
                                        pending_mod_paths.push(clean);
                                        if attr_idents.contains(&"cfg_attr") {
                                            pending_mod_has_cfg_attr_path = true;
                                        }
                                    } else {
                                        // Rejected/non-literal `#[path = ...]` (e.g. Unix backslash
                                        // filename or invalid escape): fail closed rather than
                                        // silently falling back to the default module filename.
                                        pending_mod_path_rejected = true;
                                    }
                                } else if *id == "feature"
                                    && matches!(tokens.get(j + 1), Some(RsToken::Eq))
                                    && let Some(RsToken::StringLit(feat)) = tokens.get(j + 2)
                                    && !feat.trim().is_empty()
                                {
                                    markers.insert(format!("feature:{}", feat.trim()));
                                }
                                j += 1;
                            }
                            _ => j += 1,
                        }
                    }
                    let last_ident = attr_idents.last().copied().unwrap_or("");
                    if matches!(last_ident, "test" | "rstest" | "test_case")
                        || (attr_idents.len() >= 2 && last_ident == "test")
                    {
                        pending_test_attr = true;
                    }
                    if last_ident == "pymodule" {
                        pending_pymodule_attr = true;
                    }
                    if last_ident == "wasm_bindgen" || attr_idents.contains(&"wasm_bindgen") {
                        pending_wasm_attr = true;
                        module_markers.insert("wasm_bindgen".to_string());
                    }
                    if attr_idents.contains(&"cfg") && attr_idents.contains(&"test") {
                        pending_cfg_test = true;
                    }
                    if attr_idents.contains(&"ignore") {
                        markers.insert("ignore".to_string());
                    }
                    if attr_idents.contains(&"serial") {
                        markers.insert("serial".to_string());
                        module_markers.insert("serial".to_string());
                    }
                    if attr_idents.contains(&"background_safe") {
                        module_markers.insert("background_safe".to_string());
                    }
                    if attr_idents.contains(&"parallel_safe") {
                        module_markers.insert("parallel_safe".to_string());
                    }
                    for &id in &attr_idents {
                        if id.starts_with("resource_")
                            || matches!(id, "database" | "docker" | "network" | "port" | "live")
                        {
                            markers.insert(id.to_string());
                            if is_inner {
                                module_markers.insert(id.to_string());
                            }
                        }
                    }
                    i = j;
                    continue;
                }
                i += 1;
            }
            RsToken::Ident("pub") => {
                pending_pub = true;
                i += 1;
                if matches!(tokens.get(i), Some(RsToken::LParen)) {
                    let mut depth = 1usize;
                    i += 1;
                    while i < n && depth > 0 {
                        match &tokens[i] {
                            RsToken::LParen => depth += 1,
                            RsToken::RParen => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                }
            }
            RsToken::Ident("use") => {
                let (specs, next_i) = parse_rust_use_statement(&tokens, i + 1);
                for spec in specs {
                    if pending_pub {
                        for sym in &spec.symbols {
                            if sym != "*" {
                                defined_symbols.insert(format!(
                                    "rs_reexport:{sym}->{}:{}:{sym}",
                                    spec.level, spec.module
                                ));
                                defined_symbols.insert(sym.clone());
                            }
                        }
                    }
                    imports.insert(spec);
                }
                pending_pub = false;
                pending_test_attr = false;
                pending_cfg_test = false;
                pending_mod_paths.clear();
                pending_mod_has_cfg_attr_path = false;
                pending_mod_path_rejected = false;
                i = next_i;
            }
            RsToken::LBrace => {
                brace_depth += 1;
                if let Some(dir_seg) = pending_inline_mod_dir.take() {
                    inline_mod_stack.push((brace_depth, dir_seg));
                }
                i += 1;
            }
            RsToken::RBrace => {
                while inline_mod_stack.last().is_some_and(|(d, _)| *d >= brace_depth) {
                    inline_mod_stack.pop();
                }
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
            }
            RsToken::Ident("crate" | "super" | "self")
                if matches!(tokens.get(i + 1), Some(RsToken::ColonColon))
                    && matches!(tokens.get(i + 2), Some(RsToken::Ident(_))) =>
            {
                // Capture qualified Rust path expressions outside `use` statements (e.g.
                // `crate::beta::value()`, `super::helper::run()`) as explicit `ImportSpec` edges.
                let level: usize = match &tokens[i] {
                    RsToken::Ident("crate") => 0,
                    RsToken::Ident("self") => 1,
                    RsToken::Ident("super") => 2,
                    _ => 0,
                };
                let mut segs: Vec<&str> = Vec::new();
                let mut k = i + 2;
                let mut extra_supers = 0usize;
                while k < n {
                    if let Some(RsToken::Ident(seg)) = tokens.get(k) {
                        if *seg == "super" && level >= 2 && segs.is_empty() {
                            extra_supers += 1;
                        } else {
                            segs.push(seg);
                        }
                        if matches!(tokens.get(k + 1), Some(RsToken::ColonColon)) {
                            k += 2;
                            continue;
                        }
                        k += 1;
                        break;
                    }
                    break;
                }
                let is_crate_root = matches!(&tokens[i], RsToken::Ident("crate"));
                let is_self_path = matches!(&tokens[i], RsToken::Ident("self"));
                let eff_level = if level >= 2 {
                    // `super::` climbs out of any enclosing inline `mod { ... }` scopes first;
                    // match `rust_use_path_to_parts` convention (`level == super_count`).
                    (1usize + extra_supers).saturating_sub(inline_mod_stack.len())
                } else {
                    0
                };
                if !segs.is_empty() {
                    // Preserve `crate.` / `self.` prefix (matching `parse_rust_use_statement`)
                    // and join nested segments with `.` so `crate::shared::run()` resolves from
                    // crate root even when called from `src/feature/mod.rs`.
                    let prefix_seg: Option<&str> = if is_crate_root {
                        Some("crate")
                    } else if is_self_path {
                        Some("self")
                    } else {
                        None
                    };
                    if segs.len() >= 2 {
                        let mut parts_vec: Vec<&str> = Vec::with_capacity(segs.len());
                        if let Some(pfx) = prefix_seg {
                            parts_vec.push(pfx);
                        }
                        parts_vec.extend_from_slice(&segs[..segs.len() - 1]);
                        let mod_part = parts_vec.join(".");
                        let sym_part = segs[segs.len() - 1].to_string();
                        imports.insert(ImportSpec {
                            level: eff_level,
                            module: mod_part,
                            symbols: vec![sym_part],
                        });
                    } else {
                        let mod_part = if let Some(pfx) = prefix_seg {
                            format!("{pfx}.{}", segs[0])
                        } else {
                            segs[0].to_string()
                        };
                        imports.insert(ImportSpec {
                            level: eff_level,
                            module: mod_part,
                            symbols: Vec::new(),
                        });
                    }
                }
                i = k;
            }
            RsToken::Ident("mod") => {
                if let Some(RsToken::Ident(mod_name)) = tokens.get(i + 1) {
                    let is_inline = matches!(tokens.get(i + 2), Some(RsToken::LBrace));
                    let is_external = matches!(tokens.get(i + 2), Some(RsToken::Semi));
                    if pending_pymodule_attr {
                        defined_symbols.insert(format!("pyo3:{mod_name}"));
                    }
                    let inline_prefix: String = inline_mod_stack
                        .iter()
                        .map(|(_, seg)| seg.as_str())
                        .collect::<Vec<_>>()
                        .join("/");
                    let qualify_with_inline = |p: &str| -> String {
                        if inline_prefix.is_empty() {
                            p.to_string()
                        } else {
                            format!("{inline_prefix}/{p}")
                        }
                    };
                    if is_inline {
                        let next_dir = pending_mod_paths
                            .last()
                            .cloned()
                            .unwrap_or_else(|| (*mod_name).to_string());
                        pending_inline_mod_dir = Some(next_dir);
                    }
                    if is_inline || is_external {
                        if pending_mod_path_rejected {
                            defined_symbols.insert("rust_dynamic_include".to_string());
                        }
                        if pending_mod_paths.is_empty() {
                            rust_mods.insert(RustModDecl {
                                name: (*mod_name).to_string(),
                                custom_path: if is_external && !inline_prefix.is_empty() {
                                    Some(format!("{inline_prefix}/{mod_name}.rs"))
                                } else {
                                    None
                                },
                                is_inline,
                                is_cfg_test: pending_cfg_test,
                            });
                            if is_external && !inline_prefix.is_empty() {
                                rust_mods.insert(RustModDecl {
                                    name: (*mod_name).to_string(),
                                    custom_path: Some(format!("{inline_prefix}/{mod_name}/mod.rs")),
                                    is_inline,
                                    is_cfg_test: pending_cfg_test,
                                });
                            }
                        } else {
                            // Emit EVERY conditional `#[cfg_attr(..., path = "...")]` candidate
                            // (plus the default path when any `cfg_attr` path is present), prefixed
                            // with any enclosing inline `mod` directory ancestry.
                            for custom_p in pending_mod_paths.drain(..) {
                                rust_mods.insert(RustModDecl {
                                    name: (*mod_name).to_string(),
                                    custom_path: Some(qualify_with_inline(&custom_p)),
                                    is_inline,
                                    is_cfg_test: pending_cfg_test,
                                });
                            }
                            if pending_mod_has_cfg_attr_path {
                                if !inline_prefix.is_empty() {
                                    // Emit BOTH default module candidates (`inline/child.rs` AND
                                    // `inline/child/mod.rs`) when the `cfg_attr` condition is false.
                                    rust_mods.insert(RustModDecl {
                                        name: (*mod_name).to_string(),
                                        custom_path: Some(format!("{inline_prefix}/{mod_name}.rs")),
                                        is_inline,
                                        is_cfg_test: pending_cfg_test,
                                    });
                                    rust_mods.insert(RustModDecl {
                                        name: (*mod_name).to_string(),
                                        custom_path: Some(format!(
                                            "{inline_prefix}/{mod_name}/mod.rs"
                                        )),
                                        is_inline,
                                        is_cfg_test: pending_cfg_test,
                                    });
                                } else {
                                    rust_mods.insert(RustModDecl {
                                        name: (*mod_name).to_string(),
                                        custom_path: None,
                                        is_inline,
                                        is_cfg_test: pending_cfg_test,
                                    });
                                }
                            }
                        }
                    }
                    pending_pub = false;
                    pending_test_attr = false;
                    pending_pymodule_attr = false;
                    pending_wasm_attr = false;
                    pending_cfg_test = false;
                    pending_mod_paths.clear();
                    pending_mod_has_cfg_attr_path = false;
                    pending_mod_path_rejected = false;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident("fn") => {
                if let Some(RsToken::Ident(fn_name)) = tokens.get(i + 1) {
                    defined_symbols.insert((*fn_name).to_string());
                    if pending_pymodule_attr {
                        defined_symbols.insert(format!("pyo3:{fn_name}"));
                    }
                    if pending_wasm_attr {
                        defined_symbols.insert(format!("wasm:{fn_name}"));
                    }
                    if pending_test_attr {
                        test_symbols.insert((*fn_name).to_string());
                    }
                    pending_pub = false;
                    pending_test_attr = false;
                    pending_pymodule_attr = false;
                    pending_wasm_attr = false;
                    pending_cfg_test = false;
                    pending_mod_paths.clear();
                    pending_mod_has_cfg_attr_path = false;
                    pending_mod_path_rejected = false;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(kw)
                if matches!(
                    *kw,
                    "struct" | "enum" | "trait" | "type" | "const" | "static"
                ) =>
            {
                if *kw == "const"
                    && matches!(tokens.get(i + 1), Some(RsToken::Ident("fn" | "unsafe")))
                {
                    // `const fn` modifier before `fn`
                    i += 1;
                    continue;
                }
                if let Some(RsToken::Ident(sym_name)) = tokens.get(i + 1) {
                    defined_symbols.insert((*sym_name).to_string());
                    pending_test_attr = false;
                    pending_cfg_test = false;
                    pending_mod_paths.clear();
                    pending_mod_has_cfg_attr_path = false;
                    pending_mod_path_rejected = false;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident("macro_rules") => {
                if matches!(tokens.get(i + 1), Some(RsToken::Bang))
                    && let Some(RsToken::Ident(macro_name)) = tokens.get(i + 2)
                {
                    defined_symbols.insert((*macro_name).to_string());
                    i += 3;
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(inc_mac)
                if matches!(*inc_mac, "include_str" | "include_bytes" | "include")
                    || aliased_include_macros.contains_key(*inc_mac) =>
            {
                let is_raw_embed = aliased_include_macros
                    .get(*inc_mac)
                    .copied()
                    .unwrap_or(matches!(*inc_mac, "include_str" | "include_bytes"));
                if matches!(tokens.get(i + 1), Some(RsToken::Bang))
                    && matches!(
                        tokens.get(i + 2),
                        Some(RsToken::LParen | RsToken::LBracket | RsToken::LBrace)
                    )
                {
                    if let Some(RsToken::StringLit(lit)) = tokens.get(i + 3)
                        && matches!(
                            tokens.get(i + 4),
                            Some(RsToken::RParen | RsToken::RBracket | RsToken::RBrace)
                        )
                    {
                        if let Some(clean) = normalize_include_ref(lit) {
                            data_refs.insert(clean.clone());
                            if is_raw_embed {
                                defined_symbols.insert(format!("rust_raw_embed:{clean}"));
                            }
                        } else {
                            defined_symbols.insert("rust_dynamic_include".to_string());
                        }
                        i += 5;
                    } else if matches!(tokens.get(i + 3), Some(RsToken::Ident("concat")))
                        && matches!(tokens.get(i + 4), Some(RsToken::Bang))
                        && matches!(tokens.get(i + 5), Some(RsToken::LParen))
                        && matches!(tokens.get(i + 6), Some(RsToken::Ident("env")))
                        && matches!(tokens.get(i + 7), Some(RsToken::Bang))
                        && matches!(tokens.get(i + 8), Some(RsToken::LParen))
                        && matches!(
                            tokens.get(i + 9),
                            Some(RsToken::StringLit(s)) if s == "CARGO_MANIFEST_DIR"
                        )
                        && matches!(tokens.get(i + 10), Some(RsToken::RParen))
                        && matches!(tokens.get(i + 11), Some(RsToken::Comma))
                        && let Some(RsToken::StringLit(suffix)) = tokens.get(i + 12)
                        && matches!(tokens.get(i + 13), Some(RsToken::RParen))
                    {
                        // `concat!(env!("CARGO_MANIFEST_DIR"), suffix)` is ONLY crate-relative
                        // when `suffix` begins with `/` (otherwise it appends to the crate
                        // directory name itself, e.g. `mycrate_extra/a.txt` -> fail closed).
                        if let Some(after_slash) = suffix.strip_prefix('/')
                            && let Some(clean) = normalize_include_ref(after_slash)
                        {
                            data_refs.insert(clean.clone());
                            if is_raw_embed {
                                defined_symbols.insert(format!("rust_raw_embed_crate:{clean}"));
                            }
                        } else {
                            defined_symbols.insert("rust_dynamic_include".to_string());
                        }
                        i += 14;
                    } else {
                        // Dynamic/macro-computed include path (e.g. `include_str!(concat!(env!("OUT_DIR"), ...))`):
                        // fail closed rather than silently missing the dependency.
                        defined_symbols.insert("rust_dynamic_include".to_string());
                        i += 3;
                    }
                } else {
                    i += 1;
                }
            }
            RsToken::Ident(mac)
                if matches!(*mac, "env" | "option_env") || aliased_env_macros.contains(*mac) =>
            {
                if matches!(tokens.get(i + 1), Some(RsToken::Bang))
                    && matches!(
                        tokens.get(i + 2),
                        Some(RsToken::LParen | RsToken::LBracket | RsToken::LBrace)
                    )
                {
                    if let Some(RsToken::StringLit(var_name)) = tokens.get(i + 3) {
                        if is_valid_rust_env_key(var_name) {
                            env_vars.insert(var_name.to_string());
                        } else if !var_name.is_empty() {
                            defined_symbols.insert("rust_dynamic_env".to_string());
                        }
                        i += 4;
                        continue;
                    }
                    defined_symbols.insert("rust_dynamic_env".to_string());
                    i += 3;
                    continue;
                }
                i += 1;
            }
            RsToken::Ident(env_fn)
                if matches!(*env_fn, "var" | "var_os" | "vars" | "vars_os")
                    || aliased_env_getters.contains(*env_fn)
                    || aliased_env_enums.contains(*env_fn) =>
            {
                let is_aliased_getter = aliased_env_getters.contains(*env_fn);
                let is_aliased_enum = aliased_env_enums.contains(*env_fn);
                let prev_tok = i.checked_sub(1).and_then(|idx| tokens.get(idx));
                let is_method_or_decl = matches!(
                    prev_tok,
                    Some(RsToken::Dot | RsToken::Ident("fn" | "let"))
                );
                let qualified_or_imported = is_aliased_getter
                    || is_aliased_enum
                    || matches!(prev_tok, Some(RsToken::ColonColon))
                    || source.contains("std::env")
                    || source.contains("env::var");
                if !is_method_or_decl && qualified_or_imported {
                    if matches!(tokens.get(i + 1), Some(RsToken::LParen)) {
                        if matches!(*env_fn, "vars" | "vars_os") || is_aliased_enum {
                            defined_symbols.insert("rust_dynamic_env".to_string());
                            i += 2;
                            continue;
                        }
                        if let Some(RsToken::StringLit(var_name)) = tokens.get(i + 2)
                            && matches!(tokens.get(i + 3), Some(RsToken::RParen))
                        {
                            if is_valid_rust_env_key(var_name) {
                                env_vars.insert(var_name.to_string());
                            } else if !var_name.is_empty() {
                                defined_symbols.insert("rust_dynamic_env".to_string());
                            }
                            i += 4;
                            continue;
                        }
                        // Dynamic runtime key (`env::var(key)` / `read_config(k)`) -> fail closed.
                        defined_symbols.insert("rust_dynamic_env".to_string());
                        i += 2;
                        continue;
                    } else if matches!(prev_tok, Some(RsToken::ColonColon))
                        || is_aliased_getter
                        || is_aliased_enum
                    {
                        // Function-value reference (`let f = std::env::var;` or `fn(read_config)`):
                        // arguments cannot be statically tracked, so fail closed.
                        defined_symbols.insert("rust_dynamic_env".to_string());
                    }
                }
                i += 1;
            }
            RsToken::Semi => {
                pending_test_attr = false;
                pending_cfg_test = false;
                pending_mod_paths.clear();
                pending_mod_has_cfg_attr_path = false;
                pending_mod_path_rejected = false;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    if rust_source_has_location_macro(source) {
        defined_symbols.insert("rust_caller_sensitive".to_string());
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        fixtures_defined: Vec::new(),
        fixtures_used: Vec::new(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        markers: markers.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        rust_mods: rust_mods.into_iter().collect(),
        type_checking_imports: Vec::new(),
        env_vars_referenced: env_vars.into_iter().collect(),
    }
}

pub(super) fn parse_rust_use_statement(tokens: &[RsToken<'_>], start: usize) -> (Vec<ImportSpec>, usize) {
    let n = tokens.len();
    let mut end = start;
    let mut brace_depth = 0usize;
    while end < n {
        match &tokens[end] {
            RsToken::LBrace => {
                brace_depth += 1;
                end += 1;
            }
            RsToken::RBrace => {
                brace_depth = brace_depth.saturating_sub(1);
                end += 1;
            }
            RsToken::Semi if brace_depth == 0 => {
                break;
            }
            _ => {
                end += 1;
            }
        }
    }
    let slice = &tokens[start..end];
    let next_i = if end < n && matches!(tokens[end], RsToken::Semi) {
        end + 1
    } else {
        end
    };
    let paths = expand_rust_use_slice(&[], slice);
    let mut grouped: BTreeMap<(usize, String), BTreeSet<String>> = BTreeMap::new();
    for path in paths {
        if let Some((level, module, sym)) = rust_use_path_to_parts(&path) {
            let entry = grouped.entry((level, module)).or_default();
            if let Some(s) = sym {
                entry.insert(s);
            }
        }
    }
    let specs = grouped
        .into_iter()
        .map(|((level, module), syms)| ImportSpec {
            level,
            module,
            symbols: syms.into_iter().collect(),
        })
        .collect();
    (specs, next_i)
}

pub(super) fn expand_rust_use_slice(prefix: &[String], slice: &[RsToken<'_>]) -> Vec<Vec<String>> {
    let mut results = Vec::new();
    let mut current = prefix.to_vec();
    let mut i = 0usize;
    let n = slice.len();
    while i < n {
        match &slice[i] {
            RsToken::ColonColon => {
                i += 1;
            }
            RsToken::Ident("as") => {
                i += 2;
            }
            RsToken::Ident(seg) => {
                if *seg == "self" && !current.is_empty() {
                    // `use crate::foo::{self, bar}` -> `current` itself is imported
                } else {
                    current.push((*seg).to_string());
                }
                i += 1;
            }
            RsToken::Star => {
                current.push("*".to_string());
                i += 1;
            }
            RsToken::LBrace => {
                // Find matching RBrace and split top-level comma items
                let mut depth = 1usize;
                let mut j = i + 1;
                let mut item_start = j;
                while j < n && depth > 0 {
                    match &slice[j] {
                        RsToken::LBrace => {
                            depth += 1;
                            j += 1;
                        }
                        RsToken::RBrace => {
                            depth -= 1;
                            if depth == 0 {
                                let sub = &slice[item_start..j];
                                if !sub.is_empty() {
                                    results.extend(expand_rust_use_slice(&current, sub));
                                }
                            }
                            j += 1;
                        }
                        RsToken::Comma if depth == 1 => {
                            let sub = &slice[item_start..j];
                            if !sub.is_empty() {
                                results.extend(expand_rust_use_slice(&current, sub));
                            }
                            j += 1;
                            item_start = j;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                return results;
            }
            _ => {
                i += 1;
            }
        }
    }
    if !current.is_empty() {
        results.push(current);
    }
    results
}

pub(super) fn rust_use_path_to_parts(path: &[String]) -> Option<(usize, String, Option<String>)> {
    if path.is_empty() {
        return None;
    }
    let mut idx = 0usize;
    let mut super_count = 0usize;
    while idx < path.len() && path[idx] == "super" {
        super_count += 1;
        idx += 1;
    }
    let rest = &path[idx..];
    if super_count > 0 {
        if rest.is_empty() {
            return None;
        }
        if rest.len() == 1 {
            return Some((super_count, rest[0].clone(), None));
        }
        let module = rest[..rest.len() - 1].join(".");
        let sym = rest.last().cloned();
        return Some((super_count, module, sym));
    }
    if rest.len() == 1 {
        return Some((0, rest[0].clone(), None));
    }
    let module = rest[..rest.len() - 1].join(".");
    let sym = rest.last().cloned();
    Some((0, module, sym))
}


