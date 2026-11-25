use super::*;

pub(super) const FRONTEND_RESOLVE_EXTS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".svelte", ".json",
    ".css", ".scss", ".sass", ".less", ".styl", ".stylus", ".svg",
];

pub(super) const FRONTEND_INDEX_FILES: &[&str] = &[
    "index.tsx",
    "index.ts",
    "index.jsx",
    "index.js",
    "index.mjs",
    "index.mts",
    "index.cjs",
    "index.cts",
    "index.json",
];

#[derive(Default)]
pub(super) struct FrontendModuleIndex {
    pub(super) pkg_to_dir: BTreeMap<String, String>,
    /// Directories that contain a `tsconfig.json`/`jsconfig.json` (used to select the single
    /// governing TypeScript config for `paths` lookup so an unrelated parent `@lib/*` cannot
    /// outrank a child tsconfig's `@*` when the child does not extend the parent).
    pub(super) ts_config_dirs: BTreeSet<String>,
    /// Map config directory -> `(alias_prefix, target_prefix, declaring_dir)` for TypeScript
    /// `paths` aliases (with ordered fallback targets preserved in vector order).
    pub(super) aliases_by_dir: BTreeMap<String, Vec<(String, String, String)>>,
    /// Map config directory -> `(alias_prefix, target_prefix, declaring_dir)` for Vite aliases
    /// preserved in EXACT declaration order (first match wins in Vite).
    pub(super) vite_aliases_by_dir: BTreeMap<String, Vec<(String, String, String)>>,
    /// Map config directory -> `(base_url, declaring_dir)` for tsconfig `compilerOptions.baseUrl`
    /// bare-specifier resolution (including inherited `baseUrl`).
    pub(super) base_url_by_dir: BTreeMap<String, Vec<(String, String)>>,
    pub(super) subpath_imports_by_dir: BTreeMap<String, Vec<(String, String)>>,
    /// Configured Vite `root: '...'` directory candidate(s) indexed by config dir (may hold both
    /// package-dir-relative and workspace-root-relative candidates for nested cwd-relative `root`).
    pub(super) vite_root_by_dir: BTreeMap<String, Vec<String>>,
    /// Config directories with a nested relative Vite `root: './client'` whose actual runtime
    /// resolution depends on runner cwd (workspace root vs package dir).
    pub(super) ambiguous_vite_root_dirs: BTreeSet<String>,
    /// Config directories with cwd-relative Vite aliases (`path.resolve('./generated')` without
    /// `__dirname`) whose actual runtime target depends on runner cwd.
    pub(super) ambiguous_cwd_aliases_by_dir: BTreeMap<String, Vec<String>>,
    pub(super) pkg_exports: BTreeMap<String, Vec<(String, String)>>,
    pub(super) pkg_mains: BTreeMap<String, Vec<String>>,
    pub(super) reexports: BTreeMap<(String, String), BTreeSet<String>>,
    pub(super) wildcard_reexports: BTreeMap<String, BTreeSet<String>>,
    /// Barrels that execute top-level side effects directly OR re-export a module with top-level
    /// side effects; importers must retain these barrels as direct dependencies.
    pub(super) side_effectful_barrels: BTreeSet<String>,
}


impl FrontendModuleIndex {
    pub(super) fn build(files: &BTreeMap<String, FileNode>) -> Self {
        let mut pkg_to_dir = BTreeMap::new();
        let mut ts_config_dirs: BTreeSet<String> = BTreeSet::new();
        // Governing default TS/JS config file per directory (`tsconfig.json` preferred over
        // `jsconfig.json`; auxiliary configs like `tsconfig.node.json` or `tsconfig.build.json`
        // NEVER govern a directory by default or merge into `tsconfig.json`!).
        let mut governing_ts_cfg_by_dir: BTreeMap<String, String> = BTreeMap::new();
        let mut raw_aliases_by_cfg: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut has_paths_cfgs: BTreeSet<String> = BTreeSet::new();
        let mut own_base_url_by_cfg: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut raw_vite_aliases_by_dir: BTreeMap<String, Vec<(String, String, bool)>> =
            BTreeMap::new();
        let mut ambiguous_cwd_aliases_by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut subpath_imports_by_dir: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut vite_root_by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut ambiguous_vite_root_dirs: BTreeSet<String> = BTreeSet::new();
        let mut pkg_exports: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut pkg_mains: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut raw_extends: Vec<(String, String)> = Vec::new();

        for (path, node) in files {
            if !node.is_config && !path.ends_with(".json") && !path.ends_with(".jsonc") {
                continue;
            }
            let dir = Path::new(path)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            if let Some(fn_s) = Path::new(path).file_name().and_then(|n| n.to_str()) {
                if fn_s == "tsconfig.json" {
                    governing_ts_cfg_by_dir.insert(dir.clone(), path.clone());
                    ts_config_dirs.insert(dir.clone());
                } else if fn_s == "jsconfig.json" {
                    governing_ts_cfg_by_dir
                        .entry(dir.clone())
                        .or_insert_with(|| path.clone());
                    ts_config_dirs.insert(dir.clone());
                }
            }
            for sym in &node.parsed.defined_symbols {
                if let Some(pkg_name) = sym.strip_prefix("fe_pkg:") {
                    pkg_to_dir.insert(pkg_name.to_string(), dir.clone());
                } else if sym == "fe_has_paths" {
                    has_paths_cfgs.insert(path.clone());
                } else if let Some(alias_rule) = sym.strip_prefix("fe_alias:")
                    && let Some((alias_pfx, target_list)) = alias_rule.split_once("->")
                {
                    has_paths_cfgs.insert(path.clone());
                    let list = raw_aliases_by_cfg.entry(path.clone()).or_default();
                    for target_pfx in target_list.split('\t') {
                        if !target_pfx.is_empty() {
                            list.push((alias_pfx.to_string(), target_pfx.to_string()));
                        }
                    }
                } else if let Some(vite_rule) = sym.strip_prefix("fe_vite_alias:")
                    && let Some((alias_pfx, target_pfx)) = vite_rule.split_once("->")
                {
                    raw_vite_aliases_by_dir
                        .entry(dir.clone())
                        .or_default()
                        .push((alias_pfx.to_string(), target_pfx.to_string(), false));
                } else if let Some(vite_cwd_rule) = sym.strip_prefix("fe_vite_alias_cwd:")
                    && let Some((alias_pfx, target_pfx)) = vite_cwd_rule.split_once("->")
                {
                    raw_vite_aliases_by_dir
                        .entry(dir.clone())
                        .or_default()
                        .push((alias_pfx.to_string(), target_pfx.to_string(), true));
                } else if let Some(bu) = sym.strip_prefix("fe_baseurl:") {
                    own_base_url_by_cfg
                        .entry(path.clone())
                        .or_default()
                        .push((bu.to_string(), dir.clone()));
                } else if let Some(sub_rule) = sym.strip_prefix("fe_subpath:")
                    && let Some((sub_pfx, target_pfx)) = sub_rule.split_once("->")
                {
                    subpath_imports_by_dir
                        .entry(dir.clone())
                        .or_default()
                        .push((sub_pfx.to_string(), target_pfx.to_string()));
                } else if let Some(pkg_name) = sym.strip_prefix("fe_has_exports:") {
                    pkg_exports.entry(pkg_name.to_string()).or_default();
                } else if let Some(exp_rule) = sym.strip_prefix("fe_export:")
                    && let Some((pkg_and_sub, target_pfx)) = exp_rule.split_once("->")
                    && let Some((pkg_name, sub_key)) = pkg_and_sub.rsplit_once(':')
                {
                    pkg_exports
                        .entry(pkg_name.to_string())
                        .or_default()
                        .push((sub_key.to_string(), target_pfx.to_string()));
                } else if let Some(main_rule) = sym.strip_prefix("fe_pkg_main:")
                    && let Some((pkg_name, target_pfx)) = main_rule.split_once("->")
                {
                    pkg_mains
                        .entry(pkg_name.to_string())
                        .or_default()
                        .push(target_pfx.to_string());
                } else if let Some(vr) = sym.strip_prefix("fe_vite_root:") {
                    let resolved_vr = if dir.is_empty() {
                        vr.to_string()
                    } else {
                        format!("{dir}/{vr}")
                    };
                    if let Some(norm) = normalize_lexical_rel(Path::new(&resolved_vr)) {
                        let entry = vite_root_by_dir.entry(dir.clone()).or_default();
                        if !entry.contains(&norm) {
                            entry.push(norm);
                        }
                    }
                } else if let Some(vr_cwd) = sym.strip_prefix("fe_vite_root_cwd:") {
                    // Relative Vite `root: './app/client'` or `root: './client'` depends on runner
                    // cwd (workspace root OR package dir): conservatively track BOTH candidates
                    // for `affected_tests` AND mark the config dir ambiguous when the two roots
                    // differ so root-relative imports fail closed for caching!
                    let entry = vite_root_by_dir.entry(dir.clone()).or_default();
                    let nested_cand = if dir.is_empty() {
                        vr_cwd.to_string()
                    } else {
                        format!("{dir}/{vr_cwd}")
                    };
                    let norm_nested = normalize_lexical_rel(Path::new(&nested_cand));
                    let norm_ws = normalize_lexical_rel(Path::new(vr_cwd));
                    if let Some(ref nn) = norm_nested
                        && !entry.contains(nn)
                    {
                        entry.push(nn.clone());
                    }
                    if let Some(ref nw) = norm_ws
                        && !entry.contains(nw)
                    {
                        entry.push(nw.clone());
                    }
                    if !dir.is_empty() && norm_nested != norm_ws {
                        ambiguous_vite_root_dirs.insert(dir.clone());
                    }
                } else if let Some(ext_rel) = strip_fe_extends_symbol(sym) {
                    raw_extends.push((path.clone(), ext_rel.to_string()));
                }
            }
        }

        // Resolve `extends` edges keyed strictly by exact config FILE path so sibling
        // `tsconfig.build.json` never merges into `tsconfig.json`!
        let mut extends_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (cfg_path, ext_rel) in raw_extends {
            if let Some(target_cfg) =
                resolve_tsconfig_extends_target(&cfg_path, &ext_rel, files, &pkg_to_dir)
            {
                let list = extends_map.entry(cfg_path).or_default();
                if !list.contains(&target_cfg) {
                    list.push(target_cfg);
                }
            }
        }

        let mut effective_base_url_by_dir: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut aliases_by_dir: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
        let mut resolved_vite_aliases_by_dir: BTreeMap<String, Vec<(String, String, String)>> =
            BTreeMap::new();

        for (dir, gov_cfg_path) in &governing_ts_cfg_by_dir {
            // Walk exact config-file `extends` chain (child first, then bases in TS 5.0+ DFS
            // order where later entries in `"extends": [A, B]` take precedence) with cycle guard.
            let mut chain: Vec<String> = Vec::new();
            let mut stack: Vec<String> = vec![gov_cfg_path.clone()];
            let mut visited: BTreeSet<String> = BTreeSet::new();
            while let Some(curr) = stack.pop() {
                if !visited.insert(curr.clone()) {
                    continue;
                }
                chain.push(curr.clone());
                if let Some(bases) = extends_map.get(&curr) {
                    for b in bases {
                        if !visited.contains(b) {
                            stack.push(b.clone());
                        }
                    }
                }
            }

            // Effective `baseUrl` for `gov_cfg_path`: nearest config in `chain` that declares
            // `baseUrl` (so a child's `baseUrl` overrides its base's `baseUrl`), plus all
            // inherited baseUrls for bare-import fallback.
            let mut nearest_bu: Option<&[(String, String)]> = None;
            let mut all_bus: Vec<(String, String)> = Vec::new();
            for anc_cfg in &chain {
                if let Some(bus) = own_base_url_by_cfg.get(anc_cfg)
                    && !bus.is_empty()
                {
                    if nearest_bu.is_none() {
                        nearest_bu = Some(bus.as_slice());
                    }
                    for pair in bus {
                        if !all_bus.contains(pair) {
                            all_bus.push(pair.clone());
                        }
                    }
                }
            }
            if !all_bus.is_empty() {
                effective_base_url_by_dir.insert(dir.clone(), all_bus);
            }

            let effective_bases: Option<Vec<String>> = nearest_bu.map(|bu_list| {
                bu_list
                    .iter()
                    .map(|(bu, bu_dir)| {
                        let joined = match (bu_dir.is_empty(), bu.is_empty() || bu == ".") {
                            (true, true) => String::new(),
                            (true, false) => bu.clone(),
                            (false, true) => bu_dir.clone(),
                            (false, false) => format!("{bu_dir}/{bu}"),
                        };
                        normalize_lexical_rel(Path::new(&joined)).unwrap_or(joined)
                    })
                    .collect()
            });

            // Bind the nearest `paths` declaration in `chain` (INCLUDING an explicit empty
            // `"paths": {}` which stops inheritance cleanly!) against `gov_cfg_path`'s effective
            // `baseUrl` (or the declaring config file's directory when no `baseUrl` is set).
            let entry = aliases_by_dir.entry(dir.clone()).or_default();
            for anc_cfg in &chain {
                if has_paths_cfgs.contains(anc_cfg) {
                    let anc_dir = Path::new(anc_cfg)
                        .parent()
                        .map(normalize_rel_str)
                        .unwrap_or_default();
                    if let Some(raw_list) = raw_aliases_by_cfg.get(anc_cfg) {
                        for (alias_pat, target_pat) in raw_list {
                            if let Some(ref eff_bases) = effective_bases {
                                for eff_base in eff_bases {
                                    let triple =
                                        (alias_pat.clone(), target_pat.clone(), eff_base.clone());
                                    if !entry.contains(&triple) {
                                        entry.push(triple);
                                    }
                                }
                            } else {
                                let triple =
                                    (alias_pat.clone(), target_pat.clone(), anc_dir.clone());
                                if !entry.contains(&triple) {
                                    entry.push(triple);
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }

        // Bind ALL Vite aliases (`__dirname`-relative AND cwd-relative) in ONE unified
        // declaration-order sequence per config directory.
        for (dir, vite_list) in &raw_vite_aliases_by_dir {
            let vite_entry = resolved_vite_aliases_by_dir.entry(dir.clone()).or_default();
            for (alias_pat, target_pat, is_cwd) in vite_list {
                let nested_triple = (alias_pat.clone(), target_pat.clone(), dir.clone());
                if !vite_entry.contains(&nested_triple) {
                    vite_entry.push(nested_triple);
                }
                if *is_cwd {
                    if !dir.is_empty() {
                        let ws_triple = (alias_pat.clone(), target_pat.clone(), String::new());
                        if !vite_entry.contains(&ws_triple) {
                            vite_entry.push(ws_triple);
                        }
                    }
                    let amb = ambiguous_cwd_aliases_by_dir.entry(dir.clone()).or_default();
                    if !amb.contains(alias_pat) {
                        amb.push(alias_pat.clone());
                    }
                }
            }
        }
        let base_url_by_dir = effective_base_url_by_dir;

        let mut index = Self {
            pkg_to_dir,
            ts_config_dirs,
            aliases_by_dir,
            vite_aliases_by_dir: resolved_vite_aliases_by_dir,
            base_url_by_dir,
            subpath_imports_by_dir,
            vite_root_by_dir,
            ambiguous_vite_root_dirs,
            ambiguous_cwd_aliases_by_dir,
            pkg_exports,
            pkg_mains,
            reexports: BTreeMap::new(),
            wildcard_reexports: BTreeMap::new(),
            side_effectful_barrels: BTreeSet::new(),
        };

        // Two passes to resolve nested barrel (`index.ts` / `index.tsx`) re-exports
        let barrel_files: Vec<(&String, &FileNode)> = files
            .iter()
            .filter(|(path, _)| is_frontend_barrel_file(path))
            .collect();
        for _ in 0..2 {
            let mut next_reexports = index.reexports.clone();
            let mut next_wildcards = index.wildcard_reexports.clone();
            for &(path, node) in &barrel_files {
                for sym_def in &node.parsed.defined_symbols {
                    if let Some(rest) = sym_def.strip_prefix("reexport:")
                        && let Some((exported_sym, target_part)) = rest.split_once("->")
                        && let Some((module_spec, orig_sym)) = target_part.rsplit_once(':')
                    {
                        let targets = index.resolve_raw_specifier(path, module_spec, files);
                        for t in targets {
                            if is_frontend_barrel_file(&t)
                                && let Some(forwarded) =
                                    index.reexports.get(&(t.clone(), orig_sym.to_string()))
                            {
                                next_reexports
                                    .entry((path.clone(), exported_sym.to_string()))
                                    .or_default()
                                    .extend(forwarded.iter().cloned());
                            } else {
                                next_reexports
                                    .entry((path.clone(), exported_sym.to_string()))
                                    .or_default()
                                    .insert(t);
                            }
                        }
                    }
                }
                for spec in &node.parsed.imports {
                    if spec.symbols.iter().any(|s| s == "*") {
                        let targets = index.resolve_raw_specifier(path, &spec.module, files);
                        for t in targets {
                            if is_frontend_barrel_file(&t)
                                && let Some(forwarded) = index.wildcard_reexports.get(&t)
                            {
                                next_wildcards
                                    .entry(path.clone())
                                    .or_default()
                                    .extend(forwarded.iter().cloned());
                            }
                            next_wildcards.entry(path.clone()).or_default().insert(t);
                        }
                    }
                }
            }
            index.reexports = next_reexports;
            index.wildcard_reexports = next_wildcards;
        }

        // Propagate top-level module evaluation side effects across ALL static frontend and
        // stylesheet imports using an O(V + E) reverse-edge BFS worklist so deep import chains
        // never incur quadratic rebuild cost.
        let mut se_modules: BTreeSet<String> = BTreeSet::new();
        let mut fe_rev_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, node) in files {
            if is_stylesheet_path(path) {
                let is_global_css = !path.contains(".module.")
                    || node
                        .parsed
                        .defined_symbols
                        .iter()
                        .any(|s| s == "css_has_global");
                if is_global_css {
                    se_modules.insert(path.clone());
                }
                for spec in &node.parsed.imports {
                    for t in index.resolve_raw_specifier(path, &spec.module, files) {
                        if t != *path && is_stylesheet_path(&t) {
                            fe_rev_imports.entry(t).or_default().push(path.clone());
                        }
                    }
                }
                continue;
            }
            if !is_frontend_source_path(path) {
                continue;
            }
            let is_se = if is_frontend_barrel_file(path) {
                node.parsed
                    .defined_symbols
                    .iter()
                    .any(|s| s == "fe_has_side_effects")
            } else {
                node.parsed
                    .defined_symbols
                    .iter()
                    .any(|s| s == "fe_mod_side_effects")
            };
            if is_se {
                se_modules.insert(path.clone());
            }
            if !node.parsed.imports.is_empty() {
                for spec in &node.parsed.imports {
                    for t in index.resolve_raw_specifier(path, &spec.module, files) {
                        if t != *path && (is_frontend_source_path(&t) || is_stylesheet_path(&t)) {
                            fe_rev_imports.entry(t).or_default().push(path.clone());
                        }
                    }
                }
            }
        }
        let mut se_queue: VecDeque<String> = se_modules.iter().cloned().collect();
        while let Some(curr) = se_queue.pop_front() {
            if let Some(importers) = fe_rev_imports.get(&curr) {
                for imp in importers {
                    if se_modules.insert(imp.clone()) {
                        se_queue.push_back(imp.clone());
                    }
                }
            }
        }

        let mut se_barrels = BTreeSet::new();
        for &(path, _) in &barrel_files {
            if se_modules.contains(path) {
                se_barrels.insert(path.clone());
            }
        }
        index.side_effectful_barrels = se_barrels;

        index
    }

}

impl FrontendModuleIndex {
    pub(super) fn resolve_raw_specifier(
        &self,
        importer_path: &str,
        raw_module: &str,
        files: &BTreeMap<String, FileNode>,
    ) -> BTreeSet<String> {
        let mut resolved = BTreeSet::new();
        let parent = Path::new(importer_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let pkg_dir = frontend_pkg_dir(importer_path, files);
        // Walk ALL ancestor config directories from `importer_path` up to workspace root so a
        // nested `tsconfig.json` never shadows a parent `vite.config.ts` alias or `root` setting.
        let mut ancestor_cfg_dirs: Vec<String> = Vec::new();
        let mut anc_cur = Path::new(importer_path).parent();
        while let Some(d) = anc_cur {
            let ds = normalize_rel_str(d);
            if !ancestor_cfg_dirs.contains(&ds) {
                ancestor_cfg_dirs.push(ds);
            }
            anc_cur = d.parent();
        }
        if !ancestor_cfg_dirs.contains(&pkg_dir) {
            ancestor_cfg_dirs.push(pkg_dir.clone());
        }
        if !ancestor_cfg_dirs.contains(&String::new()) {
            ancestor_cfg_dirs.push(String::new());
        }

        // 0. Vite `import.meta.glob('...')` pattern expansion (with configured alias, Vite `root`,
        //    and optional `{ caseSensitive: false }` via `glob_ci:`).
        let (is_glob_ci, glob_pat_opt) = if let Some(p) = raw_module.strip_prefix("glob_ci:") {
            (true, Some(p))
        } else if let Some(p) = raw_module.strip_prefix("glob:") {
            (false, Some(p))
        } else {
            (false, None)
        };
        if let Some(glob_pat) = glob_pat_opt {
            let mut norm_globs: Vec<String> = Vec::new();
            if glob_pat.starts_with("./") || glob_pat.starts_with("../") {
                norm_globs.push(
                    normalize_lexical_rel(&parent.join(glob_pat))
                        .unwrap_or_else(|| glob_pat.to_string()),
                );
            } else if let Some(rest) = glob_pat.strip_prefix('/') {
                // Search ALL ancestors for a Vite/Vitest config (with optional configured `root`)
                // FIRST before falling back to nearest `package.json`, so a nested `package.json`
                // never shadows an ancestor Vite root.
                let mut vite_roots: Vec<String> = Vec::new();
                for ds in &ancestor_cfg_dirs {
                    if let Some(configured_roots) = self.vite_root_by_dir.get(ds) {
                        vite_roots.extend(configured_roots.iter().cloned());
                        break;
                    }
                    let has_vite_cfg = [
                        "vite.config.ts",
                        "vite.config.mts",
                        "vite.config.js",
                        "vite.config.mjs",
                        "vite.config.cjs",
                        "vitest.config.ts",
                        "vitest.config.mts",
                        "vitest.config.js",
                        "vitest.config.mjs",
                        "vitest.config.cjs",
                    ]
                    .iter()
                    .any(|cfg| {
                        let p = if ds.is_empty() {
                            (*cfg).to_string()
                        } else {
                            format!("{ds}/{cfg}")
                        };
                        files.contains_key(&p)
                    });
                    if has_vite_cfg {
                        vite_roots.push(ds.clone());
                        break;
                    }
                }
                if vite_roots.is_empty() {
                    for ds in &ancestor_cfg_dirs {
                        let pj = if ds.is_empty() {
                            "package.json".to_string()
                        } else {
                            format!("{ds}/package.json")
                        };
                        if files.contains_key(&pj) {
                            vite_roots.push(ds.clone());
                            break;
                        }
                    }
                }
                if vite_roots.is_empty() {
                    vite_roots.push(String::new());
                }
                for vr in vite_roots {
                    norm_globs.push(if vr.is_empty() {
                        rest.to_string()
                    } else {
                        format!("{vr}/{rest}")
                    });
                }
            } else {
                let mut matched_alias = false;
                // Check Vite aliases FIRST in exact declaration order so `import.meta.glob('@ui/*.ts')`
                // with a Vite-only alias expands cleanly — resolving ALL cwd candidates for the
                // winning alias pattern before stopping!
                'vite_glob: for lookup_dir in &ancestor_cfg_dirs {
                    if let Some(vite_aliases) = self.vite_aliases_by_dir.get(lookup_dir) {
                        let mut winning_pat: Option<&str> = None;
                        for (alias_pat, target_pat, declaring_dir) in vite_aliases {
                            if let Some(wp) = winning_pat
                                && alias_pat.as_str() != wp
                            {
                                break;
                            }
                            if alias_pat.ends_with('/')
                                && let Some(rest) = glob_pat.strip_prefix(alias_pat.as_str())
                            {
                                winning_pat = Some(alias_pat.as_str());
                                matched_alias = true;
                                let rel_target = format!("{target_pat}{rest}");
                                let mapped = if declaring_dir.is_empty() {
                                    rel_target
                                } else {
                                    format!("{declaring_dir}/{rel_target}")
                                };
                                norm_globs.push(
                                    normalize_lexical_rel(Path::new(&mapped)).unwrap_or(mapped),
                                );
                            }
                        }
                        if winning_pat.is_some() {
                            break 'vite_glob;
                        }
                    }
                }
                if !matched_alias
                    && let Some(lookup_dir) = ancestor_cfg_dirs
                        .iter()
                        .find(|ds| self.ts_config_dirs.contains(ds.as_str()))
                    && let Some(aliases) = self.aliases_by_dir.get(lookup_dir)
                {
                    for (alias_pat, target_pat, declaring_dir) in aliases {
                        let expanded = if let Some((pfx, sfx)) = alias_pat.split_once('*') {
                            if sfx.is_empty() && glob_pat.starts_with(pfx) {
                                let rest = &glob_pat[pfx.len()..];
                                let tpfx = target_pat.split('*').next().unwrap_or(target_pat);
                                Some(format!("{tpfx}{rest}"))
                            } else {
                                None
                            }
                        } else if alias_pat.ends_with('/') {
                            glob_pat
                                .strip_prefix(alias_pat.as_str())
                                .map(|rest| format!("{target_pat}{rest}"))
                        } else {
                            None
                        };
                        if let Some(rel_target) = expanded {
                            matched_alias = true;
                            let mapped = if declaring_dir.is_empty() {
                                rel_target
                            } else {
                                format!("{declaring_dir}/{rel_target}")
                            };
                            norm_globs.push(
                                normalize_lexical_rel(Path::new(&mapped)).unwrap_or(mapped),
                            );
                        }
                    }
                }
                if !matched_alias
                    && let Some(rest) = glob_pat
                        .strip_prefix("@/")
                        .or_else(|| glob_pat.strip_prefix("~/"))
                {
                    norm_globs.push(if pkg_dir.is_empty() {
                        format!("src/{rest}")
                    } else {
                        format!("{pkg_dir}/src/{rest}")
                    });
                    norm_globs.push(if pkg_dir.is_empty() {
                        rest.to_string()
                    } else {
                        format!("{pkg_dir}/{rest}")
                    });
                } else if !matched_alias {
                    norm_globs.push(glob_pat.to_string());
                }
            }
            for norm_glob in norm_globs {
                if let Ok(glob) = globset::GlobBuilder::new(&norm_glob)
                    .case_insensitive(is_glob_ci)
                    .build()
                {
                    let matcher = glob.compile_matcher();
                    for candidate_path in files.keys() {
                        if candidate_path != importer_path && matcher.is_match(candidate_path) {
                            resolved.insert(candidate_path.clone());
                        }
                    }
                }
            }
            return resolved;
        }

        let Some(clean_mod) = clean_frontend_import_specifier(raw_module) else {
            return resolved;
        };

        // 0a. Vite aliases (evaluated FIRST in exact declaration order — BEFORE `package.json`
        //     `#imports` so a Vite alias for `#db` takes precedence over `"#db"` in `package.json`!).
        if !clean_mod.starts_with("./")
            && !clean_mod.starts_with("../")
            && !clean_mod.starts_with('/')
        {
            for lookup_dir in &ancestor_cfg_dirs {
                if let Some(vite_aliases) = self.vite_aliases_by_dir.get(lookup_dir) {
                    let mut winning_pat: Option<&str> = None;
                    for (alias_pat, target_pat, declaring_dir) in vite_aliases {
                        if let Some(wp) = winning_pat
                            && alias_pat.as_str() != wp
                        {
                            break;
                        }
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
                            winning_pat = Some(alias_pat.as_str());
                            let mapped = if declaring_dir.is_empty() {
                                rel_target
                            } else {
                                format!("{declaring_dir}/{rel_target}")
                            };
                            if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                                add_frontend_path_candidates(&norm, files, &mut resolved);
                            }
                        }
                    }
                    if winning_pat.is_some() {
                        return resolved;
                    }
                }
            }
        }

        // 0b. Node.js / Package `#...` Subpath Imports (`#utils/*` -> `./src/utils/*.js` OR `@org/util`)
        // Resolve the owning `package.json` directory independently of any nested `tsconfig.json`.
        if clean_mod.starts_with('#') {
            // Node `PACKAGE_IMPORTS_RESOLVE` consults ONLY the single nearest enclosing
            // `package.json` and applies `PATTERN_KEY_COMPARE` specificity precedence: exact
            // match > longest wildcard prefix > longest pattern key (so an exact `"#secret"`
            // pointing to an ignored file is NEVER masked by a wildcard `"#*"`), replacing ALL
            // `*` occurrences in the target string!
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
            if let Some(lookup_dir) = nearest_pkg_dir
                && let Some(rules) = self.subpath_imports_by_dir.get(&lookup_dir)
            {
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
                for rel in winning_rels {
                    let is_explicit_rel =
                        rel.starts_with("./") || rel.starts_with("../") || rel.starts_with('/');
                    // If a subpath import maps to a bare workspace package (`"#util": "@org/util"`),
                    // route it through workspace package/exports resolution instead of treating it as a path!
                    let is_bare_pkg = !is_explicit_rel
                        && self.pkg_to_dir.keys().any(|pkg| {
                            rel == *pkg || rel.starts_with(&format!("{pkg}/"))
                        });
                    if is_bare_pkg {
                        resolved.extend(self.resolve_raw_specifier(importer_path, &rel, files));
                    } else {
                        let mapped = if lookup_dir.is_empty() {
                            rel
                        } else {
                            format!("{lookup_dir}/{rel}")
                        };
                        if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                            add_frontend_path_candidates(&norm, files, &mut resolved);
                        }
                    }
                }
            }
            return resolved;
        }

        // 1. Relative import (`./foo`, `../bar`)
        if clean_mod.starts_with("./")
            || clean_mod.starts_with("../")
            || clean_mod == "."
            || clean_mod == ".."
        {
            if let Some(norm) = normalize_lexical_rel(&parent.join(&clean_mod)) {
                add_frontend_path_candidates(&norm, files, &mut resolved);
            }
            return resolved;
        }

        // 2. Root-relative import (`/src/...`) — when Vite `root` is configured, resolve ONLY
        //    against the configured Vite root(s) so a gitignored `app/src/generated.js` is never
        //    masked by an unrelated workspace-root `src/generated.js`.
        if let Some(stripped_root) = clean_mod.strip_prefix('/') {
            let mut matched_vite_root_cfg = false;
            for ds in &ancestor_cfg_dirs {
                if let Some(vrs) = self.vite_root_by_dir.get(ds) {
                    matched_vite_root_cfg = true;
                    for vr in vrs {
                        let vr_cand = if vr.is_empty() {
                            stripped_root.to_string()
                        } else {
                            format!("{vr}/{stripped_root}")
                        };
                        add_frontend_path_candidates(&vr_cand, files, &mut resolved);
                    }
                    break;
                }
            }
            if !matched_vite_root_cfg {
                let cand = if pkg_dir.is_empty() {
                    stripped_root.to_string()
                } else {
                    format!("{pkg_dir}/{stripped_root}")
                };
                add_frontend_path_candidates(&cand, files, &mut resolved);
                if resolved.is_empty() {
                    add_frontend_path_candidates(stripped_root, files, &mut resolved);
                }
            }
            return resolved;
        }

        // 3a. Vite aliases (evaluated in EXACT declaration order — FIRST matching alias pattern
        //     wins, resolving ALL cwd candidates for that winning pattern so `affected_tests`
        //     tracks both package-relative and workspace-relative targets for cwd-relative aliases!).
        for lookup_dir in &ancestor_cfg_dirs {
            if let Some(vite_aliases) = self.vite_aliases_by_dir.get(lookup_dir) {
                let mut winning_pat: Option<&str> = None;
                for (alias_pat, target_pat, declaring_dir) in vite_aliases {
                    if let Some(wp) = winning_pat
                        && alias_pat.as_str() != wp
                    {
                        break;
                    }
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
                        winning_pat = Some(alias_pat.as_str());
                        let mapped = if declaring_dir.is_empty() {
                            rel_target
                        } else {
                            format!("{declaring_dir}/{rel_target}")
                        };
                        if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                            add_frontend_path_candidates(&norm, files, &mut resolved);
                        }
                    }
                }
                if winning_pat.is_some() {
                    return resolved;
                }
            }
        }

        // 3b. Governing TypeScript/JSConfig `paths` aliases: select the nearest ancestor dir in
        //     `ts_config_dirs` ONLY (so a nested `vite.config.ts` never shadows a parent tsconfig!),
        //     then rank ONLY its effective mappings by exact > longest PREFIX-ONLY wildcard length!
        let governing_ts_dir = ancestor_cfg_dirs
            .iter()
            .find(|ds| self.ts_config_dirs.contains(ds.as_str()));
        let mut best_alias_rank: Option<(u8, usize)> = None;
        let mut winning_alias_targets: Vec<String> = Vec::new();
        if let Some(lookup_dir) = governing_ts_dir
            && let Some(aliases) = self.aliases_by_dir.get(lookup_dir)
        {
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
                                // Fixed target for wildcard alias (e.g. `"@app/*": ["shared.ts"]`):
                                // per TypeScript semantics, do NOT append `mid`.
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
        }
        // Stop fallback resolution once a configured alias matched (so an alias like
        // `lib -> generated` or `@lib/secret -> generated/secret` never falls through to
        // lower-specificity aliases, `src/lib/foo.ts`, or workspace packages when ignored!).
        if best_alias_rank.is_some() {
            for mapped in winning_alias_targets {
                if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                    add_frontend_path_candidates(&norm, files, &mut resolved);
                }
            }
            return resolved;
        }
        let matched_configured_alias = false;

        // 4. Conventional `@/` or `~/` alias to `{pkg_dir}/src/` or `{pkg_dir}/` — ONLY when no
        //    configured alias matched (otherwise a gitignored `@/foo.js -> generated/foo.js` would
        //    falsely resolve to `src/foo.js`!).
        if !matched_configured_alias
            && let Some(rest) = clean_mod
                .strip_prefix("@/")
                .or_else(|| clean_mod.strip_prefix("~/"))
        {
            for base in [
                if pkg_dir.is_empty() {
                    format!("src/{rest}")
                } else {
                    format!("{pkg_dir}/src/{rest}")
                },
                if pkg_dir.is_empty() {
                    rest.to_string()
                } else {
                    format!("{pkg_dir}/{rest}")
                },
                format!("src/{rest}"),
            ] {
                add_frontend_path_candidates(&base, files, &mut resolved);
            }
            if !resolved.is_empty() {
                return resolved;
            }
        }

        // 5. Monorepo workspace package import (`@scope/ui` or `@scope/ui/Button`)
        for (pkg_name, target_pkg_dir) in &self.pkg_to_dir {
            if &clean_mod == pkg_name {
                if let Some(exp_rules) = self.pkg_exports.get(pkg_name) {
                    // When `package.json` declares `exports`, Node strictly enforces `exports`
                    // and NEVER falls back to `src/index.ts` when an export target is unindexed!
                    for (sub_k, target_v) in exp_rules {
                        if sub_k == "." || sub_k.is_empty() {
                            let mapped = if target_pkg_dir.is_empty() {
                                target_v.clone()
                            } else {
                                format!("{target_pkg_dir}/{target_v}")
                            };
                            add_frontend_path_candidates(&mapped, files, &mut resolved);
                        }
                    }
                } else if let Some(main_entries) = self.pkg_mains.get(pkg_name)
                    && !main_entries.is_empty()
                {
                    for target_v in main_entries {
                        let mapped = if target_pkg_dir.is_empty() {
                            target_v.clone()
                        } else {
                            format!("{target_pkg_dir}/{target_v}")
                        };
                        add_frontend_path_candidates(&mapped, files, &mut resolved);
                    }
                } else {
                    for entry_prefix in [
                        if target_pkg_dir.is_empty() {
                            "src".to_string()
                        } else {
                            format!("{target_pkg_dir}/src")
                        },
                        target_pkg_dir.clone(),
                    ] {
                        add_frontend_path_candidates(&entry_prefix, files, &mut resolved);
                    }
                }
            } else if let Some(subpath) = clean_mod
                .strip_prefix(pkg_name.as_str())
                .and_then(|s| s.strip_prefix('/'))
            {
                if let Some(exp_rules) = self.pkg_exports.get(pkg_name) {
                    for (sub_k, target_v) in exp_rules {
                        if let Some(rel_target) =
                            match_package_export_subpath(subpath, sub_k, target_v)
                        {
                            let mapped = if target_pkg_dir.is_empty() {
                                rel_target
                            } else {
                                format!("{target_pkg_dir}/{rel_target}")
                            };
                            add_frontend_path_candidates(&mapped, files, &mut resolved);
                        }
                    }
                } else {
                    for base in [
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
                    ] {
                        add_frontend_path_candidates(&base, files, &mut resolved);
                    }
                }
            }
        }

        // 6. Configured `compilerOptions.baseUrl` bare-import resolution across ALL ancestor
        //    config directories (so nested packages inherit an ancestor tsconfig's `baseUrl`!).
        for lookup_dir in &ancestor_cfg_dirs {
            if let Some(base_urls) = self.base_url_by_dir.get(lookup_dir) {
                for (base_url, declaring_dir) in base_urls {
                    let mapped = match (
                        declaring_dir.is_empty(),
                        base_url.is_empty() || base_url == ".",
                    ) {
                        (true, true) => clean_mod.to_string(),
                        (true, false) => format!("{base_url}/{clean_mod}"),
                        (false, true) => format!("{declaring_dir}/{clean_mod}"),
                        (false, false) => format!("{declaring_dir}/{base_url}/{clean_mod}"),
                    };
                    if let Some(norm) = normalize_lexical_rel(Path::new(&mapped)) {
                        add_frontend_path_candidates(&norm, files, &mut resolved);
                    }
                }
            }
        }
        if resolved.is_empty() && !clean_mod.starts_with('@') {
            let base = if pkg_dir.is_empty() {
                format!("src/{clean_mod}")
            } else {
                format!("{pkg_dir}/src/{clean_mod}")
            };
            add_frontend_path_candidates(&base, files, &mut resolved);
        }

        resolved
    }
}


pub(super) fn add_frontend_path_candidates(
    target_path: &str,
    files: &BTreeMap<String, FileNode>,
    resolved: &mut BTreeSet<String>,
) {
    let norm = target_path.trim_matches('/');
    if norm.is_empty() {
        for idx_file in FRONTEND_INDEX_FILES {
            if files.contains_key(*idx_file) {
                resolved.insert((*idx_file).to_string());
            }
        }
        return;
    }

    // 1. Exact file match (e.g. `styles.module.css`, `logo.svg`, `helper.ts`)
    if files.contains_key(norm) {
        resolved.insert(norm.to_string());
    }

    // 2. TypeScript NodeNext/Bundler `.js` / `.jsx` / `.mjs` -> `.ts` / `.tsx` / `.mts` remapping
    //    (do NOT remap to `.d.ts`: declaration files are not executable runtime targets, so a
    //    gitignored `foo.js` with sibling `foo.d.ts` must fail closed instead of appearing resolved!)
    for (js_ext, ts_exts) in [
        (".js", &[".ts", ".tsx"][..]),
        (".jsx", &[".tsx", ".ts"][..]),
        (".mjs", &[".mts", ".ts"][..]),
        (".cjs", &[".cts", ".ts"][..]),
    ] {
        if let Some(stem) = norm.strip_suffix(js_ext) {
            for ts_ext in ts_exts {
                let cand = format!("{stem}{ts_ext}");
                if files.contains_key(&cand) {
                    resolved.insert(cand);
                }
            }
        }
    }

    // 3. Extensionless module path -> try frontend extensions
    for ext in FRONTEND_RESOLVE_EXTS {
        let cand = format!("{norm}{ext}");
        if files.contains_key(&cand) {
            resolved.insert(cand);
        }
    }

    // 4a. Directory `package.json` `main`/`module`/`browser` entrypoint (`require('./lib')` ->
    //     `lib/package.json` `"main": "./actual.js"` takes precedence over `lib/index.js` in Node!)
    //     Only treat `lib/package.json` as resolved when its entrypoint actually resolves to an
    //     indexed non-declaration runtime file. Keep Node's `index.*` fallback whenever `"main"`
    //     is absent (e.g. only `"module"` is declared!) or its `"main"` target does not exist
    //     (unindexed on-disk `"main"` files are still caught and fail-closed by
    //     `remapped_js_masked_by_unindexed_disk_file`!).
    let dir_pkg_path = format!("{norm}/package.json");
    let mut exact_main_resolved = false;
    if let Some(pkg_node) = files.get(&dir_pkg_path) {
        let mut dir_mains: Vec<&str> = Vec::new();
        let mut exact_mains: Vec<&str> = Vec::new();
        for sym in &pkg_node.parsed.defined_symbols {
            if let Some(entry_val) = sym.strip_prefix("fe_dir_main_exact:") {
                exact_mains.push(entry_val);
            } else if let Some(entry_val) = sym.strip_prefix("fe_dir_main:") {
                dir_mains.push(entry_val);
            }
        }
        let resolve_entries = |entries: &[&str]| -> BTreeSet<String> {
            let mut targets = BTreeSet::new();
            for entry_val in entries {
                if let Some(mapped) = normalize_lexical_rel(&Path::new(norm).join(entry_val)) {
                    if files.contains_key(&mapped) {
                        targets.insert(mapped.clone());
                    }
                    for ext in FRONTEND_RESOLVE_EXTS {
                        let cand = format!("{mapped}{ext}");
                        if files.contains_key(&cand) {
                            targets.insert(cand);
                        }
                    }
                    for idx_file in FRONTEND_INDEX_FILES {
                        let cand = format!("{mapped}/{idx_file}");
                        if files.contains_key(&cand) {
                            targets.insert(cand);
                        }
                    }
                }
            }
            targets
        };
        let exact_targets = resolve_entries(&exact_mains);
        let all_targets = resolve_entries(&dir_mains);
        if exact_targets
            .iter()
            .any(|t| !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts"))
        {
            exact_main_resolved = true;
        }
        if all_targets
            .iter()
            .any(|t| !t.ends_with(".d.ts") && !t.ends_with(".d.mts") && !t.ends_with(".d.cts"))
        {
            resolved.insert(dir_pkg_path);
            resolved.extend(all_targets);
        }
    }

    // 4b. Directory barrel (`index.tsx`, `index.ts`, `index.jsx`, `index.js`) — included whenever
    //     `<dir>/package.json` has no resolved `"main"` (covering both `"module"`-only manifests
    //     and Node's fallback when `"main"` points to a non-existent file; if `"main"` exists
    //     unindexed on disk, `remapped_js_masked_by_unindexed_disk_file` fails closed!).
    if !exact_main_resolved {
        for idx_file in FRONTEND_INDEX_FILES {
            let cand = format!("{norm}/{idx_file}");
            if files.contains_key(&cand) {
                resolved.insert(cand);
            }
        }
    }

    // 5. Sass / SCSS partial convention (`styles/tokens` -> `styles/_tokens.scss`, `styles/_index.scss`)
    let parent = Path::new(norm)
        .parent()
        .map(normalize_rel_str)
        .unwrap_or_default();
    if let Some(file_name) = Path::new(norm).file_name().and_then(|n| n.to_str())
        && !file_name.starts_with('_')
    {
        let partial_base = if parent.is_empty() {
            format!("_{file_name}")
        } else {
            format!("{parent}/_{file_name}")
        };
        if files.contains_key(&partial_base) {
            resolved.insert(partial_base.clone());
        }
        for ext in [".scss", ".sass", ".less", ".styl", ".stylus", ".css"] {
            let cand = format!("{partial_base}{ext}");
            if files.contains_key(&cand) {
                resolved.insert(cand);
            }
        }
    }
    for idx_scss in ["_index.scss", "index.scss", "_index.sass", "index.sass"] {
        let cand = format!("{norm}/{idx_scss}");
        if files.contains_key(&cand) {
            resolved.insert(cand);
        }
    }
}


/// Returns true when a Vite/frontend import specifier uses a query parameter whose runtime value
/// reflects exact file bytes (e.g. `?raw`, `?inline`, `?url`), so comment-only edits must hash raw.
pub(super) fn frontend_specifier_has_raw_query(raw: &str) -> bool {
    let Some((_, query)) = raw.split_once('?') else {
        return false;
    };
    query.split('&').any(|part| {
        let key = part.split('=').next().unwrap_or("").trim();
        matches!(key, "raw" | "inline" | "url")
    })
}

/// Returns true for ANY JS/TS source OR config file extension (`.js`, `.ts`, `.jsx`, `.tsx`,
/// `.mjs`, `.cjs`, `.mts`, `.cts`), including `jest.config.ts`, `vite.config.ts`, etc.
pub(super) fn is_js_or_ts_file_ext(rel_path: &str) -> bool {
    matches!(
        Path::new(rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("js" | "ts" | "jsx" | "tsx" | "mjs" | "cjs" | "mts" | "cts")
    )
}

pub fn clean_frontend_import_specifier(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("node:")
        || trimmed.starts_with("virtual:")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("data:")
        || trimmed.contains('\0')
    {
        return None;
    }
    // Strip Vite query parameters such as `?raw`, `?url`, `?worker`, `?inline`, `?react`
    let without_query = trimmed.split('?').next().unwrap_or(trimmed).trim();
    if without_query.is_empty() {
        None
    } else {
        Some(without_query.replace('\\', "/"))
    }
}


