use super::*;

pub(super) fn skip_rust_ws_and_comments(bytes: &[u8], mut idx: usize) -> usize {
    let len = bytes.len();
    while idx < len {
        if bytes[idx].is_ascii_whitespace() {
            idx += 1;
        } else if idx + 1 < len && bytes[idx] == b'/' && bytes[idx + 1] == b'/' {
            idx += 2;
            while idx < len && bytes[idx] != b'\n' {
                idx += 1;
            }
        } else if idx + 1 < len && bytes[idx] == b'/' && bytes[idx + 1] == b'*' {
            idx += 2;
            let mut cdepth = 1usize;
            while idx + 1 < len && cdepth > 0 {
                if bytes[idx] == b'/' && bytes[idx + 1] == b'*' {
                    cdepth += 1;
                    idx += 2;
                } else if bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
                    cdepth -= 1;
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
        } else {
            break;
        }
    }
    idx
}

pub(super) fn skip_rust_raw_string(bytes: &[u8], i: usize) -> Option<usize> {
    let len = bytes.len();
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        return None;
    }
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
    if p >= len || bytes[p] != b'"' {
        return None;
    }
    let hashes = p - (r_pos + 1);
    p += 1;
    while p < len {
        if bytes[p] == b'"'
            && p + 1 + hashes <= len
            && bytes[p + 1..p + 1 + hashes].iter().all(|&b| b == b'#')
        {
            return Some(p + 1 + hashes);
        }
        p += 1;
    }
    Some(len)
}

pub(super) fn text_has_rust_path_attr(text: &str) -> bool {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        if i + 1 < len && bytes[i] == b'/' && (bytes[i + 1] == b'/' || bytes[i + 1] == b'*') {
            i = skip_rust_ws_and_comments(bytes, i);
            continue;
        }
        if matches!(bytes[i], b'r' | b'b' | b'c')
            && let Some(next_i) = skip_rust_raw_string(bytes, i)
        {
            i = next_i;
            continue;
        }
        if bytes[i] == b'\'' {
            // Skip char literal (`'"'`, `'\\'`) without treating lifetimes (`'a`) as char literals.
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
        if bytes[i] == b'#' {
            // Rust allows whitespace and comments between `#`, optional `!`, and `[`
            // (e.g. `# [path = "real.rs"]`, `# /* c */ [cfg_attr(..., path = "real.rs")]`).
            let mut after_hash = skip_rust_ws_and_comments(bytes, i + 1);
            if after_hash < len && bytes[after_hash] == b'!' {
                after_hash = skip_rust_ws_and_comments(bytes, after_hash + 1);
            }
            if after_hash < len && bytes[after_hash] == b'[' {
                let mut p = after_hash + 1;
                let mut depth = 1usize;
                let mut in_str = false;
                let attr_start = p;
                while p < len && depth > 0 {
                    if in_str {
                        if bytes[p] == b'\\' && p + 1 < len {
                            p += 2;
                            continue;
                        }
                        if bytes[p] == b'"' {
                            in_str = false;
                        }
                        p += 1;
                        continue;
                    }
                    if p + 1 < len
                        && bytes[p] == b'/'
                        && (bytes[p + 1] == b'/' || bytes[p + 1] == b'*')
                    {
                        p = skip_rust_ws_and_comments(bytes, p);
                        continue;
                    }
                    if matches!(bytes[p], b'r' | b'b' | b'c')
                        && let Some(next_p) = skip_rust_raw_string(bytes, p)
                    {
                        p = next_p;
                        continue;
                    }
                    match bytes[p] {
                        b'"' => in_str = true,
                        b'[' => depth += 1,
                        b']' => depth -= 1,
                        _ => {}
                    }
                    p += 1;
                }
                let attr = &bytes[attr_start..p.saturating_sub(1)];
                let mut j = 0usize;
                while j < attr.len() {
                    if j + 1 < attr.len()
                        && attr[j] == b'/'
                        && (attr[j + 1] == b'/' || attr[j + 1] == b'*')
                    {
                        j = skip_rust_ws_and_comments(attr, j);
                        continue;
                    }
                    if attr[j] == b'"' {
                        j += 1;
                        while j < attr.len() {
                            if attr[j] == b'\\' && j + 1 < attr.len() {
                                j += 2;
                                continue;
                            }
                            if attr[j] == b'"' {
                                j += 1;
                                break;
                            }
                            j += 1;
                        }
                        continue;
                    }
                    if j + 4 <= attr.len()
                        && &attr[j..j + 4] == b"path"
                        && (j == 0
                            || !(attr[j - 1].is_ascii_alphanumeric() || attr[j - 1] == b'_'))
                    {
                        let k = skip_rust_ws_and_comments(attr, j + 4);
                        if k < attr.len() && attr[k] == b'=' {
                            return true;
                        }
                    }
                    j += 1;
                }
                i = p;
                continue;
            }
        }
        i += 1;
    }
    false
}

/// Checks whether any Rust source file under `{crate_dir}/src/`, `{crate_dir}/tests/`, or
/// any declared custom `[lib]` / `[[bin]]` / `[[test]]` target path/directory uses `#[path = "..."]`
/// module overrides, in which case filename-derived module filters or demux prefixes cannot be
/// trusted without resolving the actual `mod` tree.
pub(super) fn crate_uses_custom_mod_path(root: &Path, crate_dir: &str) -> bool {
    let crate_root = if crate_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(crate_dir)
    };
    let manifest = parse_cargo_manifest_targets(&crate_root);
    let mut stack = vec![crate_root.join("src"), crate_root.join("tests")];
    let mut extra_files: BTreeSet<PathBuf> = BTreeSet::new();
    let mut add_target_root = |rel: &str| {
        let p = crate_root.join(rel);
        if p.is_file() {
            extra_files.insert(p.clone());
        }
        if let Some(parent) = p.parent()
            && parent != crate_root
            && parent.starts_with(&crate_root)
            && !stack.iter().any(|existing| existing == parent)
        {
            stack.push(parent.to_path_buf());
        }
    };
    if let Some(ref lp) = manifest.lib_path {
        add_target_root(lp);
    }
    for bp in manifest.custom_bins.keys() {
        add_target_root(bp);
    }
    for tp in manifest.custom_tests.keys() {
        add_target_root(tp);
    }
    for p in extra_files {
        if let Ok(bytes) = std::fs::read(&p) {
            let text = String::from_utf8_lossy(&bytes);
            if text_has_rust_path_attr(&text) {
                return true;
            }
        }
    }
    let mut scanned = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            scanned += 1;
            if scanned > 1_024 {
                // Fail closed on huge source trees if we couldn't finish scanning.
                return true;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let p = entry.path();
            if ft.is_symlink() {
                // Cargo follows symlinked `.rs` files and module directories; fail closed so a
                // symlinked module containing `#[path]` can never enter filename-based batching.
                if p.is_dir() || p.extension().and_then(|e| e.to_str()) == Some("rs") {
                    return true;
                }
                continue;
            }
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("rs")
                && let Ok(bytes) = std::fs::read(&p)
            {
                let text = String::from_utf8_lossy(&bytes);
                if text_has_rust_path_attr(&text) {
                    return true;
                }
            }
        }
    }
    false
}

pub(super) fn rust_bin_name_from_bin_rel(bin_rel: &str) -> Option<String> {
    if let Some((dir_name, _)) = bin_rel.split_once('/') {
        (!dir_name.is_empty()).then(|| dir_name.to_string())
    } else {
        Path::new(bin_rel)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    }
}

pub(super) fn has_nextest_config(root: &Path, crate_dir: &str) -> bool {
    // Walk ALL ancestors from `crate_root` to the filesystem root so any enclosing Cargo
    // workspace's `.config/nextest.toml` (regardless of nesting depth) is detected.
    let crate_root = if crate_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(crate_dir)
    };
    let mut cur: Option<&Path> = Some(crate_root.as_path());
    while let Some(dir) = cur {
        if dir.join(".config").join("nextest.toml").is_file() {
            return true;
        }
        cur = dir.parent();
    }
    false
}

/// Walks Rust source with inline `mod <name> { ... }` scope tracking to extract fully-qualified
/// external child-module prefixes (e.g. `foo::` for top-level `mod foo;` AND `outer::helper::`
/// for nested `mod outer { mod helper; }`) without ever excluding inline `mod foo { ... }` tests.
pub(super) fn extract_declared_external_mod_prefixes_with_ambiguity(source: &str) -> (Vec<String>, Vec<String>) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut brace_stack: Vec<Option<String>> = Vec::new();
    let mut prefixes = Vec::new();
    let mut inline_prefixes = BTreeSet::new();

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
        if bytes[i] == b'{' {
            brace_stack.push(None);
            i += 1;
            continue;
        }
        if bytes[i] == b'}' {
            brace_stack.pop();
            i += 1;
            continue;
        }
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < len && is_ident_cont(bytes[i]) {
                i += 1;
            }
            if &bytes[start..i] == b"mod" {
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
                    let mod_name = String::from_utf8_lossy(&bytes[ns..ne]).into_owned();
                    let after_name = skip_rust_ws_and_comments(bytes, ne);
                    if after_name < len && bytes[after_name] == b';' {
                        let mut full = String::new();
                        for ancestor in brace_stack.iter().flatten() {
                            full.push_str(ancestor);
                            full.push_str("::");
                        }
                        full.push_str(&mod_name);
                        full.push_str("::");
                        prefixes.push(full);
                        i = after_name + 1;
                        continue;
                    } else if after_name < len && bytes[after_name] == b'{' {
                        let mut full = String::new();
                        for ancestor in brace_stack.iter().flatten() {
                            full.push_str(ancestor);
                            full.push_str("::");
                        }
                        full.push_str(&mod_name);
                        full.push_str("::");
                        inline_prefixes.insert(full);
                        brace_stack.push(Some(mod_name));
                        i = after_name + 1;
                        continue;
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    // Split external module prefixes into unambiguous external modules vs ambiguous conditional
    // modules that are declared BOTH externally (`#[cfg(unix)] mod sub;`) AND inline
    // (`#[cfg(not(unix))] mod sub { ... }`) in the same file so callers can fail closed.
    let mut ambiguous: Vec<String> = prefixes
        .iter()
        .filter(|p| inline_prefixes.contains(*p))
        .cloned()
        .collect();
    ambiguous.sort();
    ambiguous.dedup();
    prefixes.retain(|p| !inline_prefixes.contains(p));
    prefixes.sort();
    prefixes.dedup();
    (prefixes, ambiguous)
}

/// Resolves declared external child-module prefixes (including nested `mod outer { mod helper; }`
/// -> `outer::helper::`) AND any ambiguous conditional external+inline module overlaps directly
/// from `test_file`, so inline `mod foo { ... }` blocks inside `lib.rs` are NEVER falsely excluded
/// and conditional external/inline overlaps cannot poison root-node caching.
pub(super) fn sibling_module_prefixes_and_ambiguity_for_root(
    root: &Path,
    crate_dir: &str,
    test_file: &str,
) -> (Vec<String>, Vec<String>) {
    let Ok(bytes) = std::fs::read(root.join(test_file)) else {
        return (Vec::new(), Vec::new());
    };
    let source = String::from_utf8_lossy(&bytes);
    let (mut prefixes, ambiguous) = extract_declared_external_mod_prefixes_with_ambiguity(&source);
    if !prefixes.is_empty() {
        let crate_root = if crate_dir.is_empty() || crate_dir == "." {
            root.to_path_buf()
        } else {
            root.join(crate_dir)
        };
        let file_rel = if crate_dir.is_empty() || crate_dir == "." {
            test_file.to_string()
        } else {
            test_file
                .strip_prefix(&format!("{crate_dir}/"))
                .unwrap_or(test_file)
                .to_string()
        };
        let file_parent = Path::new(&file_rel)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let file_stem = Path::new(&file_rel)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let manifest = parse_cargo_manifest_targets(&crate_root);
        let is_root_like = file_stem == "mod"
            || file_stem == "lib"
            || file_stem == "main"
            || manifest.lib_path.as_deref() == Some(file_rel.as_str())
            || manifest.custom_bins.contains_key(&file_rel)
            || manifest.custom_tests.contains_key(&file_rel)
            || file_rel.starts_with("tests/")
            || file_rel.starts_with("src/bin/");
        let base_dir = if is_root_like {
            file_parent
        } else if file_parent.is_empty() {
            file_stem.to_string()
        } else {
            format!("{file_parent}/{file_stem}")
        };
        // Never exclude an external child module whose tree has 0 static `#[test]` functions but
        // DOES contain module-level macro invocations (`generate_tests!()`): `tctx` would never
        // schedule that child file on its own, so excluding its prefix from the parent would drop
        // its macro-generated tests!
        prefixes.retain(|pfx| {
            let mod_path = pfx.trim_end_matches("::").replace("::", "/");
            let cand1 = if base_dir.is_empty() {
                format!("{mod_path}.rs")
            } else {
                format!("{base_dir}/{mod_path}.rs")
            };
            let cand2 = if base_dir.is_empty() {
                format!("{mod_path}/mod.rs")
            } else {
                format!("{base_dir}/{mod_path}/mod.rs")
            };
            let child_rel = if crate_root.join(&cand1).is_file() {
                Some(cand1)
            } else if crate_root.join(&cand2).is_file() {
                Some(cand2)
            } else {
                None
            };
            if let Some(cr) = child_rel
                && collect_rust_target_tree_files(&crate_root, &cr, true).is_empty()
                && rust_target_tree_may_have_dynamic_tests(&crate_root, &cr)
            {
                return false;
            }
            true
        });
    }
    (prefixes, ambiguous)
}

pub(super) fn sibling_module_prefixes_for_crate_root(root: &Path, crate_dir: &str, test_file: &str) -> Vec<String> {
    let (mut pfx, amb) = sibling_module_prefixes_and_ambiguity_for_root(root, crate_dir, test_file);
    pfx.extend(amb);
    pfx.sort();
    pfx.dedup();
    pfx
}

/// Resolves the nearest enclosing crate directory on disk (by walking ancestor directories toward
/// `root` looking for `Cargo.toml`), falling back to the lexical path-based heuristic. This
/// ensures custom targets outside `src/`/`tests/` inside workspace members (e.g.
/// `crates/foo/suites/api.rs`) resolve to `crates/foo` rather than the workspace root.
pub fn rust_crate_dir_on_disk(root: &Path, rel_path: &str) -> String {
    let mut cur = Path::new(rel_path).parent();
    while let Some(dir) = cur {
        let ds = dir.to_string_lossy().replace('\\', "/");
        let check_dir = if ds.is_empty() || ds == "." {
            root.to_path_buf()
        } else {
            root.join(&ds)
        };
        if check_dir.join("Cargo.toml").is_file() {
            return if ds == "." { String::new() } else { ds };
        }
        if ds.is_empty() || ds == "." {
            break;
        }
        cur = dir.parent();
    }
    crate::ast_graph::rust_crate_dir(rel_path)
}

#[derive(Debug, Clone)]
pub(super) struct RustExternalModWithAncestry {
    /// Enclosing inline `mod` stack: `(mod_name, all_inline_path_overrides, inline_path_is_conditional)`.
    pub(super) inline_ancestors: Vec<(String, Vec<String>, bool)>,
    pub(super) name: String,
    /// All `#[path = "..."]` / `#[cfg_attr(..., path = "...")]` values attached to this `mod` item.
    pub(super) custom_paths: Vec<String>,
    pub(super) is_conditional_path: bool,
}

/// Extracts external `mod <name>;` declarations WITH their enclosing inline `mod outer { ... }`
/// ancestry (including ALL inline `#[path = "..."]` / `#[cfg_attr(..., path = "...")]` overrides)
/// and ALL preceding `#[path = "..."]` / `#[cfg_attr(..., path = "...")]` attributes, plus the set
/// of all inline module full prefixes (`"foo::"`, `"outer::foo::"`).
pub(super) fn extract_rust_external_mods_with_ancestry(
    source: &str,
) -> (Vec<RustExternalModWithAncestry>, BTreeSet<String>) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut brace_stack: Vec<Option<(String, Vec<String>, bool)>> = Vec::new();
    let mut external_mods: Vec<RustExternalModWithAncestry> = Vec::new();
    let mut inline_prefixes: BTreeSet<String> = BTreeSet::new();
    let mut pending_path_attrs: Vec<String> = Vec::new();
    let mut pending_conditional_path = false;
    let is_ident_start = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let is_ident_cont = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    while i < len {
        let next = skip_rust_ws_and_comments(bytes, i);
        if next >= len {
            break;
        }
        i = next;
        // Skip string literals
        if bytes[i] == b'"' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(len);
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
        // Raw string literal r#"..."#
        if bytes[i] == b'r' && i + 1 < len && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#') {
            let mut k = i + 1;
            let mut hashes = 0usize;
            while k < len && bytes[k] == b'#' {
                hashes += 1;
                k += 1;
            }
            if k < len && bytes[k] == b'"' {
                k += 1;
                while k < len {
                    if bytes[k] == b'"' {
                        let mut h = 0usize;
                        let mut m = k + 1;
                        while h < hashes && m < len && bytes[m] == b'#' {
                            h += 1;
                            m += 1;
                        }
                        if h == hashes {
                            k = m;
                            break;
                        }
                    }
                    k += 1;
                }
                i = k;
                continue;
            }
        }
        // Char literal 'x'
        if bytes[i] == b'\'' {
            let mut k = i + 1;
            if k < len && bytes[k] == b'\\' {
                k = (k + 2).min(len);
                if k < len && bytes[k] == b'\'' {
                    i = k + 1;
                    continue;
                }
            } else if k + 1 < len && bytes[k + 1] == b'\'' {
                i = k + 2;
                continue;
            }
            i += 1;
            continue;
        }
        // Outer attribute `#[...]` (allowing whitespace/comments between `#` and `[`, e.g.
        // `# [path = "real.rs"]`) — capture ALL `path = "..."` values (including raw strings
        // `r"..."` and multiple `cfg_attr(..., path = "...")` attributes without overwriting).
        if bytes[i] == b'#' {
            let bracket_pos = skip_rust_ws_and_comments(bytes, i + 1);
            if bracket_pos < len && bytes[bracket_pos] == b'[' {
                let attr_start = bracket_pos + 1;
                let mut depth = 1usize;
                let mut k = attr_start;
                while k < len && depth > 0 {
                    if bytes[k] == b'"' {
                        k += 1;
                        while k < len {
                            if bytes[k] == b'\\' {
                                k = (k + 2).min(len);
                                continue;
                            }
                            if bytes[k] == b'"' {
                                k += 1;
                                break;
                            }
                            k += 1;
                        }
                        continue;
                    }
                    if bytes[k] == b'[' {
                        depth += 1;
                    } else if bytes[k] == b']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    k += 1;
                }
                let attr_body = source[attr_start..k.min(len)].trim();
                let is_cfg_attr = attr_body.starts_with("cfg_attr");
                let mut search_pos = 0usize;
                while let Some(rel_pos) = attr_body[search_pos..].find("path") {
                    let pos = search_pos + rel_pos;
                    search_pos = pos + 4;
                    let after = attr_body[pos + 4..].trim_start();
                    if let Some(after_eq) = after.strip_prefix('=') {
                        let after_eq = after_eq.trim_start();
                        let extracted = if let Some(after_q) = after_eq.strip_prefix('"') {
                            after_q.find('"').map(|end_q| after_q[..end_q].trim().to_string())
                        } else if let Some(after_r) = after_eq.strip_prefix('r') {
                            let hashes = after_r.bytes().take_while(|&b| b == b'#').count();
                            after_r[hashes..].strip_prefix('"').and_then(|rest| {
                                let end_marker = format!("\"{}", "#".repeat(hashes));
                                rest.find(&end_marker).map(|e| rest[..e].trim().to_string())
                            })
                        } else {
                            None
                        };
                        if let Some(p) = extracted
                            && !p.is_empty()
                        {
                            if !pending_path_attrs.contains(&p) {
                                pending_path_attrs.push(p);
                            }
                            // Only `#[cfg_attr(..., path = "...")]` makes the path selection itself
                            // conditional (so the module can fall back to its default filename when
                            // the cfg is inactive). An unconditional `#[path = "tests.rs"]` paired
                            // with `#[cfg(test)] mod tests;` has ONE definite path whenever enabled.
                            if is_cfg_attr {
                                pending_conditional_path = true;
                            }
                        }
                    }
                }
                i = (k + 1).min(len);
                continue;
            }
        }
        if bytes[i] == b'{' {
            pending_path_attrs.clear();
            pending_conditional_path = false;
            brace_stack.push(None);
            i += 1;
            continue;
        }
        if bytes[i] == b'}' {
            pending_path_attrs.clear();
            pending_conditional_path = false;
            brace_stack.pop();
            i += 1;
            continue;
        }
        if bytes[i] == b';' {
            pending_path_attrs.clear();
            pending_conditional_path = false;
            i += 1;
            continue;
        }
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < len && is_ident_cont(bytes[i]) {
                i += 1;
            }
            let kw = &bytes[start..i];
            if kw == b"pub" || kw == b"unsafe" || kw == b"crate" || kw == b"super" || kw == b"in" {
                continue;
            }
            if kw == b"mod" {
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
                        pending_path_attrs.clear();
                        pending_conditional_path = false;
                        continue;
                    };
                    let mut ne = ns + 1;
                    while ne < len && is_ident_cont(bytes[ne]) {
                        ne += 1;
                    }
                    let mod_name = String::from_utf8_lossy(&bytes[ns..ne]).into_owned();
                    let after_name = skip_rust_ws_and_comments(bytes, ne);
                    let ancestors: Vec<(String, Vec<String>, bool)> =
                        brace_stack.iter().flatten().cloned().collect();
                    let custom_paths = std::mem::take(&mut pending_path_attrs);
                    let is_conditional_path =
                        pending_conditional_path || custom_paths.len() > 1;
                    pending_conditional_path = false;
                    if after_name < len && bytes[after_name] == b';' {
                        external_mods.push(RustExternalModWithAncestry {
                            inline_ancestors: ancestors,
                            name: mod_name,
                            custom_paths,
                            is_conditional_path,
                        });
                        i = after_name + 1;
                        continue;
                    } else if after_name < len && bytes[after_name] == b'{' {
                        let mut full = String::new();
                        for (anc, _, _) in &ancestors {
                            full.push_str(anc);
                            full.push_str("::");
                        }
                        full.push_str(&mod_name);
                        full.push_str("::");
                        inline_prefixes.insert(full);
                        brace_stack.push(Some((
                            mod_name,
                            custom_paths,
                            is_conditional_path,
                        )));
                        i = after_name + 1;
                        continue;
                    }
                }
            }
            pending_path_attrs.clear();
            pending_conditional_path = false;
            continue;
        }
        i += 1;
    }
    (external_mods, inline_prefixes)
}

/// Iteratively walks external `mod` declarations (including enclosing inline `mod outer { mod helper; }`
/// ancestry, ALL inline `#[path = "..."]` / `#[cfg_attr(..., path = "...")]` overrides, and ALL
/// `#[path = "..."]` / `#[cfg_attr(..., path = "...")]` attributes) starting from a Cargo target
/// root file (`root_rel`, crate-relative).
/// Returns `(resolved_prefixes, is_ambiguous_or_incomplete)` where the boolean is `true` on:
/// - conditional inline+external module collision,
/// - conditional or multi-alternative `#[cfg_attr(..., path = "...")]` match,
/// - traversal depth limit exceeded (`depth > 64`) or true ancestor cycle encountered.
pub(super) fn resolve_rust_mod_prefixes_from_root(
    crate_root: &Path,
    root_rel: &str,
    target_rel: &str,
) -> (Vec<String>, bool) {
    let norm_target = target_rel.trim_start_matches("./").replace('\\', "/");
    let norm_root = root_rel.trim_start_matches("./").replace('\\', "/");
    let mut results: Vec<String> = Vec::new();
    let mut ambiguous = false;
    // (cur_rel, cur_pfx, is_crate_root, depth, inherited_ambiguity, ancestor_files)
    let mut stack: Vec<(String, String, bool, usize, bool, Vec<String>)> = vec![(
        norm_root.clone(),
        String::new(),
        true,
        0,
        false,
        vec![norm_root],
    )];
    let mut visited: BTreeSet<(String, String)> = BTreeSet::new();
    while let Some((cur_rel, cur_pfx, is_crate_root, depth, inherited_amb, chain)) = stack.pop() {
        if depth > 64 {
            // Pathologically deep module chain (>64 levels): fail closed rather than guessing.
            ambiguous = true;
            continue;
        }
        if !visited.insert((cur_rel.clone(), cur_pfx.clone())) {
            continue;
        }
        let Ok(bytes) = std::fs::read(crate_root.join(&cur_rel)) else {
            continue;
        };
        let source = String::from_utf8_lossy(&bytes);
        let (ext_mods, inline_prefixes) = extract_rust_external_mods_with_ancestry(&source);
        let cur_path = Path::new(&cur_rel);
        let cur_dir = cur_path
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let cur_stem = cur_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        // Only actual Cargo target roots (`is_crate_root`) and `mod.rs` resolve children in `cur_dir`;
        // non-root files named `lib.rs` or `main.rs` (e.g. `src/part/lib.rs`) resolve under `src/part/lib/`.
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
            let mut local_full = String::new();
            let mut any_inline_anc_cond = false;
            for (anc, _, anc_cond) in &m.inline_ancestors {
                local_full.push_str(anc);
                local_full.push_str("::");
                any_inline_anc_cond |= *anc_cond;
            }
            local_full.push_str(&m.name);
            local_full.push_str("::");
            let collides_with_inline = inline_prefixes.contains(&local_full);
            let branch_amb =
                inherited_amb || collides_with_inline || m.is_conditional_path || any_inline_anc_cond;
            let next_pfx = format!("{cur_pfx}{local_full}");

            // Per the Rust Reference:
            // - For a non-inline file `src/outer.rs`, `#[path = "shared.rs"] mod b;` resolves
            //   relative to the CONTAINING DIRECTORY of `src/outer.rs` (`cur_dir` = `src/`).
            // - Inside an inline module `mod outer { ... }`, preserve ALL `#[path = "..."]`
            //   alternatives AND the default `outer` directory whenever the inline path is conditional.
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
                next_dirs.sort();
                next_dirs.dedup();
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
            candidates.sort();
            candidates.dedup();
            for cand in candidates {
                let Some(norm_cand) = crate::ast_graph::normalize_lexical_rel(Path::new(&cand))
                else {
                    continue;
                };
                if norm_cand == norm_target {
                    if branch_amb {
                        ambiguous = true;
                    }
                    results.push(next_pfx.clone());
                } else if crate_root.join(&norm_cand).is_file() {
                    if chain.contains(&norm_cand) {
                        // True module cycle back to an ancestor file: fail closed without recursing.
                        ambiguous = true;
                    } else if !visited.contains(&(norm_cand.clone(), next_pfx.clone())) {
                        let mut next_chain = chain.clone();
                        next_chain.push(norm_cand.clone());
                        stack.push((
                            norm_cand,
                            next_pfx.clone(),
                            false,
                            depth + 1,
                            branch_amb,
                            next_chain,
                        ));
                    }
                }
            }
        }
    }
    results.sort();
    results.dedup();
    (results, ambiguous)
}

pub(super) fn auto_discovered_rust_bin_roots(crate_root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bin_dir = crate_root.join("src/bin");
    let Ok(entries) = std::fs::read_dir(&bin_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file()
            && p.extension().and_then(|e| e.to_str()) == Some("rs")
            && let Some(fname) = p.file_name().and_then(|n| n.to_str())
            && let Some(stem) = fname.strip_suffix(".rs")
        {
            out.push((format!("src/bin/{fname}"), stem.to_string()));
        } else if p.is_dir()
            && p.join("main.rs").is_file()
            && let Some(dname) = p.file_name().and_then(|n| n.to_str())
        {
            out.push((format!("src/bin/{dname}/main.rs"), dname.to_string()));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RustTargetChildOwnership {
    None,
    UniqueTest(String, String),
    UniqueBin(String, String),
    /// Multiple binaries mount this shared module under `--bins`; execute every `(bin_name, mod_pfx)`.
    MultipleBins(Vec<(String, String)>),
    UniqueLib(String),
    Ambiguous,
}

pub(super) fn resolve_rust_child_target_ownership(
    crate_root: &Path,
    manifest: &CargoManifestTargets,
    rel_in: &str,
    explicit_cargo_target: Option<&str>,
) -> RustTargetChildOwnership {
    let restrict_lib = explicit_cargo_target == Some("lib");
    let restrict_bins = explicit_cargo_target == Some("bins");
    let restrict_bin_name = explicit_cargo_target.and_then(|t| t.strip_prefix("bin:"));
    let restrict_test_name = explicit_cargo_target.and_then(|t| t.strip_prefix("test:"));
    let include_lib = restrict_lib || explicit_cargo_target.is_none();
    let include_bins = restrict_bins || restrict_bin_name.is_some() || explicit_cargo_target.is_none();
    let include_tests = restrict_test_name.is_some() || explicit_cargo_target.is_none();

    let mut owners: Vec<(u8, String, String)> = Vec::new(); // (kind: 0=lib, 1=bin, 2=test, target, mod_pfx)
    let mut any_ambiguous = false;
    if include_lib
        && let Some(ref lp) = manifest.lib_path
    {
        let (pfxs, amb) = resolve_rust_mod_prefixes_from_root(crate_root, lp, rel_in);
        any_ambiguous |= amb;
        for pfx in pfxs {
            owners.push((0, String::new(), pfx));
        }
    }
    if include_bins {
        for (bin_root, bin_names) in &manifest.custom_bins {
            for bin_name in bin_names {
                if restrict_bin_name.is_some_and(|wanted| wanted != bin_name.as_str()) {
                    continue;
                }
                let (pfxs, amb) = resolve_rust_mod_prefixes_from_root(crate_root, bin_root, rel_in);
                any_ambiguous |= amb;
                for pfx in pfxs {
                    owners.push((1, bin_name.clone(), pfx));
                }
            }
        }
        if manifest.autobins {
            for (bin_root, bin_name) in auto_discovered_rust_bin_roots(crate_root) {
                if restrict_bin_name.is_some_and(|wanted| wanted != bin_name.as_str()) {
                    continue;
                }
                if bin_root != rel_in && !manifest.custom_bins.contains_key(&bin_root) {
                    let (pfxs, amb) =
                        resolve_rust_mod_prefixes_from_root(crate_root, &bin_root, rel_in);
                    any_ambiguous |= amb;
                    for pfx in pfxs {
                        owners.push((1, bin_name.clone(), pfx));
                    }
                }
            }
        }
        if let Some(ref main_name) = manifest.main_bin
            && restrict_bin_name.is_none_or(|wanted| wanted == main_name.as_str())
            && crate_root.join("src/main.rs").is_file()
            && !manifest.custom_bins.contains_key("src/main.rs")
        {
            let (pfxs, amb) =
                resolve_rust_mod_prefixes_from_root(crate_root, "src/main.rs", rel_in);
            any_ambiguous |= amb;
            for pfx in pfxs {
                owners.push((1, main_name.clone(), pfx));
            }
        }
    }
    if include_tests {
        for (test_root, test_names) in &manifest.custom_tests {
            for test_name in test_names {
                if restrict_test_name.is_some_and(|wanted| wanted != test_name.as_str()) {
                    continue;
                }
                let (pfxs, amb) = resolve_rust_mod_prefixes_from_root(crate_root, test_root, rel_in);
                any_ambiguous |= amb;
                for pfx in pfxs {
                    owners.push((2, test_name.clone(), pfx));
                }
            }
        }
        // Also check standard auto-discovered integration test roots (`tests/<name>/main.rs` and
        // `tests/<name>.rs`) ONLY when `autotests` is enabled in Cargo.toml.
        if manifest.autotests
            && let Some(rest) = rel_in.strip_prefix("tests/")
        {
            if let Some((dir_name, _)) = rest.split_once('/')
                && restrict_test_name.is_none_or(|wanted| wanted == dir_name)
            {
                let nested_root = format!("tests/{dir_name}/main.rs");
                if crate_root.join(&nested_root).is_file() {
                    let (pfxs, amb) =
                        resolve_rust_mod_prefixes_from_root(crate_root, &nested_root, rel_in);
                    any_ambiguous |= amb;
                    for pfx in pfxs {
                        owners.push((2, dir_name.to_string(), pfx));
                    }
                }
            }
            if let Ok(entries) = std::fs::read_dir(crate_root.join("tests")) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file()
                        && p.extension().and_then(|e| e.to_str()) == Some("rs")
                        && let Some(fname) = p.file_name().and_then(|n| n.to_str())
                        && let Some(stem) = fname.strip_suffix(".rs")
                        && restrict_test_name.is_none_or(|wanted| wanted == stem)
                    {
                        let root_rel = format!("tests/{fname}");
                        if root_rel != rel_in {
                            let (pfxs, amb) =
                                resolve_rust_mod_prefixes_from_root(crate_root, &root_rel, rel_in);
                            any_ambiguous |= amb;
                            for pfx in pfxs {
                                owners.push((2, stem.to_string(), pfx));
                            }
                        }
                    }
                }
            }
        }
    }
    if any_ambiguous {
        return RustTargetChildOwnership::Ambiguous;
    }
    owners.sort();
    owners.dedup();
    match owners.as_slice() {
        [] => RustTargetChildOwnership::None,
        [(0, _, pfx)] => RustTargetChildOwnership::UniqueLib(pfx.clone()),
        [(1, bin, pfx)] => RustTargetChildOwnership::UniqueBin(bin.clone(), pfx.clone()),
        [(2, test, pfx)] => RustTargetChildOwnership::UniqueTest(test.clone(), pfx.clone()),
        slice
            if explicit_cargo_target == Some("bins")
                && slice.iter().all(|(kind, _, _)| *kind == 1) =>
        {
            RustTargetChildOwnership::MultipleBins(
                slice
                    .iter()
                    .map(|(_, bin, pfx)| (bin.clone(), pfx.clone()))
                    .collect(),
            )
        }
        _ => RustTargetChildOwnership::Ambiguous,
    }
}

/// Returns true when a Rust file is shared across multiple distinct Cargo targets (e.g. a shared
/// `common.rs` mounted by both `lib.rs` and `main.rs`, or a source file declared as both `[lib]`
/// and `[[bin]]`), so single-key file-level cache entries cannot be reused across targets.
pub(super) fn rust_file_is_shared_across_targets(root: &Path, test_file: &str) -> bool {
    let crate_dir = rust_crate_dir_on_disk(root, test_file);
    let workdir = if crate_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&crate_dir)
    };
    let rel_in = rel_path_in_crate(&crate_dir, test_file);
    let manifest = parse_cargo_manifest_targets(&workdir);
    let mut target_root_count = 0usize;
    if manifest.lib_path.as_deref() == Some(rel_in) {
        target_root_count += 1;
    }
    if let Some(bin_names) = manifest.custom_bins.get(rel_in) {
        target_root_count += bin_names.len();
    } else if manifest.main_bin.is_some() && rel_in == "src/main.rs" {
        // Count auto-discovered `src/main.rs` binary root (e.g. when `[lib] path = "src/main.rs"`
        // shares the path with the auto-discovered package binary!).
        target_root_count += 1;
    } else if manifest.autobins
        && auto_discovered_rust_bin_roots(&workdir)
            .iter()
            .any(|(p, _)| p == rel_in)
    {
        target_root_count += 1;
    }
    if let Some(test_names) = manifest.custom_tests.get(rel_in) {
        target_root_count += test_names.len();
    } else if manifest.autotests
        && crate::ast_graph::rust_integration_test_target_name(test_file).is_some()
    {
        target_root_count += 1;
    }
    if target_root_count > 1 {
        return true;
    }
    let child_ownership =
        resolve_rust_child_target_ownership(&workdir, &manifest, rel_in, None);
    if target_root_count == 1
        && !matches!(child_ownership, RustTargetChildOwnership::None)
    {
        return true;
    }
    matches!(child_ownership, RustTargetChildOwnership::Ambiguous)
}

/// Attempts to uniquely resolve the declared module prefix (e.g. `renamed::`) for `test_file`
/// when its crate uses `#[path = "..."]` module overrides. Returns `None` if ambiguous/unresolved.
pub(super) fn custom_mod_prefix_for_file(root: &Path, crate_dir: &str, test_file: &str) -> Option<String> {
    let crate_root = if crate_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(crate_dir)
    };
    let crate_rel = rel_path_in_crate(crate_dir, test_file);
    let manifest = parse_cargo_manifest_targets(&crate_root);
    let mut matches = Vec::new();
    let mut any_ambiguous = false;
    let mut roots: Vec<String> = Vec::new();
    if let Some(ref lp) = manifest.lib_path {
        roots.push(lp.clone());
    }
    if manifest.main_bin.is_some() && crate_root.join("src/main.rs").is_file() {
        roots.push("src/main.rs".to_string());
    }
    if manifest.autobins {
        for (bin_root, _) in auto_discovered_rust_bin_roots(&crate_root) {
            roots.push(bin_root);
        }
    }
    for bin_root in manifest.custom_bins.keys() {
        roots.push(bin_root.clone());
    }
    for root_file in roots {
        let (pfxs, amb) = resolve_rust_mod_prefixes_from_root(&crate_root, &root_file, crate_rel);
        any_ambiguous |= amb;
        matches.extend(pfxs);
    }
    if any_ambiguous {
        return None;
    }
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

/// Resolves the owning Cargo `--test <target>` name (and optional submodule prefix) for a Rust
/// integration test file, covering flat `tests/<name>.rs`, nested `tests/<name>/main.rs`, and
/// submodules inside a nested integration test (`tests/<name>/helper.rs` when `tests/<name>/main.rs`
/// exists).
pub(super) fn rust_integration_test_owner(
    root: &Path,
    crate_dir: &str,
    test_file: &str,
) -> Option<(String, Option<String>)> {
    if let Some(target) = crate::ast_graph::rust_integration_test_target_name(test_file) {
        return Some((target, None));
    }
    if !test_file.ends_with(".rs") || crate::ast_graph::is_rust_common_test_helper(test_file) {
        return None;
    }
    let rest = if let Some(r) = test_file.strip_prefix("tests/") {
        r
    } else if let Some((_, r)) = test_file.rsplit_once("/tests/") {
        r
    } else {
        return None;
    };
    let (dir_name, sub_rel) = rest.split_once('/')?;
    if dir_name.is_empty() || sub_rel.is_empty() {
        return None;
    }
    let workdir = if crate_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(crate_dir)
    };
    if !workdir.join("tests").join(dir_name).join("main.rs").is_file() {
        return None;
    }
    if sub_rel == "main.rs" {
        return Some((dir_name.to_string(), None));
    }
    let stripped = if let Some(s) = sub_rel.strip_suffix("/mod.rs") {
        s
    } else {
        sub_rel.strip_suffix(".rs")?
    };
    let mod_pfx = if stripped.is_empty() {
        None
    } else {
        Some(format!("{}::", stripped.replace('/', "::")))
    };
    Some((dir_name.to_string(), mod_pfx))
}

pub(super) fn rust_module_filter_for_file(test_file: &str) -> Option<String> {
    let rel_after_src = if let Some(rest) = test_file.strip_prefix("src/") {
        rest
    } else if let Some((_, rest)) = test_file.rsplit_once("/src/") {
        rest
    } else {
        return None;
    };
    if matches!(rel_after_src, "lib.rs" | "main.rs") {
        return None;
    }
    if let Some(bin_rel) = rel_after_src.strip_prefix("bin/") {
        // `src/bin/cli.rs` and `src/bin/cli/main.rs` are binary roots (no module prefix);
        // nested binary modules like `src/bin/cli/helper.rs` have prefix `helper::`.
        let (_, sub_rel) = bin_rel.split_once('/')?;
        if sub_rel == "main.rs" {
            return None;
        }
        let stripped = if let Some(s) = sub_rel.strip_suffix("/mod.rs") {
            s
        } else {
            sub_rel.strip_suffix(".rs")?
        };
        return (!stripped.is_empty()).then(|| format!("{}::", stripped.replace('/', "::")));
    }
    let stripped = if let Some(s) = rel_after_src.strip_suffix("/mod.rs") {
        s
    } else {
        rel_after_src.strip_suffix(".rs")?
    };
    if stripped.is_empty() {
        None
    } else {
        Some(format!("{}::", stripped.replace('/', "::")))
    }
}


