use super::*;

pub fn rust_crate_dir(rel_path: &str) -> String {
    if rel_path.starts_with("src/")
        || rel_path.starts_with("tests/")
        || rel_path.starts_with("benches/")
        || rel_path.starts_with("examples/")
        || rel_path == "build.rs"
        || rel_path == "Cargo.toml"
    {
        return String::new();
    }
    for marker in ["/src/", "/tests/", "/benches/", "/examples/"] {
        if let Some((prefix, _)) = rel_path.rsplit_once(marker) {
            return prefix.to_string();
        }
    }
    if let Some(prefix) = rel_path.strip_suffix("/build.rs") {
        return prefix.to_string();
    }
    if let Some(prefix) = rel_path.strip_suffix("/Cargo.toml") {
        return prefix.to_string();
    }
    String::new()
}

#[derive(Default)]
pub(super) struct RustModuleIndex {
    pub(super) crate_to_src_dirs: BTreeMap<String, BTreeSet<String>>,
    pub(super) pyo3_module_to_lib_rs: BTreeMap<String, BTreeSet<String>>,
    pub(super) reexports: BTreeMap<(String, String), BTreeSet<String>>,
}

impl RustModuleIndex {
    pub(super) fn build(files: &BTreeMap<String, FileNode>) -> Self {
        let mut crate_to_src_dirs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut pyo3_module_to_lib_rs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut crate_dirs = BTreeSet::new();

        for (path, node) in files {
            if path.ends_with(".rs") {
                let cdir = rust_crate_dir(path);
                crate_dirs.insert(cdir.clone());
                if !cdir.is_empty()
                    && let Some(dir_name) = Path::new(&cdir).file_name().and_then(|n| n.to_str())
                {
                    let crate_ident = dir_name.replace('-', "_");
                    crate_to_src_dirs
                        .entry(crate_ident)
                        .or_default()
                        .insert(format!("{cdir}/src"));
                }
                for sym in &node.parsed.defined_symbols {
                    if let Some(pyo3_name) = sym.strip_prefix("pyo3:") {
                        pyo3_module_to_lib_rs
                            .entry(pyo3_name.to_string())
                            .or_default()
                            .insert(path.clone());
                    }
                }
            } else if path == "Cargo.toml" || path.ends_with("/Cargo.toml") {
                let cdir = rust_crate_dir(path);
                let src_dir = if cdir.is_empty() {
                    "src".to_string()
                } else {
                    format!("{cdir}/src")
                };
                let lib_rs = format!("{src_dir}/lib.rs");
                let has_lib = files.contains_key(&lib_rs);
                let is_cdylib = node.parsed.module_markers.iter().any(|m| m == "cdylib");
                let has_explicit_lib_name = node
                    .parsed
                    .defined_symbols
                    .iter()
                    .any(|s| s.starts_with("pyo3:"));
                for sym in &node.parsed.defined_symbols {
                    if let Some(pyo3_name) = sym.strip_prefix("pyo3:") {
                        if has_lib {
                            pyo3_module_to_lib_rs
                                .entry(pyo3_name.to_string())
                                .or_default()
                                .insert(lib_rs.clone());
                        }
                    } else {
                        crate_to_src_dirs
                            .entry(sym.clone())
                            .or_default()
                            .insert(src_dir.clone());
                        if is_cdylib && has_lib && !has_explicit_lib_name {
                            pyo3_module_to_lib_rs
                                .entry(sym.clone())
                                .or_default()
                                .insert(lib_rs.clone());
                        }
                    }
                }
            }
        }

        // Also link pyproject.toml `[tool.maturin]` modules to matching crate `src/lib.rs`
        for (path, node) in files {
            if path == "pyproject.toml" || path.ends_with("/pyproject.toml") {
                let is_maturin = node.parsed.module_markers.iter().any(|m| m == "maturin");
                if !is_maturin
                    && !node
                        .parsed
                        .defined_symbols
                        .iter()
                        .any(|s| s.starts_with("pyo3:"))
                {
                    continue;
                }
                let py_dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                let mut candidate_libs = Vec::new();
                let direct_lib = if py_dir.is_empty() {
                    "src/lib.rs".to_string()
                } else {
                    format!("{py_dir}/src/lib.rs")
                };
                if files.contains_key(&direct_lib) {
                    candidate_libs.push(direct_lib);
                }
                if candidate_libs.is_empty() {
                    for k in files.keys() {
                        if k == "src/lib.rs" || k.ends_with("/src/lib.rs") {
                            candidate_libs.push(k.clone());
                        }
                    }
                }
                for sym in &node.parsed.defined_symbols {
                    if let Some(pyo3_name) = sym.strip_prefix("pyo3:") {
                        for lib in &candidate_libs {
                            pyo3_module_to_lib_rs
                                .entry(pyo3_name.to_string())
                                .or_default()
                                .insert(lib.clone());
                        }
                    } else if is_maturin
                        && !node
                            .parsed
                            .defined_symbols
                            .iter()
                            .any(|s| s.starts_with("pyo3:"))
                    {
                        for lib in &candidate_libs {
                            pyo3_module_to_lib_rs
                                .entry(sym.clone())
                                .or_default()
                                .insert(lib.clone());
                        }
                    }
                }
            }
        }

        let mut index = Self {
            crate_to_src_dirs,
            pyo3_module_to_lib_rs,
            reexports: BTreeMap::new(),
        };

        // Resolve `pub use` re-exports in Rust barrel modules (`lib.rs`, `mod.rs`)
        let barrel_files: Vec<(&String, &FileNode)> = files
            .iter()
            .filter(|(path, _)| {
                path.ends_with("/mod.rs")
                    || *path == "mod.rs"
                    || path.ends_with("/lib.rs")
                    || *path == "src/lib.rs"
            })
            .collect();
        for _ in 0..2 {
            let mut next_reexports = index.reexports.clone();
            for &(path, node) in &barrel_files {
                let cdir = rust_crate_dir(path);
                let src_dir = if cdir.is_empty() {
                    "src".to_string()
                } else {
                    format!("{cdir}/src")
                };
                for sym_def in &node.parsed.defined_symbols {
                    if let Some(rest) = sym_def.strip_prefix("rs_reexport:")
                        && let Some((exported_sym, target_part)) = rest.split_once("->")
                        && let Some((lvl_str, mod_and_orig)) = target_part.split_once(':')
                        && let Some((mod_str, orig_sym)) = mod_and_orig.rsplit_once(':')
                    {
                        let level = lvl_str.parse::<usize>().unwrap_or(0);
                        let spec = ImportSpec {
                            level,
                            module: mod_str.to_string(),
                            symbols: vec![orig_sym.to_string()],
                        };
                        let targets = index.resolve_rust_use_spec_raw(
                            path, node, &src_dir, &spec, files,
                        );
                        for t in targets {
                            if t != *path {
                                next_reexports
                                    .entry((path.clone(), exported_sym.to_string()))
                                    .or_default()
                                    .insert(t);
                            }
                        }
                    }
                }
            }
            index.reexports = next_reexports;
        }

        index
    }

    pub(super) fn resolve_pyo3_import(
        &self,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        if !spec.module.is_empty()
            && let Some(libs) = self.pyo3_module_to_lib_rs.get(&spec.module)
        {
            for lib in libs {
                if files.contains_key(lib) {
                    resolved.insert(lib.clone());
                }
            }
        }
        for sym in &spec.symbols {
            if let Some(libs) = self.pyo3_module_to_lib_rs.get(sym) {
                for lib in libs {
                    if files.contains_key(lib) {
                        resolved.insert(lib.clone());
                    }
                }
            }
            if !spec.module.is_empty() {
                let full = format!("{}.{}", spec.module, sym);
                if let Some(libs) = self.pyo3_module_to_lib_rs.get(&full) {
                    for lib in libs {
                        if files.contains_key(lib) {
                            resolved.insert(lib.clone());
                        }
                    }
                }
            }
        }
        resolved
    }

    pub(super) fn resolve_file_dependencies(
        &self,
        path: &str,
        node: &FileNode,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut deps = BTreeSet::new();
        let owning_crates = owning_rust_crate_dirs(path, files);
        let crate_dir = owning_crates
            .first()
            .cloned()
            .unwrap_or_else(|| rust_crate_dir(path));
        let src_dir = if crate_dir.is_empty() {
            "src".to_string()
        } else {
            format!("{crate_dir}/src")
        };
        let link_crate_build_script = |cdir: &str, deps: &mut BTreeSet<String>| {
            let cargo_toml = if cdir.is_empty() {
                "Cargo.toml".to_string()
            } else {
                format!("{cdir}/Cargo.toml")
            };
            let custom_build = files.get(&cargo_toml).and_then(|n| {
                n.parsed
                    .defined_symbols
                    .iter()
                    .find_map(|s| s.strip_prefix("cargo_build_script:"))
            });
            match custom_build {
                Some("false") => {}
                Some(rel_build) => {
                    let full_rel = if cdir.is_empty() {
                        Path::new(rel_build).to_path_buf()
                    } else {
                        Path::new(cdir).join(rel_build)
                    };
                    if let Some(norm) = normalize_lexical_rel(&full_rel)
                        && files.contains_key(&norm)
                        && path != norm
                    {
                        deps.insert(norm);
                    }
                }
                None => {
                    let build_rs = if cdir.is_empty() {
                        "build.rs".to_string()
                    } else {
                        format!("{cdir}/build.rs")
                    };
                    if files.contains_key(&build_rs) && path != build_rs {
                        deps.insert(build_rs);
                    }
                }
            }
        };
        for cdir in &owning_crates {
            link_crate_build_script(cdir, &mut deps);
        }
        if owning_crates.is_empty() {
            link_crate_build_script(&crate_dir, &mut deps);
        }

        // 1. Resolve `mod <name>;` declarations.
        // For crate/module roots (`lib.rs`, `mod.rs`) outside `tests/` that are not PyO3 entrypoints
        // and do not contain their own inline tests, `pub mod <child>;` declarations mount child modules
        // into the module tree rather than making symbols defined directly in `lib.rs`/`mod.rs`
        // (such as shared `Result`/`Error` types) depend on every sibling module.
        let is_test_tree_mod = path.starts_with("tests/") || path.contains("/tests/");
        let is_crate_or_mod_root = !is_test_tree_mod
            && (path.ends_with("/lib.rs")
                || path == "lib.rs"
                || path.ends_with("/mod.rs")
                || path == "mod.rs");
        let is_pymodule_root = node
            .parsed
            .defined_symbols
            .iter()
            .any(|s| s.starts_with("pyo3:"));
        let skip_root_mod_fanout = is_crate_or_mod_root && !is_pymodule_root && !node.is_test;
        if !skip_root_mod_fanout {
            for decl in &node.parsed.rust_mods {
                if decl.is_inline {
                    continue;
                }
                for child in resolve_rust_mod_decl(path, decl, files) {
                    if child != path {
                        deps.insert(child);
                    }
                }
            }
        }

        // 2. Resolve `use ...;` statements
        for spec in &node.parsed.imports {
            for target in self.resolve_rust_use_spec(path, node, &src_dir, spec, files) {
                if target != path {
                    deps.insert(target);
                }
            }
        }

        // 3. If an integration test (`tests/*.rs`) did not resolve any specific file under `src/`,
        // fall back to linking it to the crate's `src/lib.rs` and its mounted modules if present.
        if is_rust_integration_test_path(path) {
            let src_prefix = format!("{src_dir}/");
            let has_src_dep = deps.iter().any(|d| d.starts_with(&src_prefix));
            let lib_rs = format!("{src_dir}/lib.rs");
            if !has_src_dep
                && let Some(lib_node) = files.get(&lib_rs)
            {
                deps.insert(lib_rs.clone());
                for decl in &lib_node.parsed.rust_mods {
                    if !decl.is_inline {
                        for child in resolve_rust_mod_decl(&lib_rs, decl, files) {
                            deps.insert(child);
                        }
                    }
                }
            }
        }

        deps
    }

    pub(super) fn resolve_rust_use_spec(
        &self,
        importer_path: &str,
        importer_node: &FileNode,
        src_dir: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let raw = self.resolve_rust_use_spec_raw(importer_path, importer_node, src_dir, spec, files);
        if spec.symbols.is_empty() || spec.symbols.iter().any(|s| s == "*") {
            return raw;
        }
        let mut resolved = BTreeSet::new();
        for target in &raw {
            if target.ends_with("/mod.rs") || target == "mod.rs" || target.ends_with("/lib.rs") {
                let mut accounted = true;
                for sym in &spec.symbols {
                    if let Some(reexp) = self.reexports.get(&(target.clone(), sym.clone())) {
                        resolved.extend(reexp.iter().cloned());
                    } else {
                        accounted = false;
                    }
                }
                if !accounted {
                    resolved.insert(target.clone());
                }
            } else {
                resolved.insert(target.clone());
            }
        }
        resolved
    }

    pub(super) fn resolve_rust_use_spec_raw(
        &self,
        importer_path: &str,
        importer_node: &FileNode,
        src_dir: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        let parent_dir = Path::new(importer_path)
            .parent()
            .map(normalize_rel_str)
            .unwrap_or_default();

        if spec.level > 0 {
            let has_inline_mod = importer_node.parsed.rust_mods.iter().any(|m| m.is_inline);
            if spec.module == "*" && spec.symbols.is_empty() && has_inline_mod && spec.level == 1 {
                return resolved;
            }
            let mut base = Path::new(importer_path)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf();
            let file_name = Path::new(importer_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let extra_climb = if matches!(file_name, "mod.rs" | "lib.rs" | "main.rs") {
                spec.level
            } else {
                spec.level.saturating_sub(1)
            };
            for _ in 0..extra_climb {
                base = base
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf();
            }
            let base_str = normalize_rel_str(&base);
            if spec.module == "*" {
                for root_cand in ["mod.rs", "lib.rs"] {
                    let cand = if base_str.is_empty() {
                        root_cand.to_string()
                    } else {
                        format!("{base_str}/{root_cand}")
                    };
                    if files.contains_key(&cand) {
                        resolved.insert(cand);
                    }
                }
            } else {
                let parts: Vec<&str> = spec.module.split('.').filter(|s| !s.is_empty()).collect();
                add_rust_module_candidates(&base_str, &parts, &spec.symbols, files, &mut resolved);
            }
            return resolved;
        }

        let parts: Vec<&str> = spec.module.split('.').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return resolved;
        }

        if parts[0] == "crate" {
            let tail = &parts[1..];
            add_rust_module_candidates(src_dir, tail, &spec.symbols, files, &mut resolved);
            if resolved.is_empty() {
                let lib_rs = format!("{src_dir}/lib.rs");
                if files.contains_key(&lib_rs) {
                    resolved.insert(lib_rs);
                }
            }
            return resolved;
        }

        if parts[0] == "self" {
            let tail = &parts[1..];
            let file_stem = Path::new(importer_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let self_dir = if matches!(file_stem, "mod" | "lib" | "main") {
                parent_dir.clone()
            } else if parent_dir.is_empty() {
                file_stem.to_string()
            } else {
                format!("{parent_dir}/{file_stem}")
            };
            add_rust_module_candidates(&self_dir, tail, &spec.symbols, files, &mut resolved);
            add_rust_module_candidates(&parent_dir, tail, &spec.symbols, files, &mut resolved);
            return resolved;
        }

        // 1. Check relative to importer's parent directory (e.g. `use common::setup` in `tests/test_api.rs`)
        add_rust_module_candidates(&parent_dir, &parts, &spec.symbols, files, &mut resolved);

        // 2. Check registered crate names (`use my_crate::module::Item`)
        if let Some(target_src_dirs) = self.crate_to_src_dirs.get(parts[0]) {
            let tail = &parts[1..];
            for target_src in target_src_dirs {
                let before = resolved.len();
                add_rust_module_candidates(target_src, tail, &spec.symbols, files, &mut resolved);
                if resolved.len() == before {
                    let lib_rs = format!("{target_src}/lib.rs");
                    if files.contains_key(&lib_rs) {
                        resolved.insert(lib_rs);
                    }
                }
            }
        }

        // 3. Single-crate fallback when `Cargo.toml` was not loaded in memory:
        // `use any_crate_ident::submod::Item` where `src/submod.rs` exists in the current crate
        if resolved.is_empty() && parts.len() >= 2 {
            add_rust_module_candidates(src_dir, &parts[1..], &spec.symbols, files, &mut resolved);
        } else if resolved.is_empty() && parts.len() == 1 && !spec.symbols.is_empty() {
            add_rust_module_candidates(src_dir, &[], &spec.symbols, files, &mut resolved);
        }

        resolved
    }
}


/// Returns files included via Rust `#[path = "..."] mod foo;` declarations so they are hashed
/// by exact raw bytes (preserving their `#[cfg(test)]` helpers/tests when compiled into an
/// integration test binary).
pub(super) fn collect_rust_custom_path_mod_files(files: &BTreeMap<String, FileNode>) -> BTreeSet<String> {
    let mut raw_files = BTreeSet::new();
    for (path, node) in files {
        if !path.ends_with(".rs") {
            continue;
        }
        for decl in &node.parsed.rust_mods {
            if decl.custom_path.is_some() {
                for target in resolve_rust_mod_decl(path, decl, files) {
                    raw_files.insert(target);
                }
            }
        }
    }
    raw_files
}


pub(super) fn resolve_rust_mod_decl(
    importer_path: &str,
    decl: &RustModDecl,
    files: &BTreeMap<String, FileNode>,
) -> Vec<String> {
    let mut found = Vec::new();
    let parent = Path::new(importer_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    if let Some(custom) = &decl.custom_path {
        if let Some(norm) = normalize_lexical_rel(&parent.join(custom))
            && files.contains_key(&norm)
        {
            found.push(norm);
        }
        // In non-`lib.rs`/`main.rs`/`mod.rs` files (e.g. `src/parent.rs`), inline modules like
        // `mod inner { mod child; }` also look under `src/parent/inner/child.rs` per rustc rules.
        let stem = Path::new(importer_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if !matches!(stem, "lib" | "main" | "mod") && !is_rust_integration_test_path(importer_path) {
            let sub_base = parent.join(stem);
            if let Some(norm) = normalize_lexical_rel(&sub_base.join(custom))
                && files.contains_key(&norm)
                && !found.contains(&norm)
            {
                found.push(norm);
            }
        }
        return found;
    }
    let parent_str = normalize_rel_str(parent);
    let stem = Path::new(importer_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut base_dirs = Vec::new();
    if matches!(stem, "lib" | "main" | "mod") || is_rust_integration_test_path(importer_path) {
        base_dirs.push(parent_str);
    } else {
        let sub = if parent_str.is_empty() {
            stem.to_string()
        } else {
            format!("{parent_str}/{stem}")
        };
        base_dirs.push(sub);
        base_dirs.push(parent_str);
    }
    for base in base_dirs {
        for cand in [
            if base.is_empty() {
                format!("{}.rs", decl.name)
            } else {
                format!("{base}/{}.rs", decl.name)
            },
            if base.is_empty() {
                format!("{}/mod.rs", decl.name)
            } else {
                format!("{base}/{}/mod.rs", decl.name)
            },
        ] {
            if files.contains_key(&cand) && !found.contains(&cand) {
                found.push(cand);
            }
        }
    }
    found
}

pub(super) fn add_rust_module_candidates(
    base_dir: &str,
    parts: &[&str],
    symbols: &[String],
    files: &BTreeMap<String, FileNode>,
    resolved: &mut BTreeSet<String>,
) {
    let joined = parts.join("/");
    let prefix = match (base_dir.is_empty(), joined.is_empty()) {
        (true, true) => String::new(),
        (true, false) => joined,
        (false, true) => base_dir.to_string(),
        (false, false) => format!("{base_dir}/{joined}"),
    };
    if !prefix.is_empty() {
        let rs_cand = format!("{prefix}.rs");
        if files.contains_key(&rs_cand) {
            resolved.insert(rs_cand);
        }
        let mod_cand = format!("{prefix}/mod.rs");
        if files.contains_key(&mod_cand) {
            resolved.insert(mod_cand);
        }
    }
    for sym in symbols {
        if sym == "*" {
            continue;
        }
        let sym_prefix = if prefix.is_empty() {
            sym.clone()
        } else {
            format!("{prefix}/{sym}")
        };
        let rs_cand = format!("{sym_prefix}.rs");
        if files.contains_key(&rs_cand) {
            resolved.insert(rs_cand);
        }
        let mod_cand = format!("{sym_prefix}/mod.rs");
        if files.contains_key(&mod_cand) {
            resolved.insert(mod_cand);
        }
    }
}


