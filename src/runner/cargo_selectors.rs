use super::*;

/// Reads a Rust identifier at byte index `pos` (skipping whitespace AND Rust `//`/`/* ... */`
/// comments like `fn /* comment */ fails()`!), preserving raw-identifier `r#` prefix (e.g.
/// `r#type`, `r#match`) because `rustc --test` / libtest retains `r#` in test paths!
pub(super) fn read_rust_ident(source: &str, b: &[u8], pos: usize) -> Option<(String, usize)> {
    let len = b.len();
    let mut p = pos;
    while p < len {
        if b[p].is_ascii_whitespace() {
            p += 1;
        } else if b[p] == b'/' && p + 1 < len && b[p + 1] == b'/' {
            while p < len && b[p] != b'\n' {
                p += 1;
            }
        } else if b[p] == b'/' && p + 1 < len && b[p + 1] == b'*' {
            let mut nest = 1usize;
            p += 2;
            while p + 1 < len && nest > 0 {
                if b[p] == b'/' && b[p + 1] == b'*' {
                    nest += 1;
                    p += 2;
                } else if b[p] == b'*' && b[p + 1] == b'/' {
                    nest -= 1;
                    p += 2;
                } else {
                    p += 1;
                }
            }
            if nest > 0 {
                return None;
            }
        } else {
            break;
        }
    }
    if p >= len {
        return None;
    }
    let ns = p;
    if p + 2 < len && b[p] == b'r' && b[p + 1] == b'#' {
        let mut q = p + 2;
        if let Some(ch) = source[q..].chars().next()
            && (ch == '_' || ch.is_alphabetic())
        {
            while let Some(c) = source[q..].chars().next() {
                if c == '_' || c.is_alphanumeric() {
                    q += c.len_utf8();
                } else {
                    break;
                }
            }
            return Some((source[ns..q].to_string(), q));
        }
        return None;
    }
    if let Some(ch) = source[p..].chars().next()
        && (ch == '_' || ch.is_alphabetic())
    {
        let mut q = p;
        while let Some(c) = source[q..].chars().next() {
            if c == '_' || c.is_alphanumeric() {
                q += c.len_utf8();
            } else {
                break;
            }
        }
        return Some((source[ns..q].to_string(), q));
    }
    None
}

/// Walks the Rust module tree starting at `target_root_rel` following `mod <name>;` declarations
/// AND `#[path = "..."]` attributes (including inside inline `mod <name> { ... }` blocks!) to
/// compute the exact module path prefix(es) under which `file_rel` is mounted in the crate.
/// Returns `(prefixes, unresolved_or_dynamic)`.
pub(super) fn resolve_rust_file_mod_prefixes(
    crate_root: &Path,
    target_root_rel: &str,
    file_rel: &str,
) -> (Vec<Vec<String>>, bool) {
    let norm_root = target_root_rel.trim_start_matches("./").to_string();
    let norm_target = file_rel.trim_start_matches("./").to_string();
    if norm_root == norm_target {
        return (vec![Vec::new()], false);
    }
    let mut found: Vec<Vec<String>> = Vec::new();
    let mut dynamic = false;
    let mut visited: BTreeSet<(String, Vec<String>)> = BTreeSet::new();
    let mut stack: Vec<(String, Vec<String>, bool)> = vec![(norm_root.clone(), Vec::new(), true)];
    while let Some((cur_file, cur_prefix, is_crate_root)) = stack.pop() {
        if !visited.insert((cur_file.clone(), cur_prefix.clone())) || visited.len() > 256 {
            continue;
        }
        let Ok(bytes) = std::fs::read(crate_root.join(&cur_file)) else {
            continue;
        };
        let src = String::from_utf8_lossy(&bytes);
        let b = src.as_bytes();
        let len = b.len();
        let mut i = 0usize;
        let mut depth: usize = 0;
        let mut inline_mods: Vec<(usize, String, Option<String>)> = Vec::new();
        let mut pending_mod_name: Option<String> = None;
        let mut pending_path_attr: Option<(Option<String>, usize)> = None;
        let cur_dir = Path::new(&cur_file)
            .parent()
            .map(normalize_rel_str)
            .unwrap_or_default();
        let cur_is_mod_rs = is_crate_root || cur_file.ends_with("/mod.rs") || cur_file == "mod.rs";
        let cur_stem = Path::new(&cur_file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        while i < len {
            if b[i] == b'/' && i + 1 < len && b[i + 1] == b'/' {
                while i < len && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if b[i] == b'/' && i + 1 < len && b[i + 1] == b'*' {
                let mut nest = 1usize;
                i += 2;
                while i + 1 < len && nest > 0 {
                    if b[i] == b'/' && b[i + 1] == b'*' {
                        nest += 1;
                        i += 2;
                    } else if b[i] == b'*' && b[i + 1] == b'/' {
                        nest -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            if b[i] == b'"' {
                i += 1;
                while i < len && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < len {
                        i += 1;
                        let ch = src[i..].chars().next().unwrap_or(' ');
                        i += ch.len_utf8();
                        continue;
                    }
                    let ch = src[i..].chars().next().unwrap_or(' ');
                    i += ch.len_utf8();
                }
                if i < len {
                    i += 1;
                }
                continue;
            }
            if b[i] == b'#' && i + 1 < len && b[i + 1] == b'[' {
                let start = i;
                let mut bd = 0usize;
                while i < len {
                    if b[i] == b'[' {
                        bd += 1;
                    } else if b[i] == b']' {
                        bd = bd.saturating_sub(1);
                        if bd == 0 {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
                let attr = &src[start..i];
                let inner = attr.trim_start_matches("#[").trim_end_matches(']').trim();
                if let Some(rest) = inner.strip_prefix("path")
                    && let Some(val_part) = rest.trim_start().strip_prefix('=')
                {
                    let vp = val_part.trim();
                    if let Some(lit) = vp.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                        pending_path_attr = Some((Some(decode_toml_basic_string(lit)), depth));
                    } else {
                        // Non-literal `#[path = ...]` (e.g. macro/concat): fail closed!
                        pending_path_attr = Some((None, depth));
                    }
                }
                continue;
            }
            if b[i] == b'{' {
                depth += 1;
                if let Some(mname) = pending_mod_name.take() {
                    let pa = pending_path_attr
                        .take()
                        .filter(|(_, d)| *d == depth - 1)
                        .and_then(|(p, _)| p);
                    inline_mods.push((depth, mname, pa));
                }
                i += 1;
                continue;
            }
            if b[i] == b'}' {
                if inline_mods.last().is_some_and(|(d, _, _)| *d == depth) {
                    inline_mods.pop();
                }
                depth = depth.saturating_sub(1);
                i += 1;
                continue;
            }
            if b[i] == b';' {
                if let Some(mname) = pending_mod_name.take() {
                    // External module declaration `mod <mname>;` (optionally with `#[path = "..."]`)
                    let attr_for_mod = pending_path_attr.take().filter(|(_, d)| *d == depth);
                    // Compute effective directory for child lookup considering `cur_is_mod_rs` &
                    // enclosing inline `mod` blocks.
                    let mut search_dir = if cur_is_mod_rs {
                        cur_dir.clone()
                    } else if cur_dir.is_empty() {
                        cur_stem.to_string()
                    } else {
                        format!("{cur_dir}/{cur_stem}")
                    };
                    for (_, im, ipa) in &inline_mods {
                        let im_fs = im.strip_prefix("r#").unwrap_or(im.as_str());
                        if let Some(custom) = ipa {
                            search_dir = if search_dir.is_empty() {
                                custom.clone()
                            } else {
                                format!("{search_dir}/{custom}")
                            };
                        } else if search_dir.is_empty() {
                            search_dir = im_fs.to_string();
                        } else {
                            search_dir = format!("{search_dir}/{im_fs}");
                        }
                    }
                    let child_rel = if let Some((pa_opt, _)) = attr_for_mod {
                        if let Some(custom_path) = pa_opt {
                            let base_for_attr = if cur_dir.is_empty() {
                                PathBuf::from(&custom_path)
                            } else if inline_mods.is_empty() {
                                Path::new(&cur_dir).join(&custom_path)
                            } else {
                                Path::new(&search_dir).join(&custom_path)
                            };
                            // Lexically normalize `..` and `.` segments (e.g.
                            // `#[path = "../src/checks.rs"]` from `examples/` -> `src/checks.rs`)
                            // consistently with `collect_rust_target_tree_files`!
                            let mut norm_parts: Vec<String> = Vec::new();
                            let mut escaped = false;
                            for comp in base_for_attr.components() {
                                match comp {
                                    std::path::Component::CurDir => {}
                                    std::path::Component::Normal(os) => {
                                        norm_parts.push(os.to_string_lossy().into_owned());
                                    }
                                    std::path::Component::ParentDir => {
                                        if norm_parts.pop().is_none() {
                                            escaped = true;
                                        }
                                    }
                                    _ => {
                                        escaped = true;
                                    }
                                }
                            }
                            if escaped || norm_parts.is_empty() {
                                dynamic = true;
                                String::new()
                            } else {
                                norm_parts.join("/")
                            }
                        } else {
                            dynamic = true;
                            String::new()
                        }
                    } else {
                        // Rust strips `r#` when mapping `mod r#type;` to filesystem `type.rs` /
                        // `type/mod.rs`, while libtest retains `r#type::...` in test names!
                        let fs_name = mname.strip_prefix("r#").unwrap_or(&mname);
                        let cand1 = if search_dir.is_empty() {
                            format!("{fs_name}.rs")
                        } else {
                            format!("{search_dir}/{fs_name}.rs")
                        };
                        let cand2 = if search_dir.is_empty() {
                            format!("{fs_name}/mod.rs")
                        } else {
                            format!("{search_dir}/{fs_name}/mod.rs")
                        };
                        if crate_root.join(&cand1).is_file() {
                            cand1
                        } else if crate_root.join(&cand2).is_file() {
                            cand2
                        } else {
                            String::new()
                        }
                    };
                    if !child_rel.is_empty() {
                        let mut next_prefix = cur_prefix.clone();
                        for (_, im, _) in &inline_mods {
                            next_prefix.push(im.clone());
                        }
                        next_prefix.push(mname);
                        let norm_child = child_rel.trim_start_matches("./").to_string();
                        if norm_child == norm_target {
                            if !found.contains(&next_prefix) {
                                found.push(next_prefix);
                            }
                        } else if crate_root.join(&norm_child).is_file() {
                            stack.push((norm_child, next_prefix, false));
                        }
                    }
                } else {
                    pending_path_attr = None;
                }
                i += 1;
                continue;
            }
            if b[i].is_ascii_alphabetic() || b[i] == b'_' {
                let s = i;
                while i < len && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let ident = &src[s..i];
                if ident == "mod" {
                    if let Some((mname, next_p)) = read_rust_ident(&src, b, i) {
                        pending_mod_name = Some(mname);
                        i = next_p;
                    }
                } else if ident == "include" && src[i..].trim_start().starts_with('!') {
                    dynamic = true;
                }
                continue;
            }
            let ch = src[i..].chars().next().unwrap_or(' ');
            i += ch.len_utf8();
        }
    }
    if found.is_empty() {
        dynamic = true;
    }
    (found, dynamic)
}

/// Resolves exact fully-qualified libtest test names (respecting actual target root, `#[path]`
/// module aliases, and inline `mod <name> { ... }` nesting!) for a single `.rs` file.
/// Returns `(fq_names, has_dynamic_or_parameterized_tests)`.
pub fn collect_rust_fq_test_names_in_file(
    crate_root: &Path,
    target_root_rel: &str,
    file_rel: &str,
) -> (Vec<String>, bool) {
    let Ok(bytes) = std::fs::read(crate_root.join(file_rel)) else {
        return (Vec::new(), true);
    };
    let source = String::from_utf8_lossy(&bytes);
    let mut has_dynamic = rust_source_has_module_level_macro(&source);
    let (file_mod_prefixes, mod_dynamic) =
        resolve_rust_file_mod_prefixes(crate_root, target_root_rel, file_rel);
    if mod_dynamic {
        has_dynamic = true;
    }
    if file_mod_prefixes.is_empty() {
        return (Vec::new(), true);
    }
    let b = source.as_bytes();
    let len = b.len();
    let mut i = 0usize;
    let mut depth: usize = 0;
    let mut inline_mod_stack: Vec<(usize, String)> = Vec::new();
    let mut pending_mod_name: Option<String> = None;
    let mut pending_test_attr = false;
    let mut pending_test_attr_depth: usize = 0;
    let mut out: Vec<String> = Vec::new();
    while i < len {
        // Line comment
        if b[i] == b'/' && i + 1 < len && b[i + 1] == b'/' {
            while i < len && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment (Rust allows nested `/* ... */`)
        if b[i] == b'/' && i + 1 < len && b[i + 1] == b'*' {
            let mut nest = 1usize;
            i += 2;
            while i + 1 < len && nest > 0 {
                if b[i] == b'/' && b[i + 1] == b'*' {
                    nest += 1;
                    i += 2;
                } else if b[i] == b'*' && b[i + 1] == b'/' {
                    nest -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // Raw string `r#"..."#` / `br#"..."#`
        if (b[i] == b'r' || (b[i] == b'b' && i + 1 < len && b[i + 1] == b'r'))
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
        {
            let r_start = if b[i] == b'b' { i + 1 } else { i };
            let mut p = r_start + 1;
            while p < len && b[p] == b'#' {
                p += 1;
            }
            if p < len && b[p] == b'"' {
                let hashes = p - (r_start + 1);
                p += 1;
                while p < len {
                    if b[p] == b'"' {
                        let mut h = 0usize;
                        while h < hashes && p + 1 + h < len && b[p + 1 + h] == b'#' {
                            h += 1;
                        }
                        if h == hashes {
                            p += 1 + hashes;
                            break;
                        }
                    }
                    p += 1;
                }
                i = p;
                continue;
            }
        }
        // Normal string
        if b[i] == b'"' {
            i += 1;
            while i < len && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < len {
                    i += 1;
                    let ch = source[i..].chars().next().unwrap_or(' ');
                    i += ch.len_utf8();
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
        // Char literal `'a'` / `'\n'` (distinguish from lifetimes `'a`!)
        if b[i] == b'\'' {
            if i + 2 < len && b[i + 1] == b'\\' {
                let mut p = i + 2;
                while p < len && b[p] != b'\'' && b[p] != b'\n' && p - i <= 12 {
                    p += 1;
                }
                if p < len && b[p] == b'\'' {
                    i = p + 1;
                    continue;
                }
            } else if i + 1 < len {
                let ch = source[i + 1..].chars().next().unwrap_or(' ');
                let next_p = i + 1 + ch.len_utf8();
                if next_p < len && b[next_p] == b'\'' {
                    i = next_p + 1;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        // Attribute `#[...]` (skipping strings and `//` / nested `/* ... */` comments so
        // `#[test /* comment */]` and `#[doc = "]"]` are parsed accurately!)
        if b[i] == b'#' && i + 1 < len && b[i + 1] == b'[' {
            let mut bracket_depth = 0usize;
            let mut attr_no_comments = String::new();
            while i < len {
                if b[i] == b'/' && i + 1 < len && b[i + 1] == b'/' {
                    while i < len && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                if b[i] == b'/' && i + 1 < len && b[i + 1] == b'*' {
                    let mut nest = 1usize;
                    i += 2;
                    while i + 1 < len && nest > 0 {
                        if b[i] == b'/' && b[i + 1] == b'*' {
                            nest += 1;
                            i += 2;
                        } else if b[i] == b'*' && b[i + 1] == b'/' {
                            nest -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    continue;
                }
                // Skip Rust raw strings (`r#"..."#` / `br#"..."#`) inside attributes so
                // `#[doc = r#"a " ] b"#]` NEVER closes the attribute early!
                if (b[i] == b'r' || (b[i] == b'b' && i + 1 < len && b[i + 1] == b'r'))
                    && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
                {
                    let r_start = if b[i] == b'b' { i + 1 } else { i };
                    let mut p = r_start + 1;
                    while p < len && b[p] == b'#' {
                        p += 1;
                    }
                    if p < len && b[p] == b'"' {
                        let hashes = p - (r_start + 1);
                        p += 1;
                        while p < len {
                            if b[p] == b'"' {
                                let mut h = 0usize;
                                while h < hashes && p + 1 + h < len && b[p + 1 + h] == b'#' {
                                    h += 1;
                                }
                                if h == hashes {
                                    p += 1 + hashes;
                                    break;
                                }
                            }
                            p += 1;
                        }
                        attr_no_comments.push_str("\"\"");
                        i = p;
                        continue;
                    }
                }
                if b[i] == b'"' {
                    attr_no_comments.push('"');
                    i += 1;
                    while i < len && b[i] != b'"' {
                        if b[i] == b'\\' && i + 1 < len {
                            attr_no_comments.push('\\');
                            i += 1;
                            let ch = source[i..].chars().next().unwrap_or(' ');
                            attr_no_comments.push(ch);
                            i += ch.len_utf8();
                            continue;
                        }
                        let ch = source[i..].chars().next().unwrap_or(' ');
                        attr_no_comments.push(ch);
                        i += ch.len_utf8();
                    }
                    if i < len {
                        attr_no_comments.push('"');
                        i += 1;
                    }
                    continue;
                }
                if b[i] == b'[' {
                    bracket_depth += 1;
                    attr_no_comments.push('[');
                    i += 1;
                } else if b[i] == b']' {
                    bracket_depth = bracket_depth.saturating_sub(1);
                    attr_no_comments.push(']');
                    i += 1;
                    if bracket_depth == 0 {
                        break;
                    }
                } else {
                    let ch = source[i..].chars().next().unwrap_or(' ');
                    if !ch.is_whitespace() {
                        attr_no_comments.push(ch);
                    }
                    i += ch.len_utf8();
                }
            }
            let compact = attr_no_comments;
            // Parameterized / proc-macro test attributes (`#[rstest]`, `#[test_case]`, `#[case]`,
            // `#[values]`, `#[quickcheck]`, `#[proptest]`) generate suffixed libtest names like
            // `fails::case_1` that do NOT match `--exact fails` -> mark dynamic/unresolvable!
            if compact.starts_with("#[rstest")
                || compact.starts_with("#[test_case")
                || compact.contains("::rstest")
                || compact.contains("::test_case")
                || compact.starts_with("#[case(")
                || compact.starts_with("#[quickcheck")
                || compact.starts_with("#[proptest")
            {
                has_dynamic = true;
                pending_test_attr = false;
            } else if compact == "#[test]"
                || compact.starts_with("#[test(")
                || compact.ends_with("::test]")
                || compact.contains("::test(")
                || (compact.starts_with("#[cfg_attr(")
                    && (compact.contains(",test")
                        || compact.contains("::test)")
                        || compact.contains("::test,")
                        || compact.contains("::test(")))
            {
                pending_test_attr = true;
                pending_test_attr_depth = depth;
            }
            continue;
        }
        if b[i] == b'{' {
            depth += 1;
            if let Some(mname) = pending_mod_name.take() {
                inline_mod_stack.push((depth, mname));
            }
            i += 1;
            continue;
        }
        if b[i] == b'}' {
            if inline_mod_stack.last().is_some_and(|(d, _)| *d == depth) {
                inline_mod_stack.pop();
            }
            depth = depth.saturating_sub(1);
            if pending_test_attr && depth < pending_test_attr_depth {
                pending_test_attr = false;
            }
            i += 1;
            continue;
        }
        if b[i] == b';' {
            pending_mod_name = None;
            if pending_test_attr && depth == pending_test_attr_depth {
                pending_test_attr = false;
            }
            i += 1;
            continue;
        }
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let s = i;
            while i < len && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let ident = &source[s..i];
            if ident == "mod" {
                if let Some((mname, next_p)) = read_rust_ident(&source, b, i) {
                    pending_mod_name = Some(mname);
                    i = next_p;
                }
                continue;
            }
            if ident == "fn" && pending_test_attr && depth == pending_test_attr_depth {
                if let Some((fn_name, next_p)) = read_rust_ident(&source, b, i) {
                    for fmp in &file_mod_prefixes {
                        let mut parts: Vec<&str> = fmp.iter().map(String::as_str).collect();
                        for (_, m) in &inline_mod_stack {
                            parts.push(m.as_str());
                        }
                        parts.push(fn_name.as_str());
                        let fq = parts.join("::");
                        if !out.contains(&fq) {
                            out.push(fq);
                        }
                    }
                    pending_test_attr = false;
                    i = next_p;
                } else {
                    // Unresolvable test declaration (e.g. macro-expanded fn name): fail closed!
                    has_dynamic = true;
                    pending_test_attr = false;
                }
                continue;
            }
            continue;
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        i += ch.len_utf8();
    }
    (out, has_dynamic)
}

/// Resolves which Cargo target selector(s) (`--lib`, `--bin <n>`, `--test <n>`, `--example <n>`,
/// `--bench <n>`) in `crate_root` own a specific crate-relative `.rs` file so `tctx cargo-test
/// tests/ok.rs --no-run` compiles ONLY `tests/ok.rs` instead of the whole package!
pub fn resolve_cargo_target_args_for_file(
    crate_root: &Path,
    rel_file: &str,
    extra_args: &[String],
) -> Vec<Vec<String>> {
    let clean = rel_file.trim().trim_start_matches("./");
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut out: Vec<Vec<String>> = Vec::new();
    let owns = |target_root: &str| -> bool {
        crate_root.join(target_root).is_file()
            && collect_rust_target_tree_files(crate_root, target_root, false)
                .iter()
                .any(|f| f.trim_start_matches("./") == clean)
    };
    if let Some(ref lp) = manifest.lib_path
        && owns(lp)
    {
        out.push(vec!["--lib".to_string()]);
    }
    let mut seen_bins: BTreeSet<String> = BTreeSet::new();
    let mut check_bin = |p: &str, bn: &str| {
        if seen_bins.insert(bn.to_string()) && owns(p) {
            out.push(vec!["--bin".to_string(), bn.to_string()]);
        }
    };
    for (p, names) in &manifest.custom_bins {
        for bn in names {
            check_bin(p, bn);
        }
    }
    if let Some(ref mb) = manifest.main_bin {
        check_bin("src/main.rs", mb);
    }
    if manifest.autobins {
        for (p, bn) in auto_discovered_rust_bin_roots(crate_root) {
            check_bin(&p, &bn);
        }
    }
    let mut seen_tests: BTreeSet<String> = BTreeSet::new();
    for (tn, tp) in collect_rust_integration_test_roots_and_names(crate_root) {
        if seen_tests.insert(tn.clone()) && owns(&tp) {
            out.push(vec!["--test".to_string(), tn]);
        }
    }
    // Check explicit & auto-discovered examples and benches (honoring `autoexamples`/`autobenches`
    // and manifest overrides via `resolve_rust_target_trees_for_selector`)!
    for sel_all in ["--examples", "--benches"] {
        for (targs, _, files) in
            resolve_rust_target_trees_for_selector(crate_root, sel_all, None, extra_args)
        {
            if files.iter().any(|f| f.trim_start_matches("./") == clean)
                && !out.contains(&targs)
            {
                out.push(targs);
            }
        }
    }
    out
}

pub fn collect_rust_package_all_target_files(crate_root: &Path) -> Vec<String> {
    collect_rust_package_all_target_files_for_args(crate_root, &[])
}

/// Collects test-bearing crate-relative `.rs` files owned by `[lib]`, preventing non-test library
/// sources from triggering reverse-dependency expansion into integration or binary tests.
pub fn collect_rust_lib_target_files(crate_root: &Path) -> Vec<String> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let Some(lib_root) = manifest.lib_path else {
        return Vec::new();
    };
    collect_rust_target_tree_files(crate_root, &lib_root, true)
}

/// Collects test-bearing files across all feature-eligible binary targets declared in `Cargo.toml`.
pub fn collect_rust_all_bin_target_paths_for_args(
    crate_root: &Path,
    extra_args: &[String],
) -> Vec<String> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for (p, names) in &manifest.custom_bins {
        let any_eligible = names.iter().any(|bn| {
            manifest
                .custom_bin_required_features
                .get(bn)
                .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
        });
        if any_eligible && crate_root.join(p).is_file() {
            roots.insert(p.clone());
        }
    }
    if let Some(ref main_bin) = manifest.main_bin
        && manifest
            .custom_bin_required_features
            .get(main_bin)
            .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
        && crate_root.join("src/main.rs").is_file()
    {
        roots.insert("src/main.rs".to_string());
    }
    if manifest.autobins {
        for (p, bn) in auto_discovered_rust_bin_roots(crate_root) {
            let eligible = manifest
                .custom_bin_required_features
                .get(&bn)
                .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args));
            if eligible {
                roots.insert(p);
            }
        }
    }
    let mut out: BTreeSet<String> = BTreeSet::new();
    for r in roots {
        let tree = collect_rust_target_tree_files(crate_root, &r, true);
        if tree.is_empty() {
            out.insert(r);
        } else {
            out.extend(tree);
        }
    }
    out.into_iter().collect()
}

pub fn collect_rust_all_bin_target_paths(crate_root: &Path) -> Vec<String> {
    collect_rust_all_bin_target_paths_for_args(crate_root, &[])
}

/// Resolves `--bin <name>` and expands its complete module tree (root + mounted child test files).
pub fn resolve_rust_named_bin_target_files(crate_root: &Path, name: &str) -> Option<Vec<String>> {
    let root_rel = resolve_rust_named_bin_target_path(crate_root, name)?;
    let tree = collect_rust_target_tree_files(crate_root, &root_rel, true);
    if tree.is_empty() {
        Some(vec![root_rel])
    } else {
        Some(tree)
    }
}

/// Resolves `--test <name>` and expands its complete module tree (root + mounted child test files).
pub fn resolve_rust_named_test_target_files(crate_root: &Path, name: &str) -> Option<Vec<String>> {
    let root_rel = resolve_rust_named_test_target_path(crate_root, name)?;
    let mut tree: BTreeSet<String> = collect_rust_target_tree_files(crate_root, &root_rel, true)
        .into_iter()
        .collect();
    // Always retain the integration test root itself so even macro-generated integration tests execute.
    tree.insert(root_rel);
    Some(tree.into_iter().collect())
}

/// Discovers Cargo workspace member directories relative to `root` (including virtual workspaces
/// and implicit in-workspace path-dependency members) using `cargo metadata --no-deps --format-version 1`,
/// honoring `workspace_default_members` when `default_only` is true, with a static `Cargo.toml`
/// fallback if `cargo metadata` is unavailable.
pub fn discover_cargo_workspace_member_dirs(root: &Path, default_only: bool) -> Vec<String> {
    let cargo_cmd = crate::catalog::detect_cargo_cmd(root);
    if let Some((prog, base_args)) = cargo_cmd.split_first() {
        let mut cmd = Command::new(prog);
        cmd.current_dir(root);
        cmd.args(base_args);
        cmd.args(["metadata", "--no-deps", "--format-version", "1"]);
        if let Ok(out) = run_command_with_timeout_tracked(
            &mut cmd,
            Duration::from_secs(10),
            8 * 1024 * 1024,
            None,
            None,
        ) && !out.timed_out
            && !out.cancelled
            && out.status.success()
            && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        {
            let key = if default_only {
                "workspace_default_members"
            } else {
                "workspace_members"
            };
            let selected_ids: BTreeSet<&str> = val
                .get(key)
                .or_else(|| val.get("workspace_members"))
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect()
                })
                .unwrap_or_default();
            if !selected_ids.is_empty()
                && let Some(pkgs) = val.get("packages").and_then(serde_json::Value::as_array)
            {
                let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
                let mut dirs: BTreeSet<String> = BTreeSet::new();
                for pkg in pkgs {
                    let Some(id) = pkg.get("id").and_then(serde_json::Value::as_str) else {
                        continue;
                    };
                    if !selected_ids.contains(id) {
                        continue;
                    }
                    let Some(mp) = pkg.get("manifest_path").and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    let Some(dir) = Path::new(mp).parent() else {
                        continue;
                    };
                    let canon_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
                    if let Ok(rel) = canon_dir.strip_prefix(&canon_root) {
                        let s = rel.to_string_lossy().replace('\\', "/");
                        dirs.insert(if s.is_empty() { ".".to_string() } else { s });
                    }
                }
                if !dirs.is_empty() {
                    return dirs.into_iter().collect();
                }
            }
        }
    }
    let Some(text) = read_normalized_cargo_toml(root) else {
        return Vec::new();
    };
    let mut in_workspace = false;
    let mut has_package = false;
    let mut members_raw: Vec<String> = Vec::new();
    let mut default_members_raw: Vec<String> = Vec::new();
    let mut exclude_raw: Vec<String> = Vec::new();
    let mut collecting: Option<&str> = None;

    let mut in_root = true;
    for raw in text.lines() {
        let line = strip_toml_comment_outside_quotes(raw).trim();
        if line.is_empty() {
            continue;
        }
        if collecting.is_none()
            && let Some(h) = normalize_toml_table_header(line)
        {
            in_root = false;
            in_workspace = h == "[workspace]";
            if h == "[package]" {
                has_package = true;
            }
            continue;
        }
        let target_vec = if let Some(col) = collecting {
            Some((col, line))
        } else if in_workspace && let Some((k, v)) = split_toml_kv_outside_quotes(line) {
            match k.trim() {
                "members" => Some(("members", v.trim())),
                "default-members" => Some(("default-members", v.trim())),
                "exclude" => Some(("exclude", v.trim())),
                _ => None,
            }
        } else if in_root && let Some((k, v)) = split_toml_kv_outside_quotes(line) {
            let segs = split_toml_dotted_key(k);
            if segs.first().map(String::as_str) == Some("package") {
                has_package = true;
                None
            } else if segs.len() == 2 && segs[0] == "workspace" {
                match segs[1].as_str() {
                    "members" => Some(("members", v.trim())),
                    "default-members" => Some(("default-members", v.trim())),
                    "exclude" => Some(("exclude", v.trim())),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some((kind, slice)) = target_vec {
            let dest = match kind {
                "members" => &mut members_raw,
                "default-members" => &mut default_members_raw,
                _ => &mut exclude_raw,
            };
            let mut rest = slice;
            while let Some(q1) = rest.find(['"', '\'']) {
                let q = rest.as_bytes()[q1] as char;
                let after = &rest[q1 + 1..];
                if let Some(q2) = after.find(q) {
                    let val = after[..q2].trim().trim_start_matches("./").replace('\\', "/");
                    if !val.is_empty() {
                        dest.push(val);
                    }
                    rest = &after[q2 + 1..];
                } else {
                    break;
                }
            }
            if slice.contains(']') {
                collecting = None;
            } else if slice.contains('[') {
                collecting = Some(kind);
            }
        }
    }

    if default_only && !default_members_raw.is_empty() {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for pat in &default_members_raw {
            let clean = pat.trim_end_matches('/');
            if clean == "." || clean.is_empty() {
                if has_package {
                    out.insert(".".to_string());
                }
            } else if root.join(clean).join("Cargo.toml").is_file() {
                out.insert(clean.to_string());
            }
        }
        if !out.is_empty() {
            return out.into_iter().collect();
        }
    }
    if default_only && has_package && default_members_raw.is_empty() {
        return vec![".".to_string()];
    }

    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for pat in &exclude_raw {
        excluded.insert(pat.trim_end_matches('/').to_string());
    }
    let mut out: BTreeSet<String> = BTreeSet::new();
    if has_package {
        out.insert(".".to_string());
    }
    for pat in members_raw {
        let clean = pat.trim_end_matches('/');
        if clean.contains('*') {
            if let Ok(glob) = globset::GlobBuilder::new(clean)
                .literal_separator(true)
                .build()
            {
                let matcher = glob.compile_matcher();
                // Scan top 3 directory levels for matching member Cargo.toml directories.
                let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
                while let Some((dir, depth)) = stack.pop() {
                    if depth > 3 {
                        continue;
                    }
                    let Ok(entries) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if !p.is_dir() {
                            continue;
                        }
                        if let Some(name) = p.file_name().and_then(|n| n.to_str())
                            && crate::exclusion_policy::is_excluded_dir_name(name)
                        {
                            continue;
                        }
                        if let Ok(rel) = p.strip_prefix(root) {
                            let rel_s = rel.to_string_lossy().replace('\\', "/");
                            if matcher.is_match(&rel_s) && p.join("Cargo.toml").is_file() {
                                if !excluded.contains(&rel_s) {
                                    out.insert(rel_s);
                                }
                                continue;
                            }
                            stack.push((p, depth + 1));
                        }
                    }
                }
            }
        } else if !excluded.contains(clean) && root.join(clean).join("Cargo.toml").is_file() {
            out.insert(if clean.is_empty() {
                ".".to_string()
            } else {
                clean.to_string()
            });
        }
    }
    out.into_iter().collect()
}

pub fn cargo_manifest_has_lib(crate_root: &Path) -> bool {
    parse_cargo_manifest_targets(crate_root).lib_path.is_some()
}

pub fn cargo_manifest_has_package(crate_root: &Path) -> bool {
    let Some(text) = read_normalized_cargo_toml(crate_root) else {
        return false;
    };
    let mut in_root = true;
    for l in text.lines() {
        let t = strip_toml_comment_outside_quotes(l).trim();
        if t.is_empty() {
            continue;
        }
        if let Some(h) = normalize_toml_table_header(t) {
            in_root = false;
            if h == "[package]" {
                return true;
            }
            continue;
        }
        if in_root
            && let Some((k, _)) = split_toml_kv_outside_quotes(t)
            && split_toml_dotted_key(k).first().map(String::as_str) == Some("package")
        {
            return true;
        }
    }
    false
}

pub fn cargo_manifest_has_workspace(crate_root: &Path) -> bool {
    is_cargo_workspace_root(crate_root)
}

pub(super) fn cargo_manifest_matches_explicit_target(cdir_abs: &Path, ct: &str) -> bool {
    let manifest = parse_cargo_manifest_targets(cdir_abs);
    if ct == "lib" || ct == "doc" {
        manifest.lib_path.is_some()
    } else if ct == "bins" {
        !manifest.custom_bins.is_empty()
            || (manifest.main_bin.is_some() && cdir_abs.join("src/main.rs").is_file())
            || (manifest.autobins
                && std::fs::read_dir(cdir_abs.join("src/bin"))
                    .ok()
                    .is_some_and(|rd| {
                        rd.flatten().any(|e| {
                            let p = e.path();
                            (p.is_file() && p.extension().is_some_and(|x| x == "rs"))
                                || (p.is_dir() && p.join("main.rs").is_file())
                        })
                    }))
    } else if let Some(b) = ct.strip_prefix("bin:") {
        manifest
            .custom_bins
            .values()
            .any(|ns| ns.iter().any(|n| n == b))
            || (manifest.main_bin.as_deref() == Some(b) && cdir_abs.join("src/main.rs").is_file())
            || (manifest.autobins
                && (cdir_abs.join(format!("src/bin/{b}.rs")).is_file()
                    || cdir_abs.join(format!("src/bin/{b}/main.rs")).is_file()))
    } else if let Some(t) = ct.strip_prefix("test:") {
        manifest
            .custom_tests
            .values()
            .any(|ns| ns.iter().any(|n| n == t))
            || (manifest.autotests
                && (cdir_abs.join(format!("tests/{t}.rs")).is_file()
                    || cdir_abs.join(format!("tests/{t}/main.rs")).is_file()))
    } else {
        false
    }
}

pub fn cargo_manifest_has_any_testable_target(crate_root: &Path) -> bool {
    let manifest = parse_cargo_manifest_targets(crate_root);
    if manifest.lib_path.is_some()
        || !manifest.custom_bins.is_empty()
        || !manifest.custom_tests.is_empty()
        || (manifest.main_bin.is_some() && crate_root.join("src/main.rs").is_file())
        || (manifest.autobins && !auto_discovered_rust_bin_roots(crate_root).is_empty())
    {
        return true;
    }
    if manifest.autotests
        && let Ok(entries) = std::fs::read_dir(crate_root.join("tests"))
    {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("rs")
            {
                return true;
            }
            if p.is_dir() && p.join("main.rs").is_file() {
                return true;
            }
        }
    }
    false
}

/// Resolves `--bin <name>` against `Cargo.toml` declared `[[bin]]` paths first, then `autobins`
/// `src/bin/<name>.rs` / `src/bin/<name>/main.rs`, then default `src/main.rs` (`main_bin`).
pub fn resolve_rust_named_bin_target_path(crate_root: &Path, name: &str) -> Option<String> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    if let Some((path, _)) = manifest
        .custom_bins
        .iter()
        .find(|(_, bin_names)| bin_names.iter().any(|bn| bn.as_str() == name))
        && crate_root.join(path).is_file()
    {
        return Some(path.clone());
    }
    if manifest.autobins {
        let cand_rs = format!("src/bin/{name}.rs");
        if crate_root.join(&cand_rs).is_file() {
            return Some(cand_rs);
        }
        let cand_dir = format!("src/bin/{name}/main.rs");
        if crate_root.join(&cand_dir).is_file() {
            return Some(cand_dir);
        }
    }
    if manifest.main_bin.as_deref() == Some(name) && crate_root.join("src/main.rs").is_file() {
        return Some("src/main.rs".to_string());
    }
    None
}

/// Resolves `--test <name>` against `Cargo.toml` declared `[[test]]` paths first, then `autotests`
/// `tests/<name>.rs` and `tests/<name>/main.rs`.
pub fn resolve_rust_named_test_target_path(crate_root: &Path, name: &str) -> Option<String> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    if let Some((path, _)) = manifest
        .custom_tests
        .iter()
        .find(|(_, test_names)| test_names.iter().any(|tn| tn.as_str() == name))
        && crate_root.join(path).is_file()
    {
        return Some(path.clone());
    }
    if name.ends_with(".rs") {
        return Some(name.to_string());
    }
    if manifest.autotests {
        let flat = format!("tests/{name}.rs");
        if crate_root.join(&flat).is_file() {
            return Some(flat);
        }
        let nested = format!("tests/{name}/main.rs");
        if crate_root.join(&nested).is_file() {
            return Some(nested);
        }
    }
    None
}

#[cfg(test)]
pub(super) fn parse_cargo_package_and_main_bin(workdir: &Path) -> (Option<String>, Option<String>) {
    let info = parse_cargo_manifest_targets(workdir);
    (info.pkg_name, info.main_bin)
}

pub(super) fn parse_cargo_test_executables_workspace_json(
    stdout: &[u8],
    workspace_root: &Path,
) -> BTreeMap<(String, RustArtifactKind), PathBuf> {
    let mut map = BTreeMap::new();
    let canon_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if val.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let is_test_profile = val
            .get("profile")
            .and_then(|p| p.get("test"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        if !is_test_profile {
            continue;
        }
        let Some(exe_str) = val.get("executable").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let exe_path = PathBuf::from(exe_str);
        if !exe_path.is_file() {
            continue;
        }
        let Some(manifest_str) = val.get("manifest_path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(manifest_dir) = Path::new(manifest_str).parent() else {
            continue;
        };
        let crate_dir = manifest_dir
            .strip_prefix(workspace_root)
            .ok()
            .or_else(|| {
                manifest_dir
                    .canonicalize()
                    .ok()
                    .and_then(|c| c.strip_prefix(&canon_root).ok().map(Path::to_path_buf))
                    .as_deref()
                    .map(|_| Path::new(""))
            });
        let resolved_crate_dir = if let Ok(rel) = manifest_dir.strip_prefix(workspace_root) {
            normalize_rel_str(rel)
        } else if let Ok(canon_dir) = manifest_dir.canonicalize()
            && let Ok(rel) = canon_dir.strip_prefix(&canon_root)
        {
            normalize_rel_str(rel)
        } else {
            let _ = crate_dir;
            continue;
        };
        let Some(target) = val.get("target") else {
            continue;
        };
        let name = target
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let kinds: Vec<&str> = target
            .get("kind")
            .and_then(serde_json::Value::as_array)
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        if kinds.contains(&"test") {
            map.insert(
                (
                    resolved_crate_dir,
                    RustArtifactKind::IntegrationTest(name.to_string()),
                ),
                exe_path,
            );
        } else if kinds.contains(&"bin") {
            map.insert(
                (resolved_crate_dir, RustArtifactKind::Bin(name.to_string())),
                exe_path,
            );
        } else if kinds
            .iter()
            .any(|k| matches!(*k, "lib" | "rlib" | "cdylib" | "dylib" | "proc-macro"))
        {
            map.insert((resolved_crate_dir, RustArtifactKind::Lib), exe_path);
        }
    }
    map
}


