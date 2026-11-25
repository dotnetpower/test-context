use super::*;

impl FrontendModuleIndex {
    pub(super) fn resolve_file_dependencies(
        &self,
        path: &str,
        node: &FileNode,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut deps = BTreeSet::new();
        for spec in &node.parsed.imports {
            for resolved in self.resolve_import(path, spec, files) {
                if resolved != path {
                    deps.insert(resolved);
                }
            }
        }
        // Next.js App Router (`app/**/page.tsx`, `app/**/route.ts`) automatic ancestor `layout.tsx` / `template.tsx` / `loading.tsx` linking
        let file_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if (path.starts_with("app/") || path.contains("/app/"))
            && (file_name.starts_with("page.") || file_name.starts_with("route."))
        {
            let mut cur = Path::new(path).parent();
            while let Some(dir) = cur {
                let dir_str = normalize_rel_str(dir);
                for conv in ["layout", "template", "loading"] {
                    for ext in [".tsx", ".ts", ".jsx", ".js"] {
                        let cand = if dir_str.is_empty() {
                            format!("{conv}{ext}")
                        } else {
                            format!("{dir_str}/{conv}{ext}")
                        };
                        if cand != path && files.contains_key(&cand) {
                            deps.insert(cand);
                        }
                    }
                }
                if dir_str == "app" || dir_str.ends_with("/app") || dir_str.is_empty() {
                    break;
                }
                cur = dir.parent();
            }
        }
        if node.is_test {
            let parent = Path::new(path)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            let file_name = Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let snap_in_dir = if parent.is_empty() {
                format!("__snapshots__/{file_name}.snap")
            } else {
                format!("{parent}/__snapshots__/{file_name}.snap")
            };
            if files.contains_key(&snap_in_dir) {
                deps.insert(snap_in_dir);
            }
            let adjacent_snap = format!("{path}.snap");
            if files.contains_key(&adjacent_snap) {
                deps.insert(adjacent_snap);
            }
        }
        deps
    }

    pub(super) fn collect_raw_imported_files(&self, files: &BTreeMap<String, FileNode>) -> BTreeSet<String> {
        let mut raw_files = BTreeSet::new();
        for (path, node) in files {
            let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
            let mut pkg_dirs: Option<Vec<String>> = None;
            for sym in &node.parsed.defined_symbols {
                if let Some(raw_spec) = sym.strip_prefix("fe_raw_import:") {
                    for resolved in self.resolve_raw_specifier(path, raw_spec, files) {
                        raw_files.insert(resolved);
                    }
                } else if let Some(url_fs_ref) = sym.strip_prefix("fe_raw_url_fs:") {
                    // Source-relative decoded URL literal (`new URL('../fixtures/a%23b.json', import.meta.url)`)
                    // resolves strictly beside `path`'s parent directory without stripping `?`/`#`.
                    if let Some(norm_parent) = normalize_lexical_rel(&parent.join(url_fs_ref))
                        && files.contains_key(&norm_parent)
                    {
                        raw_files.insert(norm_parent);
                    }
                } else if let Some(fs_ref) = sym.strip_prefix("fe_raw_fs:") {
                    let dirs = pkg_dirs.get_or_insert_with(|| {
                        let mut out = Vec::new();
                        let mut cur = Path::new(path).parent();
                        while let Some(d) = cur {
                            let ds = normalize_rel_str(d);
                            let pkg_json = if ds.is_empty() {
                                "package.json".to_string()
                            } else {
                                format!("{ds}/package.json")
                            };
                            if files.contains_key(&pkg_json) {
                                out.push(ds);
                            }
                            cur = d.parent();
                        }
                        out
                    });
                    let mut insert_candidate = |cand: &str| {
                        if files.contains_key(cand) {
                            raw_files.insert(cand.to_string());
                        }
                        let dir_pfx = format!("{cand}/");
                        for (k, _) in files
                            .range(dir_pfx.clone()..)
                            .take_while(|(k, _)| k.starts_with(&dir_pfx))
                        {
                            raw_files.insert(k.clone());
                        }
                    };
                    if let Some(norm_root) = normalize_lexical_rel(Path::new(fs_ref)) {
                        insert_candidate(&norm_root);
                    }
                    if !parent.as_os_str().is_empty()
                        && let Some(norm_parent) = normalize_lexical_rel(&parent.join(fs_ref))
                    {
                        insert_candidate(&norm_parent);
                    }
                    for dir in dirs.iter() {
                        if !dir.is_empty()
                            && let Some(norm_pkg) =
                                normalize_lexical_rel(&Path::new(dir).join(fs_ref))
                        {
                            insert_candidate(&norm_pkg);
                        }
                    }
                }
            }
        }
        raw_files
    }

    pub(super) fn resolve_import(
        &self,
        importer_path: &str,
        spec: &ImportSpec,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let raw_targets = self.resolve_raw_specifier(importer_path, &spec.module, files);
        if spec.symbols.is_empty() || spec.symbols.iter().any(|s| s == "*") {
            let mut expanded = raw_targets.clone();
            for t in &raw_targets {
                if let Some(wild) = self.wildcard_reexports.get(t) {
                    expanded.extend(wild.iter().cloned());
                }
            }
            return expanded;
        }

        let mut resolved = BTreeSet::new();
        for target in &raw_targets {
            if is_frontend_barrel_file(target) {
                if self.side_effectful_barrels.contains(target) {
                    // Barrel executes top-level side effects directly or via a re-exported module;
                    // retain it directly as a dependency.
                    resolved.insert(target.clone());
                }
                let mut all_accounted = true;
                for sym in &spec.symbols {
                    if let Some(reexported) = self.reexports.get(&(target.clone(), sym.clone())) {
                        resolved.extend(reexported.iter().cloned());
                    } else if files
                        .get(target)
                        .is_some_and(|n| n.parsed.defined_symbols.binary_search(sym).is_ok())
                    {
                        resolved.insert(target.clone());
                    } else {
                        let barrel_dir = Path::new(target)
                            .parent()
                            .map(normalize_rel_str)
                            .unwrap_or_default();
                        let sym_base = if barrel_dir.is_empty() {
                            sym.clone()
                        } else {
                            format!("{barrel_dir}/{sym}")
                        };
                        let mut sub_matches = BTreeSet::new();
                        add_frontend_path_candidates(&sym_base, files, &mut sub_matches);
                        if !sub_matches.is_empty() {
                            resolved.extend(sub_matches);
                        } else {
                            all_accounted = false;
                        }
                    }
                }
                if !all_accounted {
                    resolved.insert(target.clone());
                    if let Some(wild) = self.wildcard_reexports.get(target) {
                        for w in wild {
                            if files.get(w).is_some_and(|n| {
                                spec.symbols
                                    .iter()
                                    .any(|s| n.parsed.defined_symbols.contains(s))
                            }) {
                                resolved.insert(w.clone());
                            }
                        }
                    }
                }
            } else {
                resolved.insert(target.clone());
            }
        }
        resolved
    }

    /// Returns true if an unindexed (e.g. gitignored) `tsconfig.json`/`jsconfig.json` exists on
    /// disk closer to `importer_path` than any indexed governing `tsconfig.json`/`jsconfig.json`.
    pub(super) fn has_unindexed_nearest_tsconfig(
        &self,
        importer_path: &str,
        files: &BTreeMap<String, FileNode>,
        workspace_root: &Path,
    ) -> bool {
        let mut cur = Path::new(importer_path).parent();
        while let Some(d) = cur {
            let ds = normalize_rel_str(d);
            let mut found_indexed = false;
            for name in ["tsconfig.json", "jsconfig.json"] {
                let rel = if ds.is_empty() {
                    name.to_string()
                } else {
                    format!("{ds}/{name}")
                };
                if files.contains_key(&rel) {
                    found_indexed = true;
                } else if workspace_root.join(&rel).is_file() {
                    return true;
                }
            }
            if found_indexed {
                return false;
            }
            cur = d.parent();
        }
        false
    }

    /// Cacheability check for Node `#imports`: requires EVERY potentially active conditional target
    /// of the winning `PATTERN_KEY_COMPARE` rule in the nearest `package.json` to resolve to an
    /// indexed non-declaration runtime file (so an ignored `"node"` condition target can NEVER be
    /// masked by an indexed `"default"` condition target!).
    pub(super) fn resolve_subpath_for_cache_check(
        &self,
        importer_path: &str,
        raw_module: &str,
        files: &BTreeMap<String, FileNode>,
        workspace_root: Option<&Path>,
    ) -> bool {
        let Some(clean_mod) = clean_frontend_import_specifier(raw_module) else {
            return false;
        };
        let mut nearest_pkg_dir: Option<String> = None;
        let mut cur = Path::new(importer_path).parent();
        while let Some(d) = cur {
            let ds = normalize_rel_str(d);
            let pj = if ds.is_empty() {
                "package.json".to_string()
            } else {
                format!("{ds}/package.json")
            };
            if files.contains_key(&pj) {
                nearest_pkg_dir = Some(ds);
                break;
            }
            cur = d.parent();
        }
        if nearest_pkg_dir.is_none() && files.contains_key("package.json") {
            nearest_pkg_dir = Some(String::new());
        }
        let Some(lookup_dir) = nearest_pkg_dir else {
            return false;
        };
        let Some(rules) = self.subpath_imports_by_dir.get(&lookup_dir) else {
            return false;
        };
        let mut best_sub_rank: Option<(u8, usize, usize)> = None;
        let mut winning_rels: Vec<String> = Vec::new();
        for (sub_pat, target_pat) in rules {
            let matched: Option<(u8, usize, usize, String)> =
                if let Some((pat_pfx, pat_sfx)) = sub_pat.split_once('*') {
                    clean_mod
                        .strip_prefix(pat_pfx)
                        .and_then(|s| s.strip_suffix(pat_sfx))
                        .map(|mid| {
                            (1, pat_pfx.len(), sub_pat.len(), target_pat.replace('*', mid))
                        })
                } else if clean_mod == *sub_pat {
                    Some((2, sub_pat.len(), sub_pat.len(), target_pat.clone()))
                } else {
                    None
                };
            if let Some((tier, pfx_len, pat_len, rel)) = matched {
                let rank = (tier, pfx_len, pat_len);
                match best_sub_rank {
                    None => {
                        best_sub_rank = Some(rank);
                        winning_rels.push(rel);
                    }
                    Some(cur_r) if rank > cur_r => {
                        best_sub_rank = Some(rank);
                        winning_rels.clear();
                        winning_rels.push(rel);
                    }
                    Some(cur_r) if rank == cur_r => {
                        winning_rels.push(rel);
                    }
                    Some(_) => {}
                }
            }
        }
        if winning_rels.is_empty() {
            return false;
        }
        winning_rels.iter().all(|rel| {
            let is_explicit_rel =
                rel.starts_with("./") || rel.starts_with("../") || rel.starts_with('/');
            let is_bare_pkg = !is_explicit_rel
                && self
                    .pkg_to_dir
                    .keys()
                    .any(|pkg| *rel == *pkg || rel.starts_with(&format!("{pkg}/")));
            if is_bare_pkg {
                return self
                    .resolve_workspace_pkg_for_cache_check(importer_path, rel, files, workspace_root)
                    .unwrap_or(false);
            }
            let mapped = if lookup_dir.is_empty() {
                rel.clone()
            } else {
                format!("{lookup_dir}/{rel}")
            };
            let mut tmp = BTreeSet::new();
            if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                add_frontend_path_candidates(&norm, files, &mut tmp);
            }
            if !tmp
                .iter()
                .any(|t| !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts"))
            {
                return false;
            }
            if let Some(ws_root) = workspace_root
                && self.remapped_js_masked_by_unindexed_disk_file(
                    importer_path,
                    rel,
                    &tmp,
                    files,
                    ws_root,
                )
            {
                return false;
            }
            true
        })
    }

    /// Cacheability check for bare workspace-package imports (`@scope/ui` or `@scope/ui/Button`):
    /// returns `Some(true)` only when ALL potentially active conditional `exports` targets of the
    /// winning export rule resolve to indexed non-declaration runtime files (and never lets a
    /// fallback `src/index.ts` mask an unindexed `exports` target!).
    pub(super) fn resolve_workspace_pkg_for_cache_check(
        &self,
        importer_path: &str,
        raw_module: &str,
        files: &BTreeMap<String, FileNode>,
        workspace_root: Option<&Path>,
    ) -> Option<bool> {
        let clean_mod = clean_frontend_import_specifier(raw_module)?;
        let resolves_runtime = |mapped: &str, raw_target: &str| -> bool {
            let mut tmp = BTreeSet::new();
            if let Some(norm) = normalize_lexical_rel(Path::new(mapped)) {
                add_frontend_path_candidates(&norm, files, &mut tmp);
            }
            if !tmp
                .iter()
                .any(|t| !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts"))
            {
                return false;
            }
            if let Some(ws_root) = workspace_root
                && self.remapped_js_masked_by_unindexed_disk_file(
                    importer_path,
                    raw_target,
                    &tmp,
                    files,
                    ws_root,
                )
            {
                return false;
            }
            true
        };
        for (pkg_name, target_pkg_dir) in &self.pkg_to_dir {
            if &clean_mod == pkg_name {
                if let Some(exp_rules) = self.pkg_exports.get(pkg_name) {
                    let matching_targets: Vec<&str> = exp_rules
                        .iter()
                        .filter(|(sub_k, _)| sub_k == "." || sub_k.is_empty())
                        .map(|(_, tv)| tv.as_str())
                        .collect();
                    if !matching_targets.is_empty() {
                        return Some(matching_targets.iter().all(|tv| {
                            let mapped = if target_pkg_dir.is_empty() {
                                (*tv).to_string()
                            } else {
                                format!("{target_pkg_dir}/{tv}")
                            };
                            resolves_runtime(&mapped, tv)
                        }));
                    }
                    // `exports` exists on this package without a `"."` entry -> fail closed!
                    return Some(false);
                }
                if let Some(main_entries) = self.pkg_mains.get(pkg_name)
                    && !main_entries.is_empty()
                {
                    return Some(main_entries.iter().all(|tv| {
                        let mapped = if target_pkg_dir.is_empty() {
                            tv.clone()
                        } else {
                            format!("{target_pkg_dir}/{tv}")
                        };
                        resolves_runtime(&mapped, tv)
                    }));
                }
                let entry_prefixes = [
                    if target_pkg_dir.is_empty() {
                        "src".to_string()
                    } else {
                        format!("{target_pkg_dir}/src")
                    },
                    target_pkg_dir.clone(),
                ];
                let mut tmp = BTreeSet::new();
                for entry_prefix in &entry_prefixes {
                    add_frontend_path_candidates(entry_prefix, files, &mut tmp);
                }
                if !tmp
                    .iter()
                    .any(|t| !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts"))
                {
                    return Some(false);
                }
                if let Some(ws_root) = workspace_root {
                    // Check for unindexed runtime index files (e.g. gitignored `<pkg>/index.js`
                    // shadowing tracked `<pkg>/src/index.ts`!).
                    const FALLBACK_RUNTIME_EXTS: &[&str] = &[
                        ".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json",
                        ".node",
                    ];
                    for entry_prefix in &entry_prefixes {
                        for ext in FALLBACK_RUNTIME_EXTS {
                            let idx_rel = if entry_prefix.is_empty() {
                                format!("index{ext}")
                            } else {
                                format!("{entry_prefix}/index{ext}")
                            };
                            if !files.contains_key(&idx_rel) && ws_root.join(&idx_rel).is_file() {
                                return Some(false);
                            }
                        }
                    }
                    if self.remapped_js_masked_by_unindexed_disk_file(
                        importer_path,
                        "",
                        &tmp,
                        files,
                        ws_root,
                    ) {
                        return Some(false);
                    }
                }
                return Some(true);
            } else if let Some(subpath) = clean_mod
                .strip_prefix(pkg_name.as_str())
                .and_then(|s| s.strip_prefix('/'))
            {
                if let Some(exp_rules) = self.pkg_exports.get(pkg_name) {
                    let mut best_rank: Option<(u8, usize, usize)> = None;
                    let mut winning_targets: Vec<String> = Vec::new();
                    for (sub_k, target_v) in exp_rules {
                        if let Some(rel_target) =
                            match_package_export_subpath(subpath, sub_k, target_v)
                        {
                            let rank = if let Some((pfx, _)) = sub_k.split_once('*') {
                                (1, pfx.len(), sub_k.len())
                            } else {
                                (2, sub_k.len(), sub_k.len())
                            };
                            match best_rank {
                                None => {
                                    best_rank = Some(rank);
                                    winning_targets.push(rel_target);
                                }
                                Some(cur_r) if rank > cur_r => {
                                    best_rank = Some(rank);
                                    winning_targets.clear();
                                    winning_targets.push(rel_target);
                                }
                                Some(cur_r) if rank == cur_r => {
                                    winning_targets.push(rel_target);
                                }
                                Some(_) => {}
                            }
                        }
                    }
                    if !winning_targets.is_empty() {
                        return Some(winning_targets.iter().all(|rt| {
                            let mapped = if target_pkg_dir.is_empty() {
                                rt.clone()
                            } else {
                                format!("{target_pkg_dir}/{rt}")
                            };
                            resolves_runtime(&mapped, rt)
                        }));
                    }
                    return Some(false);
                }
                let subpath_bases = [
                    if target_pkg_dir.is_empty() {
                        format!("src/{subpath}")
                    } else {
                        format!("{target_pkg_dir}/src/{subpath}")
                    },
                    if target_pkg_dir.is_empty() {
                        subpath.to_string()
                    } else {
                        format!("{target_pkg_dir}/{subpath}")
                    },
                ];
                let mut tmp = BTreeSet::new();
                for base in &subpath_bases {
                    add_frontend_path_candidates(base, files, &mut tmp);
                }
                if !tmp
                    .iter()
                    .any(|t| !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts"))
                {
                    return Some(false);
                }
                if let Some(ws_root) = workspace_root {
                    if self.remapped_js_masked_by_unindexed_disk_file(
                        importer_path,
                        subpath,
                        &tmp,
                        files,
                        ws_root,
                    ) {
                        return Some(false);
                    }
                    // Also check unindexed direct/extensioned files at `<pkg>/<subpath>` even if
                    // indexed `<pkg>/src/<subpath>` matched!
                    const SUBPATH_RUNTIME_EXTS: &[&str] = &[
                        "", ".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json",
                        ".node",
                    ];
                    for base in &subpath_bases {
                        if let Some(norm) = normalize_lexical_rel(Path::new(base)) {
                            for ext in SUBPATH_RUNTIME_EXTS {
                                let cand = format!("{norm}{ext}");
                                if !files.contains_key(&cand) && ws_root.join(&cand).is_file() {
                                    return Some(false);
                                }
                                if !ext.is_empty() {
                                    let idx_cand = format!("{norm}/index{ext}");
                                    if !files.contains_key(&idx_cand)
                                        && ws_root.join(&idx_cand).is_file()
                                    {
                                        return Some(false);
                                    }
                                }
                            }
                        }
                    }
                }
                return Some(true);
            }
        }
        None
    }

    /// Returns true if an unindexed (e.g. gitignored) `package.json` exists on disk closer to
    /// `importer_path` than any indexed `package.json`.
    pub(super) fn has_unindexed_nearest_package_json(
        &self,
        importer_path: &str,
        files: &BTreeMap<String, FileNode>,
        workspace_root: &Path,
    ) -> bool {
        let mut cur = Path::new(importer_path).parent();
        while let Some(d) = cur {
            let ds = normalize_rel_str(d);
            let pj = if ds.is_empty() {
                "package.json".to_string()
            } else {
                format!("{ds}/package.json")
            };
            if files.contains_key(&pj) {
                return false;
            }
            if workspace_root.join(&pj).is_file() {
                return true;
            }
            cur = d.parent();
        }
        false
    }

    /// Returns true if a bare specifier (e.g. `features/foo`) matches an unindexed/gitignored
    /// runtime file on disk under the governing tsconfig's `compilerOptions.baseUrl` (while
    /// leaving ordinary external npm packages like `react` untouched).
    pub(super) fn bare_import_masked_by_unindexed_baseurl_file(
        &self,
        importer_path: &str,
        raw_module: &str,
        files: &BTreeMap<String, FileNode>,
        workspace_root: &Path,
    ) -> bool {
        let clean_mod = raw_module
            .split(['?', '#'])
            .next()
            .unwrap_or(raw_module)
            .trim();
        if clean_mod.is_empty()
            || clean_mod.starts_with('.')
            || clean_mod.starts_with('/')
            || clean_mod.starts_with('#')
            || clean_mod.starts_with("node:")
        {
            return false;
        }
        let mut ancestor_cfg_dirs: Vec<String> = Vec::new();
        let mut anc_cur = Path::new(importer_path).parent();
        while let Some(d) = anc_cur {
            let ds = normalize_rel_str(d);
            if !ancestor_cfg_dirs.contains(&ds) {
                ancestor_cfg_dirs.push(ds);
            }
            anc_cur = d.parent();
        }
        if !ancestor_cfg_dirs.iter().any(String::is_empty) {
            ancestor_cfg_dirs.push(String::new());
        }
        let Some(gov_dir) = ancestor_cfg_dirs
            .iter()
            .find(|ds| self.ts_config_dirs.contains(ds.as_str()))
        else {
            return false;
        };
        let Some(bus) = self.base_url_by_dir.get(gov_dir) else {
            return false;
        };
        for (bu, bu_dir) in bus {
            let base = match (bu_dir.is_empty(), bu.is_empty() || bu == ".") {
                (true, true) => clean_mod.to_string(),
                (true, false) => format!("{bu}/{clean_mod}"),
                (false, true) => format!("{bu_dir}/{clean_mod}"),
                (false, false) => format!("{bu_dir}/{bu}/{clean_mod}"),
            };
            let Some(norm_base) = normalize_lexical_rel(Path::new(&base)) else {
                continue;
            };
            if files.contains_key(&norm_base) {
                continue;
            }
            if workspace_root.join(&norm_base).is_file() {
                return true;
            }
            for ext in FRONTEND_RESOLVE_EXTS {
                let cand = format!("{norm_base}{ext}");
                if !files.contains_key(&cand) && workspace_root.join(&cand).is_file() {
                    return true;
                }
            }
            for idx_file in FRONTEND_INDEX_FILES {
                let cand = format!("{norm_base}/{idx_file}");
                if !files.contains_key(&cand) && workspace_root.join(&cand).is_file() {
                    return true;
                }
            }
        }
        false
    }

    /// Returns true if a runtime import resolved to an indexed file while an unindexed (e.g.
    /// gitignored) sibling runtime file ACTUALLY EXISTS on disk at a higher-priority or matching
    /// loader extension:
    /// - For explicit `.js`/`.jsx`/`.mjs`/`.cjs` imports remapped to `.ts`/`.tsx`/`.mts`/`.cts`:
    ///   checks whether the exact requested runtime extension exists unindexed on disk.
    /// - For extensionless imports (`import './foo'`): checks ALL Vite/Node runtime loader
    ///   extensions (`.mjs`, `.js`, `.mts`, `.ts`, `.jsx`, `.tsx`, `.cjs`, `.cts`, `.json`) on
    ///   every resolved stem so an indexed `foo.jsx` can NEVER mask an ignored `foo.js`!
    pub(super) fn remapped_js_masked_by_unindexed_disk_file(
        &self,
        importer_path: &str,
        raw_module: &str,
        resolved_targets: &BTreeSet<String>,
        files: &BTreeMap<String, FileNode>,
        workspace_root: &Path,
    ) -> bool {
        let clean_mod = raw_module
            .split(['?', '#'])
            .next()
            .unwrap_or(raw_module)
            .trim();
        let req_ext = [
            ".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json", ".vue",
            ".svelte", ".css", ".scss", ".sass", ".less", ".styl", ".stylus", ".svg",
        ]
        .into_iter()
        .find(|ext| clean_mod.ends_with(ext));
        // If an indexed file already matched the exact requested extension, no shadowing/remapping occurred.
        if let Some(ext) = req_ext
            && resolved_targets.iter().any(|t| t.ends_with(ext))
        {
            return false;
        }
        const LOADER_RUNTIME_EXTS: &[&str] = &[
            ".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json", ".node",
        ];
        // Check whether ANY resolved target has an unindexed sibling runtime file on disk!
        for target in resolved_targets {
            if let Some(ext) = req_ext {
                if matches!(ext, ".js" | ".jsx" | ".mjs" | ".cjs") {
                    for ts_ext in [".tsx", ".ts", ".mts", ".cts"] {
                        if let Some(stem) = target.strip_suffix(ts_ext) {
                            let disk_rel = format!("{stem}{ext}");
                            if !files.contains_key(&disk_rel)
                                && workspace_root.join(&disk_rel).is_file()
                            {
                                return true;
                            }
                        }
                    }
                }
            } else {
                // Extensionless import (e.g. `import './foo'` or `#helper` -> `./src/helper`):
                // check every Vite/Node runtime loader extension on the resolved stem (both direct
                // file stem and `/index.<ext>` barrel stem)!
                let mut stems: Vec<String> = Vec::new();
                // Always probe the direct relative stem (e.g. `require('./lib')` -> `lib`) even
                // if `resolved_targets` only contains a manifest or redirected file!
                if clean_mod.starts_with("./") || clean_mod.starts_with("../") {
                    let parent = Path::new(importer_path)
                        .parent()
                        .unwrap_or_else(|| Path::new(""));
                    if let Some(rel_stem) = normalize_lexical_rel(&parent.join(clean_mod)) {
                        stems.push(rel_stem);
                    }
                }
                for cand_ext in LOADER_RUNTIME_EXTS {
                    if let Some(stem) = target.strip_suffix(cand_ext) {
                        if !stems.iter().any(|s| s == stem) {
                            stems.push(stem.to_string());
                        }
                        if let Some(dir_stem) = stem.strip_suffix("/index")
                            && !stems.iter().any(|s| s == dir_stem)
                        {
                            stems.push(dir_stem.to_string());
                        }
                        break;
                    }
                }
                for stem in &stems {
                    let stem = stem.as_str();
                    // Node tries exact extensionless file (`require('./config')` -> `config`)
                    // BEFORE appending `.js`/`.json`/`.node`, AND checks `<dir>/package.json`
                    // `main` BEFORE `<dir>/index.*`, so probe both on disk!
                    if !files.contains_key(stem) && workspace_root.join(stem).is_file() {
                        return true;
                    }
                    let dir_pkg_rel = format!("{stem}/package.json");
                    if let Some(pkg_node) = files.get(&dir_pkg_rel) {
                        for sym in &pkg_node.parsed.defined_symbols {
                            if let Some(entry_val) = sym.strip_prefix("fe_dir_main:")
                                && let Some(mapped) =
                                    normalize_lexical_rel(&Path::new(stem).join(entry_val))
                            {
                                for ext in ["", ".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json", ".node"] {
                                    let cand = format!("{mapped}{ext}");
                                    if !files.contains_key(&cand)
                                        && workspace_root.join(&cand).is_file()
                                    {
                                        return true;
                                    }
                                    if !ext.is_empty() {
                                        let idx_cand = format!("{mapped}/index{ext}");
                                        if !files.contains_key(&idx_cand)
                                            && workspace_root.join(&idx_cand).is_file()
                                        {
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    } else if workspace_root.join(&dir_pkg_rel).is_file() {
                        return true;
                    }
                    for loader_ext in LOADER_RUNTIME_EXTS {
                        let disk_rel = format!("{stem}{loader_ext}");
                        if !files.contains_key(&disk_rel)
                            && workspace_root.join(&disk_rel).is_file()
                        {
                            return true;
                        }
                        let idx_rel = format!("{stem}/index{loader_ext}");
                        if !files.contains_key(&idx_rel)
                            && workspace_root.join(&idx_rel).is_file()
                        {
                            return true;
                        }
                    }
                }
            }
        }
        // Also check direct relative path when `clean_mod` is relative with an explicit JS ext.
        if req_ext.is_some_and(|e| matches!(e, ".js" | ".jsx" | ".mjs" | ".cjs"))
            && (clean_mod.starts_with("./") || clean_mod.starts_with("../"))
        {
            let parent = Path::new(importer_path)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            if let Some(norm) = normalize_lexical_rel(&parent.join(clean_mod))
                && !files.contains_key(&norm)
                && workspace_root.join(&norm).is_file()
            {
                return true;
            }
        }
        false
    }

    /// Evaluates ONLY Vite aliases in exact declaration order for cacheability (invoked BEFORE
    /// `#imports` so a Vite alias for `#db` takes precedence over `package.json` `"#db"`!).
    pub(super) fn resolve_vite_alias_for_cache_check(
        &self,
        importer_path: &str,
        raw_module: &str,
        files: &BTreeMap<String, FileNode>,
        workspace_root: Option<&Path>,
    ) -> Option<bool> {
        // Use the shared specifier normalizer so leading `#` (e.g. `#db` Vite aliases) is
        // preserved instead of split to an empty string!
        let clean_mod = clean_frontend_import_specifier(raw_module)?;
        if clean_mod.starts_with("./") || clean_mod.starts_with("../") || clean_mod.starts_with('/') {
            return None;
        }
        let mut ancestor_cfg_dirs: Vec<String> = Vec::new();
        let mut anc_cur = Path::new(importer_path).parent();
        while let Some(d) = anc_cur {
            let ds = normalize_rel_str(d);
            if !ancestor_cfg_dirs.contains(&ds) {
                ancestor_cfg_dirs.push(ds);
            }
            anc_cur = d.parent();
        }
        if !ancestor_cfg_dirs.iter().any(String::is_empty) {
            ancestor_cfg_dirs.push(String::new());
        }
        for lookup_dir in &ancestor_cfg_dirs {
            if let Some(vite_aliases) = self.vite_aliases_by_dir.get(lookup_dir) {
                for (alias_pat, target_pat, declaring_dir) in vite_aliases {
                    let matched_target = if alias_pat.ends_with('/') {
                        clean_mod
                            .strip_prefix(alias_pat.as_str())
                            .map(|rest| format!("{target_pat}{rest}"))
                    } else if clean_mod == alias_pat.as_str() {
                        Some(target_pat.clone())
                    } else {
                        None
                    };
                    if let Some(rel_target) = matched_target {
                        let mapped = if declaring_dir.is_empty() {
                            rel_target
                        } else {
                            format!("{declaring_dir}/{rel_target}")
                        };
                        let mut tmp = BTreeSet::new();
                        if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                            add_frontend_path_candidates(&norm, files, &mut tmp);
                        }
                        if !tmp.iter().any(|t| {
                            !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts")
                        }) {
                            return Some(false);
                        }
                        if let Some(ws_root) = workspace_root
                            && self.remapped_js_masked_by_unindexed_disk_file(
                                importer_path,
                                &mapped,
                                &tmp,
                                files,
                                ws_root,
                            )
                        {
                            return Some(false);
                        }
                        return Some(true);
                    }
                }
            }
        }
        None
    }

    /// Checks whether a configured Vite or TypeScript alias matches `raw_module` and whether its
    /// winning target(s) resolve unambiguously to indexed non-declaration files:
    /// - Vite aliases are evaluated in EXACT declaration order (first match wins).
    /// - TypeScript `paths` aliases select the single nearest governing `tsconfig.json`/
    ///   `jsconfig.json` first (which already includes any `"extends"` chain!), then rank its
    ///   effective mappings by exact > longest prefix-only length, and require the FIRST target in
    ///   the ordered fallback array (`["@generated/*", "src/*"]`) to resolve cleanly!
    pub(super) fn resolve_alias_for_cache_check(
        &self,
        importer_path: &str,
        raw_module: &str,
        files: &BTreeMap<String, FileNode>,
        workspace_root: Option<&Path>,
    ) -> Option<bool> {
        let clean_mod = raw_module
            .split(['?', '#'])
            .next()
            .unwrap_or(raw_module)
            .trim();
        if clean_mod.starts_with("./") || clean_mod.starts_with("../") || clean_mod.starts_with('/') {
            return None;
        }
        let mut ancestor_cfg_dirs: Vec<String> = Vec::new();
        let mut anc_cur = Path::new(importer_path).parent();
        while let Some(d) = anc_cur {
            let ds = normalize_rel_str(d);
            if !ancestor_cfg_dirs.contains(&ds) {
                ancestor_cfg_dirs.push(ds);
            }
            anc_cur = d.parent();
        }
        if !ancestor_cfg_dirs.iter().any(String::is_empty) {
            ancestor_cfg_dirs.push(String::new());
        }
        let resolves_runtime_file = |mapped: &str| -> bool {
            let mut tmp = BTreeSet::new();
            if let Some(norm) = normalize_lexical_rel(Path::new(mapped)) {
                add_frontend_path_candidates(&norm, files, &mut tmp);
            }
            if !tmp
                .iter()
                .any(|t| !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts"))
            {
                return false;
            }
            if let Some(ws_root) = workspace_root
                && self.remapped_js_masked_by_unindexed_disk_file(
                    importer_path,
                    raw_module,
                    &tmp,
                    files,
                    ws_root,
                )
            {
                return false;
            }
            true
        };

        // 1. Vite aliases in EXACT declaration order (first match wins!).
        for lookup_dir in &ancestor_cfg_dirs {
            if let Some(vite_aliases) = self.vite_aliases_by_dir.get(lookup_dir) {
                for (alias_pat, target_pat, declaring_dir) in vite_aliases {
                    let matched_target = if alias_pat.ends_with('/') {
                        clean_mod
                            .strip_prefix(alias_pat.as_str())
                            .map(|rest| format!("{target_pat}{rest}"))
                    } else if clean_mod == alias_pat.as_str() {
                        Some(target_pat.clone())
                    } else {
                        None
                    };
                    if let Some(rel_target) = matched_target {
                        let mapped = if declaring_dir.is_empty() {
                            rel_target
                        } else {
                            format!("{declaring_dir}/{rel_target}")
                        };
                        return Some(resolves_runtime_file(&mapped));
                    }
                }
            }
        }

        // 2. Governing TypeScript `tsconfig.json`/`jsconfig.json` FIRST (nearest ancestor dir in
        //    `ts_config_dirs` ONLY, so a nested `vite.config.ts` never shadows a parent tsconfig!),
        //    then rank ONLY its effective `paths` by exact > longest PREFIX-ONLY wildcard length!
        let governing_ts_dir = ancestor_cfg_dirs
            .iter()
            .find(|ds| self.ts_config_dirs.contains(ds.as_str()));
        if let Some(lookup_dir) = governing_ts_dir
            && let Some(aliases) = self.aliases_by_dir.get(lookup_dir)
        {
            let mut best_alias_rank: Option<(u8, usize)> = None;
            let mut winning_alias_targets: Vec<String> = Vec::new();
            for (alias_pat, target_pat, declaring_dir) in aliases {
                let matched: Option<(u8, usize, String)> =
                    if let Some((pfx, sfx)) = alias_pat.split_once('*') {
                        if clean_mod.starts_with(pfx)
                            && clean_mod.ends_with(sfx)
                            && clean_mod.len() >= pfx.len() + sfx.len()
                        {
                            let mid = &clean_mod[pfx.len()..clean_mod.len() - sfx.len()];
                            let target = if let Some((tpfx, tsfx)) = target_pat.split_once('*') {
                                format!("{tpfx}{mid}{tsfx}")
                            } else {
                                target_pat.clone()
                            };
                            Some((1, pfx.len(), target))
                        } else {
                            None
                        }
                    } else if alias_pat.ends_with('/') {
                        clean_mod
                            .strip_prefix(alias_pat.as_str())
                            .map(|rest| (1, alias_pat.len(), format!("{target_pat}{rest}")))
                    } else if clean_mod == alias_pat.as_str() {
                        Some((2, alias_pat.len(), target_pat.clone()))
                    } else {
                        None
                    };
                if let Some((tier, spec_len, rel_target)) = matched {
                    let mapped = if declaring_dir.is_empty() {
                        rel_target
                    } else {
                        format!("{declaring_dir}/{rel_target}")
                    };
                    let rank = (tier, spec_len);
                    match best_alias_rank {
                        None => {
                            best_alias_rank = Some(rank);
                            winning_alias_targets.push(mapped);
                        }
                        Some(cur) if rank > cur => {
                            best_alias_rank = Some(rank);
                            winning_alias_targets.clear();
                            winning_alias_targets.push(mapped);
                        }
                        Some(cur) if rank == cur => {
                            winning_alias_targets.push(mapped);
                        }
                        Some(_) => {}
                    }
                }
            }
            if best_alias_rank.is_some() {
                return Some(
                    winning_alias_targets
                        .first()
                        .is_some_and(|first_target| resolves_runtime_file(first_target)),
                );
            }
        }
        None
    }

}

