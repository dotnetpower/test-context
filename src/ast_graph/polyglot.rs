use super::*;

pub(super) fn is_c_cpp_source_path(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|e| e.to_str()),
        Some("c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx")
    )
}

/// Collects files read as raw bytes by Rust (`include_str!`, `include_bytes!`, `std::fs::read*`)
/// and Python (`open(...)`, `Path(...).read_text()`), which must be hashed by raw bytes rather
/// than comment-stripped `semantic_sha256`.
pub(super) fn collect_raw_embedded_files(files: &BTreeMap<String, FileNode>) -> BTreeSet<String> {
    let mut embedded = BTreeSet::new();
    let insert_file_or_dir = |cand: &str, out: &mut BTreeSet<String>| {
        if files.contains_key(cand) {
            out.insert(cand.to_string());
        }
        let dir_pfx = format!("{cand}/");
        for (k, _) in files
            .range(dir_pfx.clone()..)
            .take_while(|(k, _)| k.starts_with(&dir_pfx))
        {
            out.insert(k.clone());
        }
    };
    for (path, node) in files {
        if path.ends_with(".rs") {
            let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
            let crate_dirs = owning_rust_crate_dirs(path, files);
            for sym in &node.parsed.defined_symbols {
                if let Some(rel_ref) = sym.strip_prefix("rust_raw_embed:") {
                    if let Some(norm) = normalize_lexical_rel(&parent.join(rel_ref))
                        && files.contains_key(&norm)
                    {
                        embedded.insert(norm);
                    }
                    if files.contains_key(rel_ref) {
                        embedded.insert(rel_ref.to_string());
                    }
                } else if let Some(crate_ref) = sym.strip_prefix("rust_raw_embed_crate:") {
                    for crate_dir in &crate_dirs {
                        let base = if crate_dir.is_empty() {
                            Path::new(crate_ref).to_path_buf()
                        } else {
                            Path::new(crate_dir).join(crate_ref)
                        };
                        if let Some(norm) = normalize_lexical_rel(&base)
                            && files.contains_key(&norm)
                        {
                            embedded.insert(norm);
                        }
                    }
                } else if let Some(fs_ref) = sym.strip_prefix("rust_raw_fs:") {
                    if let Some(norm_root) = normalize_lexical_rel(Path::new(fs_ref)) {
                        insert_file_or_dir(&norm_root, &mut embedded);
                    }
                    if !parent.as_os_str().is_empty()
                        && let Some(norm_parent) = normalize_lexical_rel(&parent.join(fs_ref))
                    {
                        insert_file_or_dir(&norm_parent, &mut embedded);
                    }
                    for crate_dir in &crate_dirs {
                        let base = if crate_dir.is_empty() {
                            Path::new(fs_ref).to_path_buf()
                        } else {
                            Path::new(crate_dir).join(fs_ref)
                        };
                        if let Some(norm_crate) = normalize_lexical_rel(&base) {
                            insert_file_or_dir(&norm_crate, &mut embedded);
                        }
                    }
                }
            }
        } else if path.ends_with(".py") {
            let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
            for sym in &node.parsed.defined_symbols {
                if let Some(fs_ref) = sym.strip_prefix("py_raw_fs:") {
                    if let Some(norm_root) = normalize_lexical_rel(Path::new(fs_ref)) {
                        insert_file_or_dir(&norm_root, &mut embedded);
                    }
                    if !parent.as_os_str().is_empty()
                        && let Some(norm_parent) = normalize_lexical_rel(&parent.join(fs_ref))
                    {
                        insert_file_or_dir(&norm_parent, &mut embedded);
                    }
                }
            }
        }
    }
    embedded
}

/// Links a resolved C/C++ header (`norm_rel`) AND any matching sibling or mirrored `src/`
/// implementation files (`.c`, `.cpp`, `.cc`, `.cxx`).
fn link_header_with_impls(
    norm_rel: &str,
    importer_path: &str,
    files: &BTreeMap<String, FileNode>,
    deps: &mut BTreeSet<String>,
) {
    if norm_rel == importer_path || !files.contains_key(norm_rel) {
        return;
    }
    deps.insert(norm_rel.to_string());
    for hdr_ext in [".hpp", ".hxx", ".hh", ".h"] {
        if let Some(stem) = norm_rel.strip_suffix(hdr_ext) {
            for impl_ext in [".cpp", ".cc", ".cxx", ".c"] {
                let impl_cand = format!("{stem}{impl_ext}");
                if impl_cand != importer_path && files.contains_key(&impl_cand) {
                    deps.insert(impl_cand);
                }
            }
            // Also link mirrored `include/<rel>.h` -> `src/<rel>.c` / `<prefix>/include/<rel>.h` -> `<prefix>/src/<rel>.c`
            for (inc_seg, src_seg) in [("/include/", "/src/"), ("/inc/", "/src/")] {
                if stem.contains(inc_seg) {
                    let mirrored = stem.replacen(inc_seg, src_seg, 1);
                    for impl_ext in [".cpp", ".cc", ".cxx", ".c"] {
                        let impl_cand = format!("{mirrored}{impl_ext}");
                        if impl_cand != importer_path && files.contains_key(&impl_cand) {
                            deps.insert(impl_cand);
                        }
                    }
                }
            }
            for inc_pfx in ["include/", "inc/"] {
                if let Some(rest) = stem.strip_prefix(inc_pfx) {
                    for impl_ext in [".cpp", ".cc", ".cxx", ".c"] {
                        let impl_cand = format!("src/{rest}{impl_ext}");
                        if impl_cand != importer_path && files.contains_key(&impl_cand) {
                            deps.insert(impl_cand);
                        }
                    }
                }
            }
        }
    }
}

/// Resolves direct dependencies for C/C++ (`#include` across source-relative, workspace-root, and
/// ancestor `include/`/`inc/`/`src/` directories + header-to-implementation linking),
/// C# (same-namespace files, `using Namespace;`, `using static`, `using Alias = ...`),
/// Protobuf, and GraphQL files.
pub(super) fn resolve_polyglot_dependencies(
    path: &str,
    node: &FileNode,
    files: &BTreeMap<String, FileNode>,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));

    // C# files in the same namespace can reference sibling non-test types without `using`,
    // and project-wide `global using` directives apply to every `.cs` file in the same `.csproj`
    // project directory (or enclosing directory if no `.csproj` is present).
    let mut cs_extra_imports: Vec<(String, bool)> = Vec::new();
    if path.ends_with(".cs") {
        let own_namespaces: Vec<&str> = node
            .parsed
            .defined_symbols
            .iter()
            .filter_map(|s| s.strip_prefix("cs_ns:"))
            .collect();
        // Find nearest ancestor directory containing a `.csproj` file (falls back to `""`).
        let mut cs_proj_dir = String::new();
        let mut cur: Option<&Path> = Some(parent);
        while let Some(d) = cur {
            let d_str = normalize_rel_str(d);
            let d_prefix = if d_str.is_empty() {
                String::new()
            } else {
                format!("{d_str}/")
            };
            let has_csproj = files.keys().any(|k| {
                k.ends_with(".csproj")
                    && if d_prefix.is_empty() {
                        !k.contains('/')
                    } else {
                        k.strip_prefix(&d_prefix).is_some_and(|r| !r.contains('/'))
                    }
            });
            if has_csproj {
                cs_proj_dir = d_str;
                break;
            }
            if d.as_os_str().is_empty() {
                break;
            }
            cur = d.parent();
        }
        let proj_prefix = if cs_proj_dir.is_empty() {
            String::new()
        } else {
            format!("{cs_proj_dir}/")
        };
        for (cand_path, cand_node) in files {
            if cand_path == path || !cand_path.ends_with(".cs") {
                continue;
            }
            if !own_namespaces.is_empty()
                && !cand_node.is_test
                && own_namespaces.iter().any(|ns| {
                    let tag = format!("cs_ns:{ns}");
                    cand_node.parsed.defined_symbols.iter().any(|s| s == &tag)
                })
            {
                deps.insert(cand_path.clone());
            }
            let in_same_proj = proj_prefix.is_empty() || cand_path.starts_with(&proj_prefix);
            if in_same_proj {
                let mut contributed_global = false;
                for sym in &cand_node.parsed.defined_symbols {
                    if let Some(target) = sym.strip_prefix("cs_global_using:") {
                        cs_extra_imports.push((target.to_string(), false));
                        contributed_global = true;
                    } else if let Some(target) = sym
                        .strip_prefix("cs_global_using_static:")
                        .or_else(|| sym.strip_prefix("cs_global_using_alias:"))
                    {
                        cs_extra_imports.push((target.to_string(), true));
                        contributed_global = true;
                    }
                }
                if contributed_global {
                    deps.insert(cand_path.clone());
                }
            }
        }
    }

    // Collect ancestor `include/`/`inc/`/`src/` search roots for C/C++ and nonstandard headers.
    let mut c_search_dirs: Vec<std::path::PathBuf> = Vec::new();
    if is_c_cpp_source_path(path) || !path.ends_with(".cs") {
        let mut cur: Option<&Path> = Some(parent);
        while let Some(d) = cur {
            for sub in ["include", "inc", "src"] {
                let candidate_dir = if d.as_os_str().is_empty() {
                    Path::new(sub).to_path_buf()
                } else {
                    d.join(sub)
                };
                if !c_search_dirs.contains(&candidate_dir) {
                    c_search_dirs.push(candidate_dir);
                }
            }
            if d.as_os_str().is_empty() {
                break;
            }
            cur = d.parent();
        }
    }

    for spec in &node.parsed.imports {
        let mut header_cands: Vec<std::path::PathBuf> = vec![parent.join(&spec.module)];
        header_cands.push(Path::new(&spec.module).to_path_buf());
        for sdir in &c_search_dirs {
            header_cands.push(sdir.join(&spec.module));
        }
        for cand in header_cands {
            if let Some(norm_rel) = normalize_lexical_rel(&cand) {
                link_header_with_impls(&norm_rel, path, files, &mut deps);
            }
        }
        // C# `using Acme.Billing;`, `using static Acme.Billing.Pricing;`, `using P = Acme.Billing.Pricing;`
        if path.ends_with(".cs") {
            let is_type_import = spec
                .symbols
                .iter()
                .any(|s| s == "__cs_static__" || s == "__cs_alias__");
            cs_extra_imports.push((spec.module.clone(), is_type_import));
        }
    }
    if path.ends_with(".cs") {
        for (target_mod, is_type_import) in cs_extra_imports {
            let ns_tag = format!("cs_ns:{target_mod}");
            let (opt_ns_tag, last_seg) = match target_mod.rsplit_once('.') {
                Some((prefix, sym)) => (Some(format!("cs_ns:{prefix}")), sym),
                None => (None, target_mod.as_str()),
            };
            for (cand_path, cand_node) in files {
                if cand_path == path || !cand_path.ends_with(".cs") {
                    continue;
                }
                // 1. Standard namespace import (`using Acme.Billing;`): match files declaring `namespace Acme.Billing`
                if cand_node.parsed.defined_symbols.iter().any(|s| s == &ns_tag) {
                    deps.insert(cand_path.clone());
                    continue;
                }
                // 2. Type import (`using static Acme.Billing.Pricing;` / `using P = Acme.Billing.Pricing;`)
                if is_type_import
                    && !last_seg.is_empty()
                    && cand_node
                        .parsed
                        .defined_symbols
                        .iter()
                        .any(|s| s == last_seg)
                    && opt_ns_tag.as_ref().is_none_or(|prefix_ns| {
                        cand_node
                            .parsed
                            .defined_symbols
                            .iter()
                            .any(|s| s == prefix_ns)
                    })
                {
                    deps.insert(cand_path.clone());
                }
            }
        }
    }
    deps
}
