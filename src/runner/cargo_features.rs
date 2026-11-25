use super::*;

/// Checks whether a Rust test file `test_file` (workspace-relative) belongs to an explicit Cargo
/// target selector (`"lib"`, `"bins"`, `"bin:<name>"`, `"test:<name>"`), preventing reverse-dep
/// expansion of a non-test library file from pulling in integration or binary tests.
pub(super) fn rust_file_matches_explicit_cargo_target(
    root: &Path,
    test_file: &str,
    cargo_target: &str,
) -> bool {
    let crate_dir = rust_crate_dir_on_disk(root, test_file);
    let workdir = if crate_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&crate_dir)
    };
    let rel_in = rel_path_in_crate(&crate_dir, test_file);
    let manifest = parse_cargo_manifest_targets(&workdir);
    match cargo_target {
        "lib" => {
            if manifest.lib_path.as_deref() == Some(rel_in) {
                return true;
            }
            matches!(
                resolve_rust_child_target_ownership(&workdir, &manifest, rel_in, Some("lib")),
                RustTargetChildOwnership::UniqueLib(_) | RustTargetChildOwnership::Ambiguous
            )
        }
        "bins" => {
            let is_root = manifest.custom_bins.contains_key(rel_in)
                || (manifest.main_bin.is_some() && rel_in == "src/main.rs")
                || (manifest.autobins
                    && auto_discovered_rust_bin_roots(&workdir)
                        .iter()
                        .any(|(p, _)| p == rel_in));
            if is_root {
                return true;
            }
            matches!(
                resolve_rust_child_target_ownership(&workdir, &manifest, rel_in, Some("bins")),
                RustTargetChildOwnership::UniqueBin(_, _)
                    | RustTargetChildOwnership::MultipleBins(_)
                    | RustTargetChildOwnership::Ambiguous
            )
        }
        "doc" => false,
        other => {
            if let Some(wanted_bin) = other.strip_prefix("bin:") {
                let is_root = manifest
                    .custom_bins
                    .get(rel_in)
                    .is_some_and(|names| names.iter().any(|n| n == wanted_bin))
                    || (manifest.main_bin.as_deref() == Some(wanted_bin) && rel_in == "src/main.rs")
                    || (manifest.autobins
                        && auto_discovered_rust_bin_roots(&workdir)
                            .iter()
                            .any(|(p, n)| p == rel_in && n == wanted_bin));
                if is_root {
                    return true;
                }
                matches!(
                    resolve_rust_child_target_ownership(&workdir, &manifest, rel_in, Some(other)),
                    RustTargetChildOwnership::UniqueBin(_, _) | RustTargetChildOwnership::Ambiguous
                )
            } else if let Some(wanted_test) = other.strip_prefix("test:") {
                let is_root = manifest
                    .custom_tests
                    .get(rel_in)
                    .is_some_and(|names| names.iter().any(|n| n == wanted_test))
                    || (manifest.autotests
                        && crate::ast_graph::rust_integration_test_target_name(test_file)
                            .as_deref()
                            == Some(wanted_test));
                if is_root {
                    return true;
                }
                matches!(
                    resolve_rust_child_target_ownership(&workdir, &manifest, rel_in, Some(other)),
                    RustTargetChildOwnership::UniqueTest(_, _) | RustTargetChildOwnership::Ambiguous
                )
            } else {
                true
            }
        }
    }
}

/// Collects all crate-relative `.rs` files reachable from a Cargo target root (`root_rel`),
/// recursively following external `mod` declarations and `#[path = "..."]`.
pub fn collect_rust_target_tree_files(
    crate_root: &Path,
    root_rel: &str,
    test_bearing_only: bool,
) -> Vec<String> {
    let norm_root = root_rel.trim_start_matches("./").replace('\\', "/");
    let mut owned: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<(String, bool, usize)> = vec![(norm_root.clone(), true, 0)];
    let mut visited: BTreeSet<String> = BTreeSet::new();
    while let Some((cur_rel, is_crate_root, depth)) = stack.pop() {
        if depth > 64 || !visited.insert(cur_rel.clone()) {
            continue;
        }
        let full = crate_root.join(&cur_rel);
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        let source = String::from_utf8_lossy(&bytes);
        if !test_bearing_only
            || !crate::ast_graph::parse_rust_source_for_path(&cur_rel, &source)
                .test_symbols
                .is_empty()
        {
            owned.insert(cur_rel.clone());
        }
        let (ext_mods, _) = extract_rust_external_mods_with_ancestry(&source);
        let cur_path = Path::new(&cur_rel);
        let cur_dir = cur_path
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let cur_stem = cur_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let is_mod_dir_root = is_crate_root || cur_stem == "mod";
        let join_dir = |base: &str, rel: &str| -> String {
            let clean = rel.trim_start_matches("./").replace('\\', "/");
            if base.is_empty() {
                clean
            } else {
                format!("{base}/{clean}")
            }
        };
        for m in &ext_mods {
            let initial_base = if is_mod_dir_root {
                cur_dir.clone()
            } else {
                join_dir(&cur_dir, cur_stem)
            };
            let mut base_dirs: Vec<String> = vec![initial_base];
            for (anc_name, anc_paths, anc_cond) in &m.inline_ancestors {
                let mut next_dirs = Vec::new();
                for bd in &base_dirs {
                    if anc_paths.is_empty() {
                        next_dirs.push(join_dir(bd, anc_name));
                    } else {
                        for ap in anc_paths {
                            next_dirs.push(join_dir(bd, ap));
                        }
                        if *anc_cond {
                            next_dirs.push(join_dir(bd, anc_name));
                        }
                    }
                }
                base_dirs = next_dirs;
            }
            let mut candidates: Vec<String> = Vec::new();
            if !m.custom_paths.is_empty() {
                let path_bases: Vec<&str> = if m.inline_ancestors.is_empty() {
                    vec![cur_dir.as_str()]
                } else {
                    base_dirs.iter().map(String::as_str).collect()
                };
                for pb in path_bases {
                    for pa in &m.custom_paths {
                        let norm_pa = pa.trim_start_matches("./").replace('\\', "/");
                        candidates.push(join_dir(pb, &norm_pa));
                    }
                }
                if m.is_conditional_path {
                    for bd in &base_dirs {
                        candidates.push(join_dir(bd, &format!("{}.rs", m.name)));
                        candidates.push(join_dir(bd, &format!("{}/mod.rs", m.name)));
                    }
                }
            } else {
                for bd in &base_dirs {
                    candidates.push(join_dir(bd, &format!("{}.rs", m.name)));
                    candidates.push(join_dir(bd, &format!("{}/mod.rs", m.name)));
                }
            }
            for cand in candidates {
                if let Some(norm_cand) = crate::ast_graph::normalize_lexical_rel(Path::new(&cand))
                    && crate_root.join(&norm_cand).is_file()
                {
                    stack.push((norm_cand, false, depth + 1));
                }
            }
        }
    }
    owned.into_iter().collect()
}

/// Conservatively checks whether a Rust source file contains a module-level macro invocation
/// (at top-level or nested inside `mod ... { ... }`, outside `fn`/`impl`/`struct`/etc. bodies,
/// excluding `macro_rules!` and `thread_local!`) that could expand `#[test]` functions dynamically.
pub(super) fn rust_source_has_module_level_macro(source: &str) -> bool {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut brace_stack: Vec<bool> = Vec::new();
    let mut pending_mod_brace = false;
    let is_ident_start = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let is_ident_cont = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    while i < len {
        if bytes[i].is_ascii_whitespace()
            || (i + 1 < len && bytes[i] == b'/' && (bytes[i + 1] == b'/' || bytes[i + 1] == b'*'))
        {
            i = skip_rust_ws_and_comments(bytes, i);
            continue;
        }
        if matches!(bytes[i], b'r' | b'b' | b'c')
            && let Some(next_i) = skip_rust_raw_string(bytes, i)
        {
            i = next_i;
            continue;
        }
        if bytes[i] == b'"' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'\'' {
            if i + 2 < len && bytes[i + 1] == b'\\' {
                let mut p = i + 3;
                while p < len && bytes[p] != b'\'' && bytes[p] != b'\n' && p - i <= 12 {
                    p += 1;
                }
                if p < len && bytes[p] == b'\'' {
                    i = p + 1;
                    continue;
                }
            } else if i + 2 < len && bytes[i + 2] == b'\'' && bytes[i + 1] != b'\'' {
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        // Inspect `#![...]` / `#[...]` attributes: skip inner attributes so `!` isn't confused
        // with macros, and check outer module-scope attributes for custom proc-macro test
        // generators (e.g. `#[quickcheck]`, `#[rstest]`, `#[test_case(...)]`, `#[sqlx::test]`).
        if bytes[i] == b'#' {
            let mut p = i + 1;
            let is_inner = p < len && bytes[p] == b'!';
            if is_inner {
                p += 1;
            }
            p = skip_rust_ws_and_comments(bytes, p);
            if p < len && bytes[p] == b'[' {
                let attr_body_start = p + 1;
                let mut depth = 1usize;
                p += 1;
                while p < len && depth > 0 {
                    match bytes[p] {
                        b'[' => depth += 1,
                        b']' => depth -= 1,
                        b'"' => {
                            p += 1;
                            while p < len {
                                if bytes[p] == b'\\' && p + 1 < len {
                                    p += 2;
                                    continue;
                                }
                                if bytes[p] == b'"' {
                                    break;
                                }
                                p += 1;
                            }
                        }
                        _ => {}
                    }
                    p += 1;
                }
                if !is_inner && brace_stack.iter().all(|&is_mod| is_mod) && p > attr_body_start {
                    let attr_body = &source[attr_body_start..p.saturating_sub(1)];
                    let attr_path: String = attr_body
                        .trim_start()
                        .chars()
                        .take_while(|c: &char| c.is_ascii_alphanumeric() || matches!(c, '_' | ':'))
                        .collect();
                    let is_known_non_generator_attr = |ap: &str| {
                        matches!(
                            ap,
                            "" | "cfg"
                                | "allow"
                                | "warn"
                                | "deny"
                                | "forbid"
                                | "expect"
                                | "doc"
                                | "derive"
                                | "repr"
                                | "inline"
                                | "cold"
                                | "must_use"
                                | "deprecated"
                                | "non_exhaustive"
                                | "path"
                                | "test"
                                | "ignore"
                                | "should_panic"
                                | "macro_export"
                                | "macro_use"
                                | "no_mangle"
                                | "export_name"
                                | "link"
                                | "link_name"
                                | "link_section"
                                | "used"
                                | "automatically_derived"
                                | "track_caller"
                                | "target_feature"
                                | "diagnostic"
                                | "serde"
                                | "tokio::test"
                                | "async_std::test"
                                | "actix_rt::test"
                                | "futures_test::test"
                                | "serial"
                                | "serial_test::serial"
                                | "tokio::main"
                                | "async_std::main"
                        ) || ap.starts_with("clippy::")
                            || ap.starts_with("rustfmt::")
                            || ap.starts_with("diagnostic::")
                    };
                    if attr_path == "cfg_attr" {
                        // Inspect EVERY top-level nested attribute inside
                        // `#[cfg_attr(pred, attr1, attr2, ...)]` (not just the first one!).
                        if let Some(open_paren) = attr_body.find('(') {
                            let inner = &attr_body[open_paren + 1..];
                            let mut paren_d = 0usize;
                            let mut brack_d = 0usize;
                            let mut brace_d = 0usize;
                            let mut in_str = false;
                            let mut item_idx = 0usize;
                            let mut item_start = 0usize;
                            let ibytes = inner.as_bytes();
                            let mut idx = 0usize;
                            while idx < ibytes.len() {
                                let ch = ibytes[idx];
                                if in_str {
                                    if ch == b'\\' && idx + 1 < ibytes.len() {
                                        idx += 2;
                                        continue;
                                    }
                                    if ch == b'"' {
                                        in_str = false;
                                    }
                                    idx += 1;
                                    continue;
                                }
                                match ch {
                                    b'"' => in_str = true,
                                    b'(' => paren_d += 1,
                                    b')' => {
                                        if paren_d == 0 && brack_d == 0 && brace_d == 0 {
                                            if item_idx > 0 {
                                                let item = inner[item_start..idx].trim_start();
                                                let nested_path: String = item
                                                    .chars()
                                                    .take_while(|c: &char| {
                                                        c.is_ascii_alphanumeric()
                                                            || matches!(c, '_' | ':')
                                                    })
                                                    .collect();
                                                if !is_known_non_generator_attr(&nested_path) {
                                                    return true;
                                                }
                                            }
                                            break;
                                        }
                                        paren_d -= 1;
                                    }
                                    b'[' => brack_d += 1,
                                    b']' => brack_d = brack_d.saturating_sub(1),
                                    b'{' => brace_d += 1,
                                    b'}' => brace_d = brace_d.saturating_sub(1),
                                    b',' if paren_d == 0 && brack_d == 0 && brace_d == 0 => {
                                        if item_idx > 0 {
                                            let item = inner[item_start..idx].trim_start();
                                            let nested_path: String = item
                                                .chars()
                                                .take_while(|c: &char| {
                                                    c.is_ascii_alphanumeric()
                                                        || matches!(c, '_' | ':')
                                                })
                                                .collect();
                                            if !is_known_non_generator_attr(&nested_path) {
                                                return true;
                                            }
                                        }
                                        item_idx += 1;
                                        item_start = idx + 1;
                                    }
                                    _ => {}
                                }
                                idx += 1;
                            }
                        }
                    } else if !is_known_non_generator_attr(&attr_path) {
                        return true;
                    }
                }
                i = p;
                continue;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'{' {
            brace_stack.push(pending_mod_brace);
            pending_mod_brace = false;
            i += 1;
            continue;
        }
        if bytes[i] == b'}' {
            brace_stack.pop();
            pending_mod_brace = false;
            i += 1;
            continue;
        }
        if bytes[i] == b';' {
            pending_mod_brace = false;
            i += 1;
            continue;
        }
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < len && is_ident_cont(bytes[i]) {
                i += 1;
            }
            let ident = &source[start..i];
            if ident == "mod" && brace_stack.iter().all(|&is_mod| is_mod) {
                let name_start = skip_rust_ws_and_comments(bytes, i);
                if name_start < len {
                    let ns = if name_start + 2 < len
                        && bytes[name_start] == b'r'
                        && bytes[name_start + 1] == b'#'
                        && is_ident_start(bytes[name_start + 2])
                    {
                        name_start + 2
                    } else if is_ident_start(bytes[name_start]) {
                        name_start
                    } else {
                        continue;
                    };
                    let mut ne = ns + 1;
                    while ne < len && is_ident_cont(bytes[ne]) {
                        ne += 1;
                    }
                    let after_name = skip_rust_ws_and_comments(bytes, ne);
                    if after_name < len && bytes[after_name] == b'{' {
                        pending_mod_brace = true;
                        i = after_name;
                        continue;
                    }
                }
            }
            if brace_stack.iter().all(|&is_mod| is_mod) {
                let after = skip_rust_ws_and_comments(bytes, i);
                if after < len && bytes[after] == b'!' {
                    let after_bang = skip_rust_ws_and_comments(bytes, after + 1);
                    if after_bang < len
                        && matches!(bytes[after_bang], b'(' | b'{' | b'[')
                        && !matches!(ident, "macro_rules" | "thread_local")
                    {
                        return true;
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    false
}

/// Returns true if any `.rs` file in the target's module tree contains a module-level macro
/// invocation that could generate `#[test]` functions not statically visible in the AST.
pub fn rust_target_tree_may_have_dynamic_tests(crate_root: &Path, root_rel: &str) -> bool {
    for rel in collect_rust_target_tree_files(crate_root, root_rel, false) {
        if let Ok(bytes) = std::fs::read(crate_root.join(&rel)) {
            let source = String::from_utf8_lossy(&bytes);
            if rust_source_has_module_level_macro(&source) {
                return true;
            }
        }
    }
    false
}

/// Returns true if the crate has a `[lib]` target with `doctest` enabled (default true) and
/// any `.rs` file in its module tree contains doc-comment code fences or `#[doc = ...]` attributes
/// that could produce executable Rust doctests.
pub fn rust_lib_has_potential_doctests(crate_root: &Path) -> bool {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let Some(lib_root) = manifest.lib_path else {
        return false;
    };
    if let Some(toml_text) = read_normalized_cargo_toml(crate_root) {
        let mut in_lib = false;
        for raw in toml_text.lines() {
            let line = strip_toml_comment_outside_quotes(raw).trim();
            if let Some(h) = normalize_toml_table_header(line) {
                in_lib = h == "[lib]";
                continue;
            }
            if in_lib
                && let Some((k, v)) = line.split_once('=')
                && k.trim() == "doctest"
                && v.trim() == "false"
            {
                return false;
            }
        }
    }
    for rel in collect_rust_target_tree_files(crate_root, &lib_root, false) {
        if let Ok(bytes) = std::fs::read(crate_root.join(&rel)) {
            let src = String::from_utf8_lossy(&bytes);
            fn strip_doc_blockquote(mut cur: &str) -> (&str, usize) {
                // Strip CommonMark blockquote container prefixes (`>`, `> `, `  > > `) AND return
                // the blockquote nesting depth so entering a blockquote right after a paragraph
                // (`/// text\n/// >     panic!()`) resets blank-line state cleanly!
                let mut depth = 0usize;
                loop {
                    let bytes = cur.as_bytes();
                    let mut sp = 0usize;
                    while sp < 3 && sp < bytes.len() && bytes[sp] == b' ' {
                        sp += 1;
                    }
                    if sp < bytes.len() && bytes[sp] == b'>' {
                        depth += 1;
                        cur = &cur[sp + 1..];
                        if let Some(after_sp) = cur.strip_prefix(' ') {
                            cur = after_sp;
                        }
                    } else {
                        break;
                    }
                }
                (cur, depth)
            }
            let doc_line_info = |doc_body: &str, strip_one_space: bool| -> (bool, bool, usize) {
                let s0 = if strip_one_space {
                    doc_body.strip_prefix(' ').unwrap_or(doc_body)
                } else {
                    doc_body
                };
                let (s, bq_depth) = strip_doc_blockquote(s0);
                let mut col = 0usize;
                let mut rest_idx = 0usize;
                for (bi, ch) in s.char_indices() {
                    match ch {
                        ' ' => {
                            col += 1;
                            rest_idx = bi + 1;
                        }
                        '\t' => {
                            col = (col / 4 + 1) * 4;
                            rest_idx = bi + 1;
                        }
                        _ => break,
                    }
                }
                let is_blank = s.trim().is_empty();
                let is_4col_code = col >= 4 && !s[rest_idx..].trim().is_empty();
                (is_4col_code, is_blank, bq_depth)
            };
            let mut in_block_doc = false;
            for line in src.lines() {
                let t = line.trim_start();
                if in_block_doc {
                    let (body_part, closed) = match line.find("*/") {
                        Some(end_idx) => (&line[..end_idx], true),
                        None => (line, false),
                    };
                    let doc_body = body_part
                        .trim_start()
                        .strip_prefix('*')
                        .unwrap_or(body_part);
                    if doc_body.contains("```") || doc_body.contains("~~~") {
                        return true;
                    }
                    let (is_4col_code, _is_blank, _bq_depth) = doc_line_info(doc_body, false);
                    if is_4col_code {
                        return true;
                    }
                    if closed {
                        in_block_doc = false;
                    }
                    continue;
                }
                if let Some(doc_body) = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")) {
                    if doc_body.contains("```") || doc_body.contains("~~~") {
                        return true;
                    }
                    // Conservatively treat any 4-column indented doc line (following headings,
                    // lists, thematic breaks, blockquotes, or blank lines) as a potential doctest
                    // so `/// # Heading\n///     panic!()` is NEVER missed!
                    let (is_4col_code, _is_blank, _bq_depth) = doc_line_info(doc_body, true);
                    if is_4col_code {
                        return true;
                    }
                } else {
                    // Detect `/** ... */` and `/*! ... */` block doc comments (excluding empty `/**/`)
                    // including both fenced AND space/tab-indented / blockquoted CommonMark code blocks!
                    let block_start = t
                        .find("/**")
                        .filter(|&p| !t[p..].starts_with("/**/"))
                        .or_else(|| t.find("/*!"));
                    if let Some(bs) = block_start {
                        let after = &t[bs + 3..];
                        let (body_part, closed) = match after.find("*/") {
                            Some(end_idx) => (&after[..end_idx], true),
                            None => (after, false),
                        };
                        let (is_4col_code, _is_blank, _bq_depth) = doc_line_info(body_part, false);
                        if body_part.contains("```") || body_part.contains("~~~") || is_4col_code {
                            return true;
                        }
                        if !closed {
                            in_block_doc = true;
                        }
                    }
                    if (t.starts_with("#[doc") || t.starts_with("#![doc"))
                        && (t.contains('=')
                            || t.contains("```")
                            || t.contains("~~~")
                            || t.contains("include_str!"))
                    {
                        return true;
                    }
                    if t.starts_with("#[cfg_attr")
                        && t.contains("doc")
                        && (t.contains('=') || t.contains("include_str!"))
                    {
                        return true;
                    }
                }
            }
            // Also scan complete `#![...]` / `#[...]` attribute blocks across newlines so
            // multiline `#[cfg_attr(\n ...,\n doc = include_str!("README.md")\n)]` triggers doctests!
            let b = src.as_bytes();
            let mut ai = 0usize;
            while ai < b.len() {
                if b[ai] == b'#' {
                    let mut ap = ai + 1;
                    if ap < b.len() && b[ap] == b'!' {
                        ap += 1;
                    }
                    ap = skip_rust_ws_and_comments(b, ap);
                    if ap < b.len() && b[ap] == b'[' {
                        let start = ap + 1;
                        let mut depth = 1usize;
                        ap += 1;
                        while ap < b.len() && depth > 0 {
                            match b[ap] {
                                b'[' => depth += 1,
                                b']' => depth -= 1,
                                b'"' => {
                                    ap += 1;
                                    while ap < b.len() {
                                        if b[ap] == b'\\' && ap + 1 < b.len() {
                                            ap += 2;
                                            continue;
                                        }
                                        if b[ap] == b'"' {
                                            break;
                                        }
                                        ap += 1;
                                    }
                                }
                                _ => {}
                            }
                            ap += 1;
                        }
                        if ap > start {
                            let body = &src[start..ap.saturating_sub(1)];
                            let head: String = body
                                .trim_start()
                                .chars()
                                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                                .collect();
                            if (head == "doc" || head == "cfg_attr")
                                && body.contains("doc")
                                && (body.contains('=')
                                    || body.contains("include_str!")
                                    || body.contains("```")
                                    || body.contains("~~~"))
                            {
                                return true;
                            }
                        }
                        ai = ap;
                        continue;
                    }
                }
                ai += 1;
            }
        }
    }
    false
}

/// Computes `(all_features, enabled_feature_closure)` for `crate_root` given Cargo CLI `extra_args`
/// (`--all-features`, `--no-default-features`, `-F`/`--features`), expanding `[features]` entries
/// transitively (e.g. `default = ["foo"]`, `foo = ["bar"]` -> `{"default", "foo", "bar"}`).
pub fn compute_cargo_enabled_features(
    crate_root: &Path,
    extra_args: &[String],
) -> (bool, BTreeSet<String>) {
    let mut all_features = false;
    let mut no_default_features = false;
    let mut seeds: BTreeSet<String> = BTreeSet::new();
    let pkg_name = cargo_manifest_package_name(crate_root);
    let mut add_feature_tokens = |val: &str| {
        for f in val.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
            let ft = f.trim();
            if ft.is_empty() {
                continue;
            }
            // Preserve dependency-feature selectors (`--features serde/derive`) directly so
            // `required-features = ["serde/derive"]` matches, AND when `qual_pkg` matches this
            // crate's package name (`--features mypkg/foo`) also enable `foo`!
            if let Some((qual_pkg, qual_feat)) = ft.split_once('/') {
                seeds.insert(ft.to_string());
                if pkg_name.as_deref() == Some(qual_pkg.trim()) && !qual_feat.trim().is_empty() {
                    seeds.insert(qual_feat.trim().to_string());
                }
            } else {
                seeds.insert(ft.to_string());
            }
        }
    };
    let mut i = 0usize;
    while i < extra_args.len() && extra_args[i] != "--" {
        let a = extra_args[i].as_str();
        if a == "--all-features" {
            all_features = true;
        } else if a == "--no-default-features" {
            no_default_features = true;
        } else if matches!(a, "-F" | "--features") {
            if let Some(val) = extra_args.get(i + 1) {
                add_feature_tokens(val);
                i += 1;
            }
        } else if let Some(val) = a.strip_prefix("--features=").or_else(|| a.strip_prefix("-F=")) {
            add_feature_tokens(val);
        } else if let Some(val) = a.strip_prefix("-F")
            && !val.is_empty()
        {
            // Attached short feature option (`-Ffoo` or `-Fpkg/foo`)!
            add_feature_tokens(val);
        }
        i += 1;
    }
    let mut feat_graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(text) = read_normalized_cargo_toml(crate_root) {
        let mut in_root_table = true;
        let mut in_features = false;
        let mut collecting_key: Option<String> = None;
        let parse_items = parse_toml_string_array_items;
        // Parse an inline TOML feature table `{ default = ["a"], a = [] }` into `feat_graph`.
        let parse_inline_features = |inline_src: &str, out: &mut BTreeMap<String, Vec<String>>| {
            let trimmed = inline_src.trim();
            let body = trimmed
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or(trimmed);
            let mut entries: Vec<&str> = Vec::new();
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
                        entries.push(&body[start..idx]);
                        start = idx + 1;
                    }
                    _ => {}
                }
            }
            entries.push(&body[start..]);
            for ent in entries {
                if let Some((ek, ev)) = split_toml_kv_outside_quotes(ent) {
                    let kt = ek.trim();
                    let key = unquote_toml_scalar(kt).unwrap_or_else(|| kt.to_string());
                    if !key.is_empty() {
                        let slot = out.entry(key).or_default();
                        parse_items(ev, slot);
                    }
                }
            }
        };
        for raw in text.lines() {
            let line = strip_toml_comment_outside_quotes(raw).trim();
            if line.is_empty() {
                continue;
            }
            if collecting_key.is_none()
                && let Some(h) = normalize_toml_table_header(line)
            {
                in_root_table = false;
                in_features = h == "[features]";
                continue;
            }
            if let Some(ref ck) = collecting_key {
                let entry = feat_graph.entry(ck.clone()).or_default();
                parse_items(line, entry);
                if line.contains(']') {
                    collecting_key = None;
                }
                continue;
            }
            if in_features {
                if let Some((k, v)) = split_toml_kv_outside_quotes(line) {
                    let kt = k.trim();
                    let key = unquote_toml_scalar(kt).unwrap_or_else(|| kt.to_string());
                    let entry = feat_graph.entry(key.clone()).or_default();
                    parse_items(v, entry);
                    if v.contains('[') && !v.contains(']') {
                        collecting_key = Some(key);
                    }
                }
            } else if in_root_table && let Some((k, v)) = split_toml_kv_outside_quotes(line) {
                // Support BOTH root-level dotted TOML feature definitions (`features.default = ["a"]`,
                // `features."integration" = []`) AND root-level inline feature tables
                // (`features = { default = ["integration"], integration = [] }`)!
                let segs = split_toml_dotted_key(k);
                if segs.len() == 1 && segs[0] == "features" && v.trim().starts_with('{') {
                    parse_inline_features(v, &mut feat_graph);
                } else if segs.len() == 2 && segs[0] == "features" {
                    let key = segs[1].clone();
                    let entry = feat_graph.entry(key.clone()).or_default();
                    parse_items(v, entry);
                    if v.contains('[') && !v.contains(']') {
                        collecting_key = Some(key);
                    }
                }
            }
        }
    }
    if !no_default_features && feat_graph.contains_key("default") {
        seeds.insert("default".to_string());
    }
    // Collect optional dependency names whose implicit feature is suppressed by `dep:<name>`
    // anywhere in `[features]` (Cargo 1.60+ semantics): `bar = ["dep:foo"]` activates optional
    // dependency `foo`, NOT `[features].foo`!
    let mut dep_colon_suppressed: BTreeSet<String> = BTreeSet::new();
    for children in feat_graph.values() {
        for ch in children {
            if let Some(dep) = ch.strip_prefix("dep:") {
                dep_colon_suppressed.insert(dep.trim().to_string());
            }
        }
    }
    // Parse `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` / `[target.*.dependencies]`
    // (plus `[workspace.dependencies]` inheritance!) to collect non-optional deps, optional deps,
    // and dependency-declared `features = [...]` (e.g. `serde = { version = "1", features = ["derive"] }`).
    let mut non_opt_deps: BTreeSet<String> = BTreeSet::new();
    let mut opt_deps: BTreeSet<String> = BTreeSet::new();
    let mut dep_declared_features: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ws_dep_features: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut parse_dep_sections = |toml_text: &str, is_ws_root: bool| {
        let mut cur_sec = String::new();
        let mut collecting_dep_feats_for: Option<String> = None;
        for raw_line in toml_text.lines() {
            let line = strip_toml_comment_outside_quotes(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(ref dep_k) = collecting_dep_feats_for {
                let target_map = if is_ws_root {
                    &mut ws_dep_features
                } else {
                    &mut dep_declared_features
                };
                parse_toml_string_array_items(line, target_map.entry(dep_k.clone()).or_default());
                if line.contains(']') {
                    collecting_dep_feats_for = None;
                }
                continue;
            }
            if let Some(h) = normalize_toml_table_header(line) {
                cur_sec = h;
                continue;
            }
            let is_dep_table = if is_ws_root {
                cur_sec == "[workspace.dependencies]"
            } else {
                cur_sec == "[dependencies]"
                    || cur_sec == "[dev-dependencies]"
                    || cur_sec == "[build-dependencies]"
                    || (cur_sec.starts_with("[target.")
                        && (cur_sec.ends_with(".dependencies]")
                            || cur_sec.ends_with(".dev-dependencies]")
                            || cur_sec.ends_with(".build-dependencies]")))
            };
            let subtable_dep: Option<String> = if is_ws_root {
                cur_sec
                    .strip_prefix("[workspace.dependencies.")
                    .and_then(|s| s.strip_suffix(']'))
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            } else {
                cur_sec
                    .strip_prefix("[dependencies.")
                    .or_else(|| cur_sec.strip_prefix("[dev-dependencies."))
                    .or_else(|| cur_sec.strip_prefix("[build-dependencies."))
                    .and_then(|s| s.strip_suffix(']'))
                    .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            };
            if let Some(dep_name) = subtable_dep {
                if !is_ws_root && !opt_deps.contains(&dep_name) {
                    non_opt_deps.insert(dep_name.clone());
                }
                if let Some((k, v)) = line.split_once('=') {
                    let kt = k.trim();
                    let vt = v.trim();
                    if kt == "optional" && vt == "true" && !is_ws_root {
                        non_opt_deps.remove(&dep_name);
                        opt_deps.insert(dep_name.clone());
                    } else if kt == "workspace" && vt == "true" && !is_ws_root {
                        if let Some(wf) = ws_dep_features.get(&dep_name) {
                            dep_declared_features
                                .entry(dep_name.clone())
                                .or_default()
                                .extend(wf.iter().cloned());
                        }
                    } else if kt == "features" && vt.contains('[') {
                        let target_map = if is_ws_root {
                            &mut ws_dep_features
                        } else {
                            &mut dep_declared_features
                        };
                        parse_toml_string_array_items(
                            vt,
                            target_map.entry(dep_name.clone()).or_default(),
                        );
                        if !vt.contains(']') {
                            collecting_dep_feats_for = Some(dep_name);
                        }
                    }
                }
                continue;
            }
            if !is_dep_table && !cur_sec.is_empty() {
                continue;
            }
            if let Some((k, v)) = split_toml_kv_outside_quotes(line) {
                let (dep_name, dotted_sub_owned): (String, Option<String>) = if cur_sec.is_empty()
                {
                    // Root-level dotted TOML dependencies (`dependencies.b = { features = [...] }`,
                    // `dependencies.b.features = [...]`, `workspace.dependencies.b = ...`).
                    let segs = split_toml_dotted_key(k);
                    let rest: Option<&[String]> = if is_ws_root {
                        if segs.len() >= 3 && segs[0] == "workspace" && segs[1] == "dependencies" {
                            Some(&segs[2..])
                        } else {
                            None
                        }
                    } else if segs.len() >= 2
                        && matches!(
                            segs[0].as_str(),
                            "dependencies" | "dev-dependencies" | "build-dependencies"
                        )
                    {
                        Some(&segs[1..])
                    } else if segs.len() >= 4
                        && segs[0] == "target"
                        && matches!(
                            segs[2].as_str(),
                            "dependencies" | "dev-dependencies" | "build-dependencies"
                        )
                    {
                        Some(&segs[3..])
                    } else {
                        None
                    };
                    let Some(rem) = rest else {
                        continue;
                    };
                    (rem[0].clone(), rem.get(1).cloned())
                } else {
                    let segs = split_toml_dotted_key(k);
                    let Some(first) = segs.first() else {
                        continue;
                    };
                    (first.clone(), segs.get(1).cloned())
                };
                let dotted_sub = dotted_sub_owned.as_deref();
                if dep_name.is_empty() {
                    continue;
                }
                let vt = v.trim();
                if let Some(sub) = dotted_sub {
                    if !is_ws_root && !opt_deps.contains(&dep_name) {
                        non_opt_deps.insert(dep_name.clone());
                    }
                    if sub == "optional" && vt == "true" && !is_ws_root {
                        non_opt_deps.remove(&dep_name);
                        opt_deps.insert(dep_name.clone());
                    } else if sub == "workspace" && vt == "true" && !is_ws_root {
                        if let Some(wf) = ws_dep_features.get(&dep_name) {
                            dep_declared_features
                                .entry(dep_name.clone())
                                .or_default()
                                .extend(wf.iter().cloned());
                        }
                    } else if sub == "features" && vt.contains('[') {
                        let target_map = if is_ws_root {
                            &mut ws_dep_features
                        } else {
                            &mut dep_declared_features
                        };
                        parse_toml_string_array_items(
                            vt,
                            target_map.entry(dep_name.clone()).or_default(),
                        );
                        if !vt.contains(']') {
                            collecting_dep_feats_for = Some(dep_name);
                        }
                    }
                    continue;
                }
                let is_optional = vt.contains("optional")
                    && vt
                        .split("optional")
                        .nth(1)
                        .is_some_and(|r| r.trim_start().starts_with('=') && r.split('=').nth(1).is_some_and(|rv| rv.trim_start().starts_with("true")));
                if !is_ws_root {
                    if is_optional {
                        opt_deps.insert(dep_name.clone());
                    } else {
                        non_opt_deps.insert(dep_name.clone());
                    }
                    if vt.contains("workspace")
                        && vt
                            .split("workspace")
                            .nth(1)
                            .is_some_and(|r| r.trim_start().starts_with('=') && r.split('=').nth(1).is_some_and(|rv| rv.trim_start().starts_with("true")))
                        && let Some(wf) = ws_dep_features.get(&dep_name)
                    {
                        dep_declared_features
                            .entry(dep_name.clone())
                            .or_default()
                            .extend(wf.iter().cloned());
                    }
                }
                if let Some(fpos) = vt.find("features")
                    && let Some(eq_off) = vt[fpos + "features".len()..].find('=')
                {
                    let after_eq = vt[fpos + "features".len() + eq_off + 1..].trim_start();
                    if let Some(lb) = after_eq.find('[') {
                        let slice = &after_eq[lb..];
                        let rb = slice.find(']').map_or(slice.len(), |r| r + 1);
                        let target_map = if is_ws_root {
                            &mut ws_dep_features
                        } else {
                            &mut dep_declared_features
                        };
                        parse_toml_string_array_items(
                            &slice[..rb],
                            target_map.entry(dep_name.clone()).or_default(),
                        );
                        if !slice.contains(']') {
                            collecting_dep_feats_for = Some(dep_name);
                        }
                    }
                }
            }
        }
    };
    // Walk starting at `crate_root` ITSELF (not just `crate_root.parent()`) so a manifest that is
    // BOTH `[workspace]` and `[package]` resolves its own `[workspace.dependencies]` inheritance!
    {
        let mut anc: Option<&Path> = Some(crate_root);
        while let Some(d) = anc {
            let ct = d.join("Cargo.toml");
            if ct.is_file()
                && let Ok(ws_text) = std::fs::read_to_string(&ct)
                && ws_text.contains("[workspace")
            {
                parse_dep_sections(&ws_text, true);
                break;
            }
            anc = d.parent();
        }
    }
    if let Some(text) = read_normalized_cargo_toml(crate_root) {
        parse_dep_sections(&text, false);
    }

    // Fixed-point Cargo feature & dependency activation solver preserving feature provenance:
    // - `root_closure`: root `[features]` & CLI `--features` (ONLY these propagate strong
    //   `dep/feat` -> `[features].<dep>`!)
    // - `dep_feat_closure`: dependency-declared `features = [...]` from `[dependencies]` (NEVER
    //   falsely activates an unrelated root `[features].<dep>`!)
    // - `active_deps`: non-optional deps + optional deps activated via `--all-features`, `dep:<d>`,
    //   or implicit features (revisiting weak `dep?/feat` to a fixed point!)
    let mut root_closure: BTreeSet<String> = seeds;
    let mut dep_feat_closure: BTreeSet<String> = BTreeSet::new();
    let mut active_deps: BTreeSet<String> = non_opt_deps;
    if all_features {
        for k in feat_graph.keys() {
            root_closure.insert(k.clone());
        }
        for od in &opt_deps {
            active_deps.insert(od.clone());
            if !dep_colon_suppressed.contains(od) {
                root_closure.insert(od.clone());
            }
        }
    }
    loop {
        let prev_r = root_closure.len();
        let prev_df = dep_feat_closure.len();
        let prev_d = active_deps.len();
        // 1. Active dependencies enable all features declared on their dependency entries
        //    (stored in `dep_feat_closure`, NEVER treated as strong root feature requests!).
        for d in &active_deps {
            if let Some(df_list) = dep_declared_features.get(d) {
                for df in df_list {
                    dep_feat_closure.insert(format!("{d}/{df}"));
                }
            }
        }
        // 2. Propagate root/CLI features (snapshot keys to avoid borrow conflict).
        let current_feats: Vec<String> = root_closure.iter().cloned().collect();
        for feat in current_feats {
            // CLI or transitive strong `dep/feat` selector: activates BOTH `dep` in `active_deps`
            // AND any explicit `[features].<dep>` (Cargo's strong dependency-selector semantics)!
            if !feat.contains("?/")
                && let Some((dep_name, _)) = feat.split_once('/')
            {
                let dn = dep_name.trim();
                if !dn.is_empty() {
                    active_deps.insert(dn.to_string());
                    if feat_graph.contains_key(dn)
                        || (opt_deps.contains(dn) && !dep_colon_suppressed.contains(dn))
                    {
                        root_closure.insert(dn.to_string());
                    }
                }
            } else if opt_deps.contains(&feat) && !dep_colon_suppressed.contains(&feat) {
                active_deps.insert(feat.clone());
            }
            if let Some(children) = feat_graph.get(&feat) {
                for ch in children {
                    let cht = ch.trim();
                    if let Some(dep_only) = cht.strip_prefix("dep:") {
                        let dn = dep_only.trim();
                        if !dn.is_empty() {
                            active_deps.insert(dn.to_string());
                        }
                        continue;
                    }
                    if let Some((dep_name, dep_feat)) = cht.split_once("?/") {
                        let dn = dep_name.trim();
                        let df = dep_feat.trim();
                        if active_deps.contains(dn) && !dn.is_empty() && !df.is_empty() {
                            dep_feat_closure.insert(format!("{dn}/{df}"));
                        }
                        continue;
                    }
                    if let Some((dep_name, dep_feat)) = cht.split_once('/') {
                        let dn = dep_name.trim();
                        let df = dep_feat.trim();
                        if !dn.is_empty() && !df.is_empty() {
                            dep_feat_closure.insert(format!("{dn}/{df}"));
                            active_deps.insert(dn.to_string());
                            if feat_graph.contains_key(dn)
                                || (opt_deps.contains(dn) && !dep_colon_suppressed.contains(dn))
                            {
                                root_closure.insert(dn.to_string());
                            }
                        }
                    } else if !cht.is_empty() {
                        root_closure.insert(cht.to_string());
                    }
                }
            }
        }
        if root_closure.len() == prev_r
            && dep_feat_closure.len() == prev_df
            && active_deps.len() == prev_d
        {
            break;
        }
    }
    let mut closure = root_closure;
    closure.extend(dep_feat_closure);
    (all_features, closure)
}

/// Collects all Cargo workspace crate directories implied by a request (from test files, explicit
/// package-directory targets even when testless, AND all workspace members on a full-workspace run
/// when `invocation_targets` is empty) so testless packages that enable features never disappear!
pub(super) fn requested_workspace_crate_dirs(
    root: &Path,
    invocation_targets: &[String],
    test_files: &[String],
) -> Vec<String> {
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for tf in test_files {
        if tf.ends_with(".rs") {
            let d = rust_crate_dir_on_disk(root, tf);
            dirs.insert(if d.is_empty() { ".".to_string() } else { d });
        }
    }
    if invocation_targets.is_empty() {
        for m in discover_cargo_workspace_member_dirs(root, false) {
            dirs.insert(m);
        }
    } else {
        for raw_t in invocation_targets {
            let base = raw_t
                .split("::")
                .next()
                .unwrap_or(raw_t)
                .trim()
                .trim_start_matches("./")
                .trim_end_matches('/');
            if base.is_empty() || base == "." {
                if root.join("Cargo.toml").is_file() {
                    if cargo_manifest_has_package(root) {
                        dirs.insert(".".to_string());
                    }
                    if cargo_manifest_has_workspace(root) {
                        for m in discover_cargo_workspace_member_dirs(root, false) {
                            dirs.insert(m);
                        }
                    }
                }
            } else if root.join(base).join("Cargo.toml").is_file() {
                dirs.insert(base.to_string());
            } else if root.join(base).is_dir() && cargo_manifest_has_workspace(root) {
                // Include ALL workspace member crates located underneath a requested parent
                // directory (e.g. `targets: ["crates"]` includes testless `crates/b` as well!).
                let pfx = format!("{base}/");
                for m in discover_cargo_workspace_member_dirs(root, false) {
                    if m == base || m.starts_with(&pfx) {
                        dirs.insert(m);
                    }
                }
            }
        }
    }
    dirs.into_iter().collect()
}

/// Checks whether a multi-package workspace invocation requires unified Cargo feature resolution
/// (either CLI feature flags like `--features`/`-F`/`--no-default-features`/`--all-features` were
/// passed — which can be valid for one package and invalid for another — OR ANY selected package
/// depends on another selected package, which can unify default/optional features in Cargo).
pub fn workspace_needs_unified_cargo_features(
    workspace_root: &Path,
    selected_bases: &[String],
    extra_args: &[String],
) -> bool {
    if selected_bases.len() < 2 {
        return false;
    }
    // 1. Any CLI feature flag on a multi-package run must stay unified so package-specific
    //    features (e.g. `--features foo` defined on package A but not B) or `--no-default-features`
    //    never fail or diverge under split per-package invocations!
    if extra_args.iter().take_while(|a| a.as_str() != "--").any(|a| {
        matches!(
            a.as_str(),
            "-F" | "--features" | "--all-features" | "--no-default-features"
        ) || a.starts_with("--features=")
            || a.starts_with("-F")
    }) {
        return true;
    }
    // 2. Any dependency between two selected workspace packages OR ANY shared/transitive
    //    dependency (when >= 2 selected packages declare dependencies, they can share a common
    //    dependency C whose features unify across A and B in Cargo!) requires unified execution.
    let selected_names: BTreeSet<String> = selected_bases
        .iter()
        .filter_map(|b| {
            let dir = if b == "." {
                workspace_root.to_path_buf()
            } else {
                workspace_root.join(b)
            };
            cargo_manifest_package_name(&dir)
        })
        .collect();
    // Also resolve renamed `[workspace.dependencies]` aliases in `workspace_root/Cargo.toml`
    // (e.g. `alias = { package = "b", path = "crates/b" }` inherited via `alias.workspace = true`)
    // so member crates inheriting a renamed alias are recognized as depending on `b`!
    let mut selected_or_ws_alias_names = selected_names.clone();
    if let Some(ws_text) = read_normalized_cargo_toml(workspace_root) {
        let mut in_ws_deps = false;
        let mut cur_ws_dep_alias: Option<String> = None;
        for raw in ws_text.lines() {
            let line = strip_toml_comment_outside_quotes(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(h) = normalize_toml_table_header(line) {
                in_ws_deps = h == "[workspace.dependencies]";
                cur_ws_dep_alias = h
                    .strip_prefix("[workspace.dependencies.")
                    .and_then(|s| s.strip_suffix(']'))
                    .map(|d| d.trim_matches('"').trim_matches('\'').to_string());
                continue;
            }
            if let Some(ref alias) = cur_ws_dep_alias
                && let Some((k, v)) = line.split_once('=')
                && k.trim() == "package"
                && let Some(real_pkg) = unquote_toml_scalar(v)
                && selected_names.contains(&real_pkg)
            {
                selected_or_ws_alias_names.insert(alias.clone());
            } else if in_ws_deps && let Some((k, v)) = line.split_once('=') {
                let alias_key = k.trim().trim_matches('"').trim_matches('\'');
                if v.contains("package")
                    && selected_names
                        .iter()
                        .any(|n| v.contains(&format!("\"{n}\"")) || v.contains(&format!("'{n}'")))
                {
                    selected_or_ws_alias_names.insert(alias_key.to_string());
                }
            }
        }
    }
    let mut pkgs_with_any_dep = 0usize;
    for base in selected_bases {
        let dir = if base == "." {
            workspace_root.to_path_buf()
        } else {
            workspace_root.join(base)
        };
        let Some(text) = read_normalized_cargo_toml(&dir) else {
            continue;
        };
        let mut in_root = true;
        let mut in_deps = false;
        let mut in_dep_subtable = false;
        let mut this_pkg_has_dep = false;
        for raw in text.lines() {
            let line = strip_toml_comment_outside_quotes(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(h) = normalize_toml_table_header(line) {
                in_root = false;
                in_deps = matches!(
                    h.as_str(),
                    "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]"
                ) || h.ends_with(".dependencies]")
                    || h.ends_with(".dev-dependencies]")
                    || h.ends_with(".build-dependencies]");
                let table_dep = h
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .and_then(|s| s.rsplit_once('.'))
                    .and_then(|(pfx, dep)| {
                        (pfx.ends_with("dependencies"))
                            .then(|| dep.trim_matches('"').trim_matches('\'').to_string())
                    });
                in_dep_subtable = table_dep.is_some();
                if let Some(ref d) = table_dep {
                    this_pkg_has_dep = true;
                    if selected_or_ws_alias_names.contains(d) {
                        return true;
                    }
                }
                continue;
            }
            if in_root && let Some((k, v)) = split_toml_kv_outside_quotes(line) {
                // Support root-level dotted TOML dependencies (`dependencies.b = { ... }`,
                // `dev-dependencies.b.features = [...]`, `target.'cfg(target_os = "linux")'.dependencies.b = ...`).
                let segs = split_toml_dotted_key(k);
                let dep_seg: Option<&str> = if segs.len() >= 2
                    && matches!(
                        segs[0].as_str(),
                        "dependencies" | "dev-dependencies" | "build-dependencies"
                    ) {
                    Some(segs[1].as_str())
                } else if segs.len() >= 4
                    && segs[0] == "target"
                    && matches!(
                        segs[2].as_str(),
                        "dependencies" | "dev-dependencies" | "build-dependencies"
                    )
                {
                    Some(segs[3].as_str())
                } else {
                    None
                };
                if let Some(dep_key) = dep_seg {
                    this_pkg_has_dep = true;
                    if selected_or_ws_alias_names.contains(dep_key) {
                        return true;
                    }
                    if v.contains("package")
                        && selected_names
                            .iter()
                            .any(|n| v.contains(&format!("\"{n}\"")) || v.contains(&format!("'{n}'")))
                    {
                        return true;
                    }
                }
                continue;
            }
            if in_dep_subtable
                && let Some((k, v)) = line.split_once('=')
                && k.trim() == "package"
                && let Some(real_pkg) = unquote_toml_scalar(v)
                && selected_names.contains(&real_pkg)
            {
                // Renamed table-form dependency `[dependencies.alias]\npackage = "b"`!
                return true;
            }
            if in_deps && let Some((k, v)) = line.split_once('=') {
                this_pkg_has_dep = true;
                // Handle dotted inline keys (`alias.workspace = true`, `foo.features = [...]`)!
                let dep_key = k
                    .trim()
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                if selected_or_ws_alias_names.contains(dep_key) {
                    return true;
                }
                if v.contains("package")
                    && selected_names
                        .iter()
                        .any(|n| v.contains(&format!("\"{n}\"")) || v.contains(&format!("'{n}'")))
                {
                    return true;
                }
            }
        }
        if this_pkg_has_dep {
            pkgs_with_any_dep += 1;
            // Even 1 selected package having a dependency can transitively unify features onto
            // another selected package (e.g. A -> C -> B where B has 0 dependencies)!
            if pkgs_with_any_dep >= 1 {
                return true;
            }
        }
    }
    false
}

/// Parses a `file://` or `path+file://` Cargo package URL (including `file://localhost/...` and
/// `%XX` percent-encoded segments) into a local filesystem `PathBuf`. Rejects non-localhost hosts.
pub fn cargo_file_url_to_path(src_url: &str) -> Option<PathBuf> {
    let rest = src_url
        .strip_prefix("path+file://")
        .or_else(|| src_url.strip_prefix("file://"))?;
    let path_str = if rest.starts_with('/') {
        rest
    } else if rest
        .get(..10)
        .is_some_and(|p| p.eq_ignore_ascii_case("localhost/"))
    {
        &rest[9..]
    } else {
        return None;
    };
    let mut decoded_bytes: Vec<u8> = Vec::with_capacity(path_str.len());
    let pb = path_str.as_bytes();
    let mut i = 0usize;
    while i < pb.len() {
        if pb[i] == b'%'
            && i + 2 < pb.len()
            && pb[i + 1].is_ascii_hexdigit()
            && pb[i + 2].is_ascii_hexdigit()
            && let Ok(v) = u8::from_str_radix(&path_str[i + 1..i + 3], 16)
        {
            decoded_bytes.push(v);
            i += 3;
            continue;
        }
        decoded_bytes.push(pb[i]);
        i += 1;
    }
    let decoded = String::from_utf8(decoded_bytes).ok()?;
    Some(PathBuf::from(decoded))
}

/// Checks whether a Cargo target's `required-features` are all satisfied under `extra_args`.
/// Queries Cargo's resolved feature graph (`cargo metadata --format-version 1`) for an active
/// dependency's resolved features when a target declares `required-features = ["dep/feat"]` that
/// may be satisfied via `dep`'s OWN `default` features!
pub(super) fn cargo_metadata_dep_feature_enabled(
    crate_root: &Path,
    dep_name: &str,
    dep_feat: &str,
    extra_args: &[String],
) -> Option<bool> {
    let cargo_cmd = crate::catalog::detect_cargo_cmd(crate_root);
    let (prog, base_args) = cargo_cmd.split_first()?;
    let mut cmd = Command::new(prog);
    cmd.current_dir(crate_root);
    cmd.args(base_args);
    cmd.args([
        "metadata",
        "--format-version",
        "1",
        "--manifest-path",
        &crate_root.join("Cargo.toml").to_string_lossy(),
    ]);
    let mut i = 0usize;
    while i < extra_args.len() && extra_args[i] != "--" {
        let a = extra_args[i].as_str();
        if matches!(a, "--all-features" | "--no-default-features" | "--offline" | "--locked" | "--frozen") {
            cmd.arg(a);
        } else if matches!(a, "-F" | "--features") {
            cmd.arg(a);
            if let Some(v) = extra_args.get(i + 1) {
                cmd.arg(v);
                i += 1;
            }
        } else if a.starts_with("--features=")
            || a.starts_with("-F=")
            || (a.starts_with("-F") && a.len() > 2)
        {
            cmd.arg(a);
        }
        i += 1;
    }
    let out = run_command_with_timeout(&mut cmd, Duration::from_secs(15), 8 * 1024 * 1024).ok()?;
    if !out.status.success() {
        return None;
    }
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let target_manifest_canon = crate_root
        .join("Cargo.toml")
        .canonicalize()
        .ok();
    let pkg_name = cargo_manifest_package_name(crate_root);
    let packages = val.get("packages")?.as_array()?;
    // Match the package by canonical `manifest_path` FIRST (so a crate depending on an older
    // version of itself under an alias never resolves the wrong package ID!), falling back to
    // package name only when manifest canonicalization is unavailable.
    let root_id = target_manifest_canon
        .as_ref()
        .and_then(|canon_mp| {
            packages.iter().find_map(|p| {
                let mp = p.get("manifest_path")?.as_str()?;
                let p_canon = Path::new(mp).canonicalize().ok()?;
                if &p_canon == canon_mp {
                    p.get("id")?.as_str().map(str::to_string)
                } else {
                    None
                }
            })
        })
        .or_else(|| {
            let pn = pkg_name.as_deref()?;
            packages.iter().find_map(|p| {
                if p.get("name")?.as_str()? == pn {
                    p.get("id")?.as_str().map(str::to_string)
                } else {
                    None
                }
            })
        })?;
    let nodes = val.get("resolve")?.get("nodes")?.as_array()?;
    let root_node = nodes
        .iter()
        .find(|n| n.get("id").and_then(serde_json::Value::as_str) == Some(root_id.as_str()))?;
    let deps_arr = root_node.get("deps")?.as_array()?;
    // Distinguish "optional dependency genuinely absent from resolved graph" (`Some(false)`) from
    // metadata execution/parsing failure (`None`) so disabled optional deps are NEVER forced on!
    let Some(dep_pkg_id) = deps_arr.iter().find_map(|d| {
        let d_name = d.get("name")?.as_str()?;
        if d_name == dep_name || d_name.replace('-', "_") == dep_name.replace('-', "_") {
            d.get("pkg")?.as_str().map(str::to_string)
        } else {
            None
        }
    }) else {
        return Some(false);
    };
    let dep_node = nodes
        .iter()
        .find(|n| n.get("id").and_then(serde_json::Value::as_str) == Some(dep_pkg_id.as_str()))?;
    let dep_feats = dep_node.get("features")?.as_array()?;
    Some(
        dep_feats
            .iter()
            .any(|f| f.as_str() == Some(dep_feat)),
    )
}

pub fn cargo_required_features_satisfied(
    crate_root: &Path,
    required_features: &[String],
    extra_args: &[String],
) -> bool {
    if required_features.is_empty() {
        return true;
    }
    let (all_features, enabled) = compute_cargo_enabled_features(crate_root, extra_args);
    required_features.iter().all(|rf| {
        // `--all-features` enables every crate feature & optional dep, NOT arbitrary `dep/feat`
        // requirements (`serde/derive`) unless activated by `[features]`, `--features dep/feat`,
        // OR `dep`'s own resolved default features (verified via `cargo metadata`)!
        if let Some((dep_name, dep_feat)) = rf.split_once('/') {
            if enabled.contains(rf) {
                true
            } else {
                cargo_metadata_dep_feature_enabled(
                    crate_root,
                    dep_name.trim(),
                    dep_feat.trim(),
                    extra_args,
                )
                // Fail-open on metadata timeout/truncation (`None` -> `true`) so an indeterminate
                // metadata lookup NEVER silently skips an eligible failing target!
                .unwrap_or(true)
            }
        } else {
            all_features || enabled.contains(rf)
        }
    })
}

/// Checks whether a specific integration test target is eligible for default execution (neither
/// disabled via `test = false` nor gated by unsatisfied `required-features`).
pub fn cargo_test_target_features_satisfied(
    crate_root: &Path,
    test_name: &str,
    extra_args: &[String],
) -> bool {
    let manifest = parse_cargo_manifest_targets(crate_root);
    if manifest.test_disabled.contains(test_name) {
        return false;
    }
    manifest
        .custom_test_required_features
        .get(test_name)
        .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
}

/// Resolves file-valued `--config <rel_path>` Cargo CLI arguments against `origin_dir` (even when
/// absent!) so changing `current_dir` into a workspace member crate never resolves a missing root
/// config path to a different file inside the member directory!
pub fn normalize_cargo_config_args(origin_dir: &Path, extra_args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(extra_args.len());
    let mut i = 0usize;
    let mut after_sep = false;
    while i < extra_args.len() {
        let arg = &extra_args[i];
        if arg == "--" {
            after_sep = true;
            out.push(arg.clone());
            i += 1;
            continue;
        }
        if !after_sep
            && matches!(arg.as_str(), "--config" | "--target-dir" | "--target")
            && i + 1 < extra_args.len()
        {
            let val = &extra_args[i + 1];
            out.push(arg.clone());
            let should_abs = match arg.as_str() {
                "--config" => !val.contains('=') && !Path::new(val).is_absolute(),
                "--target-dir" => !val.is_empty() && !Path::new(val).is_absolute(),
                "--target" => {
                    !Path::new(val).is_absolute()
                        && (val.ends_with(".json")
                            || val.contains('/')
                            || val.contains('\\'))
                }
                _ => false,
            };
            if should_abs {
                let abs = origin_dir.join(val);
                let resolved = abs.canonicalize().unwrap_or(abs);
                out.push(resolved.to_string_lossy().into_owned());
                i += 2;
                continue;
            }
            out.push(val.clone());
            i += 2;
            continue;
        }
        if !after_sep {
            if let Some(val) = arg.strip_prefix("--config=")
                && !val.contains('=')
                && !Path::new(val).is_absolute()
            {
                let abs = origin_dir.join(val);
                let resolved = abs.canonicalize().unwrap_or(abs);
                out.push(format!("--config={}", resolved.to_string_lossy()));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--target-dir=")
                && !val.is_empty()
                && !Path::new(val).is_absolute()
            {
                let abs = origin_dir.join(val);
                let resolved = abs.canonicalize().unwrap_or(abs);
                out.push(format!("--target-dir={}", resolved.to_string_lossy()));
                i += 1;
                continue;
            }
            if let Some(val) = arg.strip_prefix("--target=")
                && !Path::new(val).is_absolute()
                && (val.ends_with(".json") || val.contains('/') || val.contains('\\'))
            {
                let abs = origin_dir.join(val);
                let resolved = abs.canonicalize().unwrap_or(abs);
                out.push(format!("--target={}", resolved.to_string_lossy()));
                i += 1;
                continue;
            }
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}


