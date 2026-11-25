use super::*;

/// Returns true when a Python file inspects call-stack line numbers or raw source lines
/// (e.g. `inspect.currentframe()`, `inspect.stack()`, `inspect.getsource()`, `sys._getframe()`),
/// so blank/comment line shifts must invalidate the test cache.
pub(super) fn python_source_has_location_inspection(source: &str) -> bool {
    source.contains("currentframe")
        || source.contains("_getframe")
        || source.contains("f_lineno")
        || source.contains("getsourcelines")
        || source.contains("getsource(")
        || source.contains("extract_stack")
        || source.contains("inspect.stack")
        || source.contains("co_firstlineno")
}


#[derive(Default)]
pub(super) struct ModuleIndex {
    pub(super) module_to_files: BTreeMap<String, BTreeSet<String>>,
    pub(super) reexports: BTreeMap<(String, String), BTreeSet<String>>,
}

impl ModuleIndex {
    pub(super) fn build(files: &BTreeMap<String, FileNode>) -> Self {
        let mut module_to_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for path in files.keys() {
            if !path.ends_with(".py") {
                continue;
            }
            for dotted in candidate_module_names(path) {
                module_to_files
                    .entry(dotted)
                    .or_default()
                    .insert(path.clone());
            }
        }
        let mut index = Self {
            module_to_files,
            reexports: BTreeMap::new(),
        };
        // Two passes to resolve nested package `__init__.py` re-exports
        let init_files: Vec<(&String, &FileNode)> = files
            .iter()
            .filter(|(path, _)| *path == "__init__.py" || path.ends_with("/__init__.py"))
            .collect();
        for _ in 0..2 {
            let mut next_reexports = index.reexports.clone();
            for &(path, node) in &init_files {
                for spec in &node.parsed.imports {
                    if spec.symbols.is_empty() {
                        continue;
                    }
                    let targets = index.resolve_raw_import(path, spec, files);
                    for sym in &spec.symbols {
                        if sym == "*" {
                            continue;
                        }
                        let mut actual_targets = BTreeSet::new();
                        for t in &targets {
                            if (t == "__init__.py" || t.ends_with("/__init__.py"))
                                && let Some(forwarded) =
                                    index.reexports.get(&(t.clone(), sym.clone()))
                            {
                                actual_targets.extend(forwarded.iter().cloned());
                            } else {
                                actual_targets.insert(t.clone());
                            }
                        }
                        if !actual_targets.is_empty() {
                            next_reexports
                                .entry((path.clone(), sym.clone()))
                                .or_default()
                                .extend(actual_targets);
                        }
                    }
                }
            }
            index.reexports = next_reexports;
        }
        index
    }

    pub(super) fn resolve_import(
        &self,
        importer_path: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let raw_targets = self.resolve_raw_import(importer_path, spec, files);
        if spec.symbols.is_empty() || spec.symbols.iter().any(|s| s == "*") {
            return raw_targets;
        }

        let mut resolved = BTreeSet::new();
        for target in &raw_targets {
            if target == "__init__.py" || target.ends_with("/__init__.py") {
                let has_side_effects = files.get(target).is_some_and(|n| {
                    n.parsed
                        .defined_symbols
                        .iter()
                        .any(|s| s == "py_has_top_level_code")
                });
                let mut all_symbols_accounted = true;
                for sym in &spec.symbols {
                    if let Some(reexported_files) =
                        self.reexports.get(&(target.clone(), sym.clone()))
                    {
                        resolved.extend(reexported_files.iter().cloned());
                    } else if files
                        .get(target)
                        .is_some_and(|n| n.parsed.defined_symbols.binary_search(sym).is_ok())
                    {
                        resolved.insert(target.clone());
                    } else {
                        let pkg_dir = target.strip_suffix("/__init__.py").unwrap_or("");
                        let sub_py = if pkg_dir.is_empty() {
                            format!("{sym}.py")
                        } else {
                            format!("{pkg_dir}/{sym}.py")
                        };
                        let sub_init = if pkg_dir.is_empty() {
                            format!("{sym}/__init__.py")
                        } else {
                            format!("{pkg_dir}/{sym}/__init__.py")
                        };
                        if !raw_targets.contains(&sub_py) && !raw_targets.contains(&sub_init) {
                            all_symbols_accounted = false;
                        }
                    }
                }
                // Retain `pkg/__init__.py` whenever it executes top-level code/side effects
                // OR defines/misses any imported symbol; pure re-export `__init__.py` barrels
                // tree-shake cleanly until top-level code is added.
                if has_side_effects || !all_symbols_accounted {
                    resolved.insert(target.clone());
                }
            } else {
                resolved.insert(target.clone());
            }
        }
        resolved
    }

    pub(super) fn resolve_raw_import(
        &self,
        importer_path: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        if spec.level > 0 {
            // Relative import from importer_path
            let mut base_dir = Path::new(importer_path)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf();
            for _ in 1..spec.level {
                base_dir = base_dir
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf();
            }
            let target_dir = if spec.module.is_empty() {
                base_dir
            } else {
                base_dir.join(spec.module.replace('.', "/"))
            };
            add_path_candidates(&target_dir, files, &mut resolved);
            for sym in &spec.symbols {
                if sym != "*" {
                    add_path_candidates(&target_dir.join(sym), files, &mut resolved);
                }
            }
        } else {
            // Absolute import
            if !spec.module.is_empty() {
                if let Some(matches) = self.module_to_files.get(&spec.module) {
                    resolved.extend(filter_by_package_locality(importer_path, matches));
                }
                // Same-directory fallback (e.g. `import helper` inside `tests/unit/test_foo.py`)
                if let Some(parent) = Path::new(importer_path).parent() {
                    let rel_target = parent.join(spec.module.replace('.', "/"));
                    add_path_candidates(&rel_target, files, &mut resolved);
                }
                for sym in &spec.symbols {
                    if sym != "*" {
                        let sub_mod = format!("{}.{}", spec.module, sym);
                        if let Some(matches) = self.module_to_files.get(&sub_mod) {
                            resolved.extend(filter_by_package_locality(importer_path, matches));
                        }
                        if let Some(parent) = Path::new(importer_path).parent() {
                            let rel_target = parent.join(spec.module.replace('.', "/")).join(sym);
                            add_path_candidates(&rel_target, files, &mut resolved);
                        }
                    }
                }
            }
        }
        // Python executes EVERY ancestor package's `__init__.py` along the resolved import path
        // (e.g. `import pkg.sub.mod` executes `pkg/__init__.py` and `pkg/sub/__init__.py` first).
        // Add every ancestor `__init__.py` that contains executable top-level code (preserving
        // pure re-export barrel tree-shaking such as PyO3/maturin `hybrid_pkg/__init__.py`,
        // while immediately linking any `__init__.py` with side effects or newly added code!).
        let mut ancestor_inits = Vec::new();
        let init_has_code = |cand: &str| -> bool {
            files.get(cand).is_some_and(|n| {
                n.parsed
                    .defined_symbols
                    .iter()
                    .any(|s| s == "py_has_top_level_code")
            })
        };
        for res_path in &resolved {
            let mut cur = Path::new(res_path).parent();
            while let Some(dir) = cur {
                if dir.as_os_str().is_empty() {
                    if init_has_code("__init__.py") {
                        ancestor_inits.push("__init__.py".to_string());
                    }
                    break;
                }
                let init_cand = format!("{}/__init__.py", normalize_rel_str(dir));
                if init_has_code(&init_cand) {
                    ancestor_inits.push(init_cand);
                }
                cur = dir.parent();
            }
        }
        resolved.extend(ancestor_inits);
        resolved
    }
}

pub(super) fn service_compartment(rel_path: &str) -> Option<&str> {
    let mut parts = rel_path.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    if matches!(first, "services" | "extensions" | "benchmarks" | "apps")
        || (first == "src" && second.contains('-'))
    {
        let len = first.len() + 1 + second.len();
        Some(&rel_path[..len])
    } else {
        None
    }
}

pub(super) fn filter_by_package_locality(
    importer_path: &str,
    candidates: &BTreeSet<String>,
) -> BTreeSet<String> {
    if candidates.len() <= 1 {
        return candidates.clone();
    }
    // If `PYTHONPATH` explicitly includes external/cross-compartment directories, preserve
    // candidates whose compartment appears in `PYTHONPATH` so cross-app imports are not dropped.
    let pythonpath_entries: Vec<String> = std::env::var_os("PYTHONPATH")
        .map(|pp| {
            std::env::split_paths(&pp)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect()
        })
        .unwrap_or_default();
    let in_pythonpath = |cand: &str| -> bool {
        if pythonpath_entries.is_empty() {
            return false;
        }
        let Some(comp) = service_compartment(cand) else {
            return false;
        };
        pythonpath_entries.iter().any(|pp| {
            let clean_pp = pp.trim_start_matches("./").trim_end_matches('/');
            clean_pp == comp
                || clean_pp.starts_with(&format!("{comp}/"))
                || clean_pp.ends_with(&format!("/{comp}"))
                || clean_pp.contains(&format!("/{comp}/"))
        })
    };
    if let Some(importer_comp) = service_compartment(importer_path) {
        let same_comp: BTreeSet<String> = candidates
            .iter()
            .filter(|c| service_compartment(c) == Some(importer_comp) || in_pythonpath(c))
            .cloned()
            .collect();
        if !same_comp.is_empty() {
            return same_comp;
        }
        let non_foreign_service: BTreeSet<String> = candidates
            .iter()
            .filter(|c| match service_compartment(c) {
                Some(comp) => comp == importer_comp || in_pythonpath(c),
                None => true,
            })
            .cloned()
            .collect();
        if !non_foreign_service.is_empty() {
            return non_foreign_service;
        }
    }
    candidates.clone()
}

pub(super) fn add_path_candidates(
    target_no_ext: &Path,
    files: &BTreeMap<String, FileNode>,
    resolved: &mut BTreeSet<String>,
) {
    let norm = normalize_rel_str(target_no_ext);
    let file_candidate = if norm.is_empty() {
        String::new()
    } else {
        format!("{norm}.py")
    };
    if !file_candidate.is_empty() && files.contains_key(&file_candidate) {
        resolved.insert(file_candidate);
    }
    let init_candidate = if norm.is_empty() {
        "__init__.py".to_string()
    } else {
        format!("{norm}/__init__.py")
    };
    if files.contains_key(&init_candidate) {
        resolved.insert(init_candidate);
    }
}

pub(super) fn candidate_module_names(rel_py_path: &str) -> Vec<String> {
    let stripped = if let Some(s) = rel_py_path.strip_suffix("/__init__.py") {
        s
    } else if rel_py_path == "__init__.py" {
        return Vec::new();
    } else if let Some(s) = rel_py_path.strip_suffix(".py") {
        s
    } else {
        return Vec::new();
    };

    let parts: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }

    let mut names = BTreeSet::new();
    // 1. Full path from repo root (when all segments are valid Python identifiers)
    if parts.iter().all(|s| is_valid_dotted_module(s)) {
        names.insert(parts.join("."));
    }

    // 2. If there is a `src` or `python` segment (e.g. `src/pkg/mod.py`, `services/foo/src/pkg/mod.py`, `packages/bar/src/pkg/mod.py`),
    // register suffix after `src` or `python`.
    for (idx, &seg) in parts.iter().enumerate() {
        if (seg == "src" || seg == "python") && idx + 1 < parts.len() {
            let tail = &parts[idx + 1..];
            if tail.iter().all(|s| is_valid_dotted_module(s)) {
                names.insert(tail.join("."));
            }
        }
    }

    // 3. Support polyglot service roots like `src/ai-service/routers/description_generator.py` or `services/order-svc/app/main.py`
    // where a parent directory segment is a hyphenated service folder or a top-level service container (`src/<svc>/...`, `services/<svc>/...`).
    for (idx, &seg) in parts.iter().enumerate() {
        if !is_valid_dotted_module(seg) && idx + 1 < parts.len() {
            let tail = &parts[idx + 1..];
            if tail.iter().all(|s| is_valid_dotted_module(s)) {
                names.insert(tail.join("."));
            }
        }
    }
    if parts.len() >= 2
        && matches!(
            parts[0],
            "src" | "services" | "apps" | "packages" | "extensions"
        )
        && parts.get(2) != Some(&"src")
    {
        let tail = &parts[2..];
        if !tail.is_empty() && tail.iter().all(|s| is_valid_dotted_module(s)) {
            names.insert(tail.join("."));
        }
    }

    names.into_iter().collect()
}


