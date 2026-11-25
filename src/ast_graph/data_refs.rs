use super::*;

impl DependencyGraph {
    pub fn discover_referenced_data_files(&mut self, workspace: &Workspace, policy: &MemoryPolicy) {
        let _ = self.discover_referenced_data_files_with_cache(workspace, policy, &BTreeMap::new());
    }

    pub(super) fn discover_referenced_data_files_with_cache(
        &mut self,
        workspace: &Workspace,
        policy: &MemoryPolicy,
        persisted: &BTreeMap<String, FileNode>,
    ) -> usize {
        // Reset budget flag at the start of each complete discovery pass so removing an oversized
        // directive and rediscovering cleanly re-enables caching.
        // Reset discovery-derived state at the START of each pass before populating it below.
        self.workspace_root = Some(workspace.root.clone());
        self.discovery_budget_exceeded = false;
        self.resolved_go_replaces_by_mod.clear();
        self.referenced_data_dirs.clear();
        let mut total_fresh = 0usize;
        let mut total_source_bytes: usize = self
            .files
            .values()
            .filter_map(|n| n.stamp.map(|s| usize::try_from(s.size).unwrap_or(0)))
            .sum();
        let mut pkg_to_dir: BTreeMap<String, String> = BTreeMap::new();
        let mut worklist: VecDeque<String> = VecDeque::new();
        let mut visited_sources: BTreeSet<String> = BTreeSet::new();
        // Group Go `//go:embed` patterns AND cgo `-I` include directories by package directory.
        let mut go_embeds_by_dir: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut cgo_inc_by_pkg: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut cgo_pkg_dirs: BTreeSet<String> = BTreeSet::new();
        // Worklist entries carry `(orig_pkg_dir, current_file_dir, hdr_mod, is_angle_sys)` so
        // nested headers inherit BOTH their own relative dir AND the original Go package's `-I`
        // dirs, while unresolved non-standard `<vendor/api.h>` system headers fail closed.
        let mut cgo_hdr_worklist: VecDeque<(String, String, String, bool)> = VecDeque::new();
        let canon_ws_root =
            std::fs::canonicalize(&workspace.root).unwrap_or_else(|_| workspace.root.clone());
        let resolve_cgo_inc_dir = |pkg_dir: &str, raw_inc: &str| -> Option<String> {
            let p = Path::new(raw_inc);
            if p.is_absolute() {
                if let Ok(rel) = p.strip_prefix(&workspace.root) {
                    return Some(normalize_rel_str(rel));
                }
                if let Ok(canon_p) = std::fs::canonicalize(p)
                    && let Ok(rel) = canon_p.strip_prefix(&canon_ws_root)
                {
                    return Some(normalize_rel_str(rel));
                }
                // External absolute `-I` directory outside workspace: cannot be watched.
                return None;
            }
            // Resolve relative `-I` against `pkg_dir`, allowing the workspace root itself (`""`
            // for e.g. `-I${SRCDIR}/..` inside `pkg/`), and returning `None` ONLY when `..`
            // actually escapes above `workspace.root`.
            normalize_lexical_rel_allow_root(&Path::new(pkg_dir).join(p))
        };
        // Collect ONLY the nearest effective `go.work` directory for each indexed Go module/source
        // so an inactive outer `outer/go.work` above a nearer `outer/inner/go.work` does not
        // falsely mark discovery incomplete when it contains an external `use`.
        let mut active_auto_gowork_dirs: BTreeSet<String> = BTreeSet::new();
        for p in self.files.keys() {
            if p == "go.mod" || p.ends_with("/go.mod") || p.ends_with(".go") {
                let mut cur = Path::new(p)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                loop {
                    let cand_gw = if cur.is_empty() {
                        "go.work".to_string()
                    } else {
                        format!("{cur}/go.work")
                    };
                    if self.files.contains_key(&cand_gw) || workspace.root.join(&cand_gw).is_file()
                    {
                        active_auto_gowork_dirs.insert(cur);
                        break;
                    }
                    if cur.is_empty() {
                        break;
                    }
                    cur = cur
                        .rsplit_once('/')
                        .map_or(String::new(), |(par, _)| par.to_string());
                }
            }
        }
        // Check whether a resolved Go `replace`/`use` directory itself OR any descendant `.go` /
        // native / `go.mod` file inside it is excluded/gitignored (so `.gitignore` excluding
        // `lib/*.go` while allowing `lib/` still fails closed instead of producing stale hits!).
        let go_dir_has_ignored_sources = |rel_dir: &str| -> bool {
            if !rel_dir.is_empty() && workspace.is_excluded_or_ignored_rel(Path::new(rel_dir)) {
                return true;
            }
            let start_abs = if rel_dir.is_empty() {
                workspace.root.clone()
            } else {
                workspace.root.join(rel_dir)
            };
            if !start_abs.is_dir() {
                return false;
            }
            let mut stack: Vec<(std::path::PathBuf, String)> =
                vec![(start_abs, rel_dir.to_string())];
            let mut visited_entries = 0usize;
            while let Some((dir_abs, dir_rel)) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir_abs) else {
                    return true;
                };
                for ent in entries.flatten() {
                    visited_entries += 1;
                    if visited_entries > 4096 {
                        return true;
                    }
                    let fname = ent.file_name();
                    let Some(fname_s) = fname.to_str() else {
                        continue;
                    };
                    if fname_s == ".git" {
                        continue;
                    }
                    let child_rel = if dir_rel.is_empty() {
                        fname_s.to_string()
                    } else {
                        format!("{dir_rel}/{fname_s}")
                    };
                    let child_abs = ent.path();
                    // Fail closed on ANY symlinked file or directory inside a Go target (workspace
                    // indexing skips symlinks, while `go test` follows them!).
                    let Ok(sym_meta) = std::fs::symlink_metadata(&child_abs) else {
                        return true;
                    };
                    if sym_meta.file_type().is_symlink() {
                        return true;
                    }
                    let is_dir = sym_meta.is_dir();
                    if workspace.is_excluded_or_ignored_rel(Path::new(&child_rel)) {
                        if !is_dir
                            && (child_rel.ends_with(".go")
                                || child_rel.ends_with("/go.mod")
                                || child_rel == "go.mod"
                                || is_cgo_native_source_path(&child_rel)
                                || child_rel.ends_with(".syso")
                                || child_rel.ends_with(".swig")
                                || child_rel.ends_with(".swigcxx"))
                        {
                            return true;
                        }
                        if is_dir && !matches!(fname_s, "node_modules" | "target" | ".venv") {
                            // Ignored subdirectory (including `vendor/`!) inside an active Go
                            // replace/use target could hold active Go/vendored sources.
                            return true;
                        }
                        continue;
                    }
                    if is_dir {
                        stack.push((child_abs, child_rel));
                    }
                }
            }
            false
        };
        for (path, node) in &self.files {
            if node.is_config {
                let dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                for sym in &node.parsed.defined_symbols {
                    if let Some(pkg_name) = sym.strip_prefix("fe_pkg:") {
                        pkg_to_dir.insert(pkg_name.to_string(), dir.clone());
                    }
                }
            }
            if path.ends_with(".go") {
                let dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                if node.parsed.imports.iter().any(|imp| imp.module == "C") {
                    cgo_pkg_dirs.insert(dir.clone());
                }
                for sym in &node.parsed.defined_symbols {
                    if let Some(embed_pat) = sym.strip_prefix("go_embed:") {
                        go_embeds_by_dir
                            .entry(dir.clone())
                            .or_default()
                            .insert(embed_pat.to_string());
                    } else if let Some(inc_dir) = sym.strip_prefix("cgo_inc:") {
                        cgo_pkg_dirs.insert(dir.clone());
                        if let Some(norm_inc) = resolve_cgo_inc_dir(&dir, inc_dir) {
                            cgo_inc_by_pkg
                                .entry(dir.clone())
                                .or_default()
                                .insert(norm_inc);
                        } else {
                            // External absolute OR escaping relative `-I` path (`-I${SRCDIR}/../shared`)
                            // cannot be watched inside the workspace; fail closed.
                            self.discovery_budget_exceeded = true;
                        }
                    } else if let Some(hdr) = sym.strip_prefix("c_include_opt:") {
                        cgo_pkg_dirs.insert(dir.clone());
                        cgo_hdr_worklist.push_back((
                            dir.clone(),
                            dir.clone(),
                            hdr.to_string(),
                            true,
                        ));
                    } else if let Some(hdr) = sym.strip_prefix("c_include_sys:") {
                        cgo_pkg_dirs.insert(dir.clone());
                        cgo_hdr_worklist.push_back((
                            dir.clone(),
                            dir.clone(),
                            hdr.to_string(),
                            false,
                        ));
                    } else if let Some(hdr) = sym.strip_prefix("c_include:") {
                        cgo_pkg_dirs.insert(dir.clone());
                        cgo_hdr_worklist.push_back((
                            dir.clone(),
                            dir.clone(),
                            hdr.to_string(),
                            false,
                        ));
                    }
                }
            }
            let gowork_env_disc = effective_gowork_value();
            let gowork_disabled = gowork_env_disc.as_deref().map(str::trim) == Some("off");
            let gowork_is_explicit_disc = gowork_env_disc
                .as_deref()
                .map(str::trim)
                .is_some_and(|v| !v.is_empty() && v != "auto" && v != "off");
            let is_go_work = path == "go.work" || path.ends_with("/go.work");
            let mod_dir = Path::new(path)
                .parent()
                .map(normalize_rel_str)
                .unwrap_or_default();
            if path == "go.mod"
                || path.ends_with("/go.mod")
                || (!gowork_disabled
                    && !gowork_is_explicit_disc
                    && is_go_work
                    && active_auto_gowork_dirs.contains(&mod_dir))
            {
                let base_abs = if mod_dir.is_empty() {
                    workspace.root.clone()
                } else {
                    workspace.root.join(&mod_dir)
                };
                for sym in &node.parsed.defined_symbols {
                    if let Some(repl) = sym.strip_prefix("go_replace:")
                        && let Some((old_mod, target)) = repl.split_once('=')
                    {
                        if let Some(resolved_rel) =
                            resolve_go_local_dir_no_symlink(&workspace.root, &base_abs, target)
                            && !go_dir_has_ignored_sources(&resolved_rel)
                        {
                            if !old_mod.is_empty() {
                                let stored_key = if is_go_work {
                                    format!("__gowork__:{old_mod}")
                                } else {
                                    old_mod.to_string()
                                };
                                self.resolved_go_replaces_by_mod
                                    .entry(mod_dir.clone())
                                    .or_default()
                                    .push((stored_key, resolved_rel));
                            }
                        } else {
                            // Local `go.mod`/`go.work` `replace` points outside the workspace,
                            // through a symlink, or to an excluded/gitignored target: fail closed.
                            self.discovery_budget_exceeded = true;
                        }
                    } else if let Some(use_target) = sym.strip_prefix("go_work_use:") {
                        if let Some(resolved_rel) =
                            resolve_go_local_dir_no_symlink(&workspace.root, &base_abs, use_target)
                            && !go_dir_has_ignored_sources(&resolved_rel)
                        {
                            self.resolved_go_replaces_by_mod
                                .entry(mod_dir.clone())
                                .or_default()
                                .push(("__gowork_use__".to_string(), resolved_rel));
                        } else {
                            self.discovery_budget_exceeded = true;
                        }
                    }
                }
            }
            if !node.parsed.data_refs.is_empty() && visited_sources.insert(path.clone()) {
                worklist.push_back(path.clone());
            }
        }
        // Check active external `GOWORK` or nearest ancestor `go.work` from EVERY Go module
        // directory (including nested ignored `go.work` files) when the workspace contains Go
        // sources and `GOWORK != "off"`: parse their `use` / `replace` directives and fail closed
        // if any referenced module lives outside the workspace or behind a symlink.
        if self.files.keys().any(|p| p.ends_with(".go")) {
            let gowork_env = effective_gowork_value();
            let gowork_trimmed = gowork_env.as_deref().map(str::trim);
            if gowork_trimmed != Some("off") {
                let mut gowork_paths: BTreeSet<std::path::PathBuf> = BTreeSet::new();
                if let Some(explicit) = gowork_trimmed.filter(|v| !v.is_empty() && *v != "auto") {
                    let p = Path::new(explicit);
                    gowork_paths.insert(if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        workspace.root.join(p)
                    });
                } else {
                    let mut search_roots: BTreeSet<std::path::PathBuf> = BTreeSet::new();
                    search_roots.insert(workspace.root.clone());
                    for p in self.files.keys() {
                        if (p.ends_with("/go.mod") || p.ends_with(".go"))
                            && let Some(parent) = Path::new(p).parent()
                            && !parent.as_os_str().is_empty()
                        {
                            search_roots.insert(workspace.root.join(parent));
                        }
                    }
                    for start_dir in search_roots {
                        let mut cur: Option<&Path> = Some(&start_dir);
                        while let Some(dir) = cur {
                            let cand = dir.join("go.work");
                            if cand.is_file() {
                                gowork_paths.insert(cand);
                                break;
                            }
                            cur = dir.parent();
                        }
                    }
                }
                let is_explicit_gowork =
                    gowork_trimmed.is_some_and(|v| !v.is_empty() && v != "auto");
                let gw_use_key = if is_explicit_gowork {
                    "__gowork_explicit_use__"
                } else {
                    "__gowork_use__"
                };
                let gw_repl_pfx = if is_explicit_gowork {
                    "__gowork_explicit__:"
                } else {
                    "__gowork__:"
                };
                for gw_path in gowork_paths {
                    if !gw_path.is_file() {
                        continue;
                    }
                    let already_indexed = gw_path
                        .strip_prefix(&workspace.root)
                        .ok()
                        .map(normalize_rel_str)
                        .is_some_and(|rel| self.files.contains_key(&rel));
                    if !already_indexed || is_explicit_gowork {
                        if let Ok(text) = std::fs::read_to_string(&gw_path) {
                            let gw_dir = gw_path.parent().unwrap_or(&workspace.root);
                            let gw_scope = if is_explicit_gowork {
                                String::new()
                            } else {
                                gw_dir
                                    .strip_prefix(&workspace.root)
                                    .ok()
                                    .map(normalize_rel_str)
                                    .unwrap_or_default()
                            };
                            let parsed_gw = parse_manifest_metadata(&text, "go.work");
                            for sym in &parsed_gw.defined_symbols {
                                if let Some(use_target) = sym.strip_prefix("go_work_use:") {
                                    if let Some(resolved_rel) = resolve_go_local_dir_no_symlink(
                                        &workspace.root,
                                        gw_dir,
                                        use_target,
                                    ) && !go_dir_has_ignored_sources(&resolved_rel)
                                    {
                                        self.resolved_go_replaces_by_mod
                                            .entry(gw_scope.clone())
                                            .or_default()
                                            .push((gw_use_key.to_string(), resolved_rel));
                                    } else {
                                        self.discovery_budget_exceeded = true;
                                    }
                                } else if let Some(repl) = sym.strip_prefix("go_replace:")
                                    && let Some((old_mod, target)) = repl.split_once('=')
                                {
                                    if let Some(resolved_rel) = resolve_go_local_dir_no_symlink(
                                        &workspace.root,
                                        gw_dir,
                                        target,
                                    ) && !go_dir_has_ignored_sources(&resolved_rel)
                                    {
                                        if !old_mod.is_empty() {
                                            self.resolved_go_replaces_by_mod
                                                .entry(gw_scope.clone())
                                                .or_default()
                                                .push((format!("{gw_repl_pfx}{old_mod}"), resolved_rel));
                                        }
                                    } else {
                                        self.discovery_budget_exceeded = true;
                                    }
                                }
                            }
                        } else {
                            self.discovery_budget_exceeded = true;
                        }
                    }
                }
            }
            // Verify EVERY Go import path in indexed `.go` files against known local module
            // roots (main `go.mod`, `replace`, `go.work use`) AND `vendor/<imp>` on disk so
            // gitignored/excluded local packages (including packages under `target/`,
            // `node_modules/`, `.venv/`, or `vendor/`) never produce a silent false-green pass!
            let mut local_go_mod_prefixes: Vec<(String, String)> = Vec::new();
            for (path, node) in &self.files {
                if path == "go.mod" || path.ends_with("/go.mod") {
                    let mdir = Path::new(path)
                        .parent()
                        .map(normalize_rel_str)
                        .unwrap_or_default();
                    for sym in &node.parsed.defined_symbols {
                        if let Some(mname) = sym.strip_prefix("go_mod:") {
                            local_go_mod_prefixes.push((mname.to_string(), mdir.clone()));
                        }
                    }
                }
            }
            for entries in self.resolved_go_replaces_by_mod.values() {
                for (raw_key, target_rel) in entries {
                    let mod_key = raw_key
                        .strip_prefix("__gowork__:")
                        .or_else(|| raw_key.strip_prefix("__gowork_explicit__:"))
                        .unwrap_or(raw_key.as_str());
                    if mod_key != "__gowork_use__" && mod_key != "__gowork_explicit_use__" && !mod_key.is_empty() {
                        local_go_mod_prefixes.push((mod_key.to_string(), target_rel.clone()));
                    } else if !target_rel.is_empty() {
                        let gm_rel = format!("{target_rel}/go.mod");
                        if let Some(gm_node) = self.files.get(&gm_rel) {
                            for sym in &gm_node.parsed.defined_symbols {
                                if let Some(mname) = sym.strip_prefix("go_mod:") {
                                    local_go_mod_prefixes.push((mname.to_string(), target_rel.clone()));
                                }
                            }
                        }
                    }
                }
            }
            let mut checked_go_pkg_dirs: BTreeSet<String> = BTreeSet::new();
            let check_go_pkg_dir_ignored = |pkg_rel: &str,
                                            files: &BTreeMap<String, FileNode>,
                                            ws: &Workspace|
             -> bool {
                if !pkg_rel.is_empty()
                    && resolve_go_local_dir_no_symlink(&ws.root, &ws.root, pkg_rel).is_none()
                    && ws.root.join(pkg_rel).exists()
                {
                    return true;
                }
                let pkg_abs = if pkg_rel.is_empty() {
                    ws.root.clone()
                } else {
                    ws.root.join(pkg_rel)
                };
                let Ok(meta) = std::fs::symlink_metadata(&pkg_abs) else {
                    return false;
                };
                if meta.file_type().is_symlink() {
                    return true;
                }
                if !meta.is_dir() {
                    return false;
                }
                let Ok(rd) = std::fs::read_dir(&pkg_abs) else {
                    return true;
                };
                for ent in rd.flatten() {
                    let fname = ent.file_name();
                    let Some(fname_s) = fname.to_str() else {
                        continue;
                    };
                    let child_rel = if pkg_rel.is_empty() {
                        fname_s.to_string()
                    } else {
                        format!("{pkg_rel}/{fname_s}")
                    };
                    let child_abs = ent.path();
                    let Ok(cmeta) = std::fs::symlink_metadata(&child_abs) else {
                        return true;
                    };
                    if cmeta.file_type().is_symlink() {
                        if !cmeta.is_dir()
                            || child_rel.ends_with(".go")
                            || is_cgo_native_source_path(&child_rel)
                        {
                            return true;
                        }
                        if child_abs.is_file()
                            && (child_rel.ends_with(".go")
                                || is_cgo_native_source_path(&child_rel)
                                || child_rel.ends_with(".syso")
                                || child_rel.ends_with(".swig")
                                || child_rel.ends_with(".swigcxx"))
                        {
                            return true;
                        }
                    }
                    if !cmeta.is_dir()
                        && (child_rel.ends_with(".go")
                            || fname_s == "go.mod"
                            || fname_s == "go.work"
                            || is_cgo_native_source_path(&child_rel)
                            || child_rel.ends_with(".syso")
                            || child_rel.ends_with(".swig")
                            || child_rel.ends_with(".swigcxx"))
                        && (ws.is_excluded_or_ignored_rel(Path::new(&child_rel))
                            || !files.contains_key(&child_rel))
                    {
                        return true;
                    }
                }
                false
            };
            'go_imp_check: for (path, node) in &self.files {
                if !path.ends_with(".go") {
                    continue;
                }
                let file_dir = Path::new(path)
                    .parent()
                    .map(normalize_rel_str)
                    .unwrap_or_default();
                // Check EVERY indexed Go package's OWN directory AND every ancestor directory up
                // to the workspace root `""` for ignored/unindexed `go.mod` / `go.work` manifests
                // (so an ignored root `go.mod` or nested `sub/go.work` above `sub/pkg/` is NEVER
                // missed!) as well as ignored/symlinked sibling `.go`/cgo files in `file_dir`.
                if checked_go_pkg_dirs.insert(file_dir.clone())
                    && check_go_pkg_dir_ignored(&file_dir, &self.files, workspace)
                {
                    self.discovery_budget_exceeded = true;
                    break 'go_imp_check;
                }
                let mut anc_dir = file_dir.as_str();
                let mut found_in_root_go_mod = false;
                loop {
                    for mf_name in ["go.mod", "go.work"] {
                        let mf_rel = if anc_dir.is_empty() {
                            mf_name.to_string()
                        } else {
                            format!("{anc_dir}/{mf_name}")
                        };
                        let mf_abs = workspace.root.join(&mf_rel);
                        if let Ok(mmeta) = std::fs::symlink_metadata(&mf_abs) {
                            if mf_name == "go.mod" {
                                found_in_root_go_mod = true;
                            }
                            if mmeta.file_type().is_symlink()
                                || workspace.is_excluded_or_ignored_rel(Path::new(&mf_rel))
                                || !self.files.contains_key(&mf_rel)
                            {
                                self.discovery_budget_exceeded = true;
                                break 'go_imp_check;
                            }
                        }
                    }
                    if anc_dir.is_empty() {
                        break;
                    }
                    anc_dir = anc_dir.rsplit_once('/').map_or("", |(p, _)| p);
                }
                // If no `go.mod` exists inside `workspace.root` for this Go file, check parent
                // directories ABOVE `workspace.root`: a parent `/repo/go.mod` outside registered
                // `/repo/sub` makes inter-package imports inside `sub` unresolvable -> fail closed!
                if !found_in_root_go_mod {
                    let mut ext_parent = workspace.root.parent();
                    while let Some(ep) = ext_parent {
                        if ep.join("go.mod").is_file() {
                            self.discovery_budget_exceeded = true;
                            break 'go_imp_check;
                        }
                        ext_parent = ep.parent();
                    }
                }
                for imp in &node.parsed.imports {
                    if imp.module == "C" || imp.module.is_empty() {
                        continue;
                    }
                    for (mname, mdir) in &local_go_mod_prefixes {
                        let sub = if imp.module == *mname {
                            Some("")
                        } else {
                            imp.module.strip_prefix(&format!("{mname}/"))
                        };
                        if let Some(sub_rel) = sub {
                            let pkg_rel = if mdir.is_empty() {
                                sub_rel.to_string()
                            } else if sub_rel.is_empty() {
                                mdir.clone()
                            } else {
                                format!("{mdir}/{sub_rel}")
                            };
                            if checked_go_pkg_dirs.insert(pkg_rel.clone())
                                && check_go_pkg_dir_ignored(&pkg_rel, &self.files, workspace)
                            {
                                self.discovery_budget_exceeded = true;
                                break 'go_imp_check;
                            }
                        }
                    }
                    // Also detect gitignored `vendor/<imp>` directories reachable by this Go file.
                    let mut cur_anc = file_dir.as_str();
                    loop {
                        let vrel = if cur_anc.is_empty() {
                            format!("vendor/{}", imp.module)
                        } else {
                            format!("{cur_anc}/vendor/{}", imp.module)
                        };
                        if checked_go_pkg_dirs.insert(vrel.clone())
                            && check_go_pkg_dir_ignored(&vrel, &self.files, workspace)
                        {
                            self.discovery_budget_exceeded = true;
                            break 'go_imp_check;
                        }
                        if cur_anc.is_empty() {
                            break;
                        }
                        cur_anc = cur_anc.rsplit_once('/').map_or("", |(p, _)| p);
                    }
                }
            }
        }
        // Only seed cgo header discovery from sibling native C/C++/ObjC/Fortran sources when
        // their directory is an actual cgo Go package so unrelated standalone C files with
        // `#include HEADER` macros never disable caching for Python/Rust/Frontend workspaces.
        if !cgo_pkg_dirs.is_empty() {
            for (path, node) in &self.files {
                if is_cgo_native_source_path(path) {
                    let dir = Path::new(path)
                        .parent()
                        .map(normalize_rel_str)
                        .unwrap_or_default();
                    if cgo_pkg_dirs.contains(&dir) {
                        for sym in &node.parsed.defined_symbols {
                            if let Some(hdr) = sym.strip_prefix("c_include_opt:") {
                                cgo_hdr_worklist.push_back((
                                    dir.clone(),
                                    dir.clone(),
                                    hdr.to_string(),
                                    true,
                                ));
                            } else if let Some(hdr) = sym.strip_prefix("c_include_sys:") {
                                cgo_hdr_worklist.push_back((
                                    dir.clone(),
                                    dir.clone(),
                                    hdr.to_string(),
                                    false,
                                ));
                            } else if let Some(hdr) = sym.strip_prefix("c_include:") {
                                cgo_hdr_worklist.push_back((
                                    dir.clone(),
                                    dir.clone(),
                                    hdr.to_string(),
                                    false,
                                ));
                            }
                        }
                    }
                }
            }
        }
        self.resolved_cgo_inc_by_pkg = cgo_inc_by_pkg.clone();

        // Expand all grouped Go `//go:embed` directives on disk once per unique package directory.
        // Record EVERY matching directory (including currently empty subdirectories like
        // `assets/nested/`) in `go_embed_watch_dirs` so new files dropped there are watched,
        // and apply Go's embed traversal rules (skipping only VCS dirs `.git`/`.hg`/`.svn` unless
        // explicitly named, never skipping valid `assets/target/...` or `dist/...` subtrees).
        self.go_embed_watch_dirs.clear();
        let mut go_embed_candidates = BTreeSet::new();
        for (parent_prefix, raw_pats) in go_embeds_by_dir {
            let compiled_pats: Vec<CompiledGoEmbed> = raw_pats
                .iter()
                .filter_map(|p| CompiledGoEmbed::new(p))
                .collect();
            if compiled_pats.is_empty() {
                continue;
            }
            let base_dir = if parent_prefix.is_empty() {
                workspace.root.clone()
            } else {
                workspace.root.join(&parent_prefix)
            };
            if !parent_prefix.is_empty() {
                self.go_embed_watch_dirs.insert(parent_prefix.clone());
            }
            let mut walk_dirs: Vec<std::path::PathBuf> = vec![base_dir.clone()];
            let mut scanned_entries = 0usize;
            while let Some(dir) = walk_dirs.pop() {
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(_) => {
                        if std::fs::symlink_metadata(&dir).is_ok() {
                            self.discovery_budget_exceeded = true;
                        }
                        continue;
                    }
                };
                for entry_res in entries {
                    let Ok(entry) = entry_res else {
                        self.discovery_budget_exceeded = true;
                        continue;
                    };
                    scanned_entries += 1;
                    if scanned_entries > policy.max_files.max(4_096) {
                        self.discovery_budget_exceeded = true;
                        walk_dirs.clear();
                        break;
                    }
                    let Ok(ft) = entry.file_type() else {
                        self.discovery_budget_exceeded = true;
                        continue;
                    };
                    if ft.is_symlink() {
                        continue;
                    }
                    let p = entry.path();
                    let Ok(rel_from_base) = p.strip_prefix(&base_dir) else {
                        continue;
                    };
                    let rel_under = normalize_rel_str(rel_from_base);
                    let rel_from_root = if parent_prefix.is_empty() {
                        rel_under.clone()
                    } else {
                        format!("{parent_prefix}/{rel_under}")
                    };
                    if ft.is_dir() {
                        let dname = entry.file_name();
                        let dname_str = dname.to_str().unwrap_or("");
                        if matches!(dname_str, ".git" | ".hg" | ".svn")
                            && !compiled_pats.iter().any(|cp| cp.explicitly_targets_dir(dname_str))
                        {
                            continue;
                        }
                        if compiled_pats.iter().any(|cp| cp.dir_could_match(&rel_under)) {
                            self.go_embed_watch_dirs.insert(rel_from_root);
                            walk_dirs.push(p);
                        }
                    } else if ft.is_file()
                        && compiled_pats.iter().any(|cp| cp.matches_rel(&rel_under))
                    {
                        go_embed_candidates.insert(rel_from_root);
                    }
                }
            }
        }
        for rel_str in go_embed_candidates {
            let rel_path = Path::new(&rel_str);
            // Check explicit workspace exclude / `.gitignore` rules BEFORE checking `self.files`
            // so even previously-indexed files that became gitignored fail closed cleanly.
            if workspace.is_excluded_or_ignored_rel(rel_path) {
                self.discovery_budget_exceeded = true;
                self.files.remove(&rel_str);
                continue;
            }
            if self.files.contains_key(&rel_str) {
                continue;
            }
            let full_path = workspace.root.join(rel_path);
            if let Some(stamp) = file_stamp(&full_path) {
                let fsize = usize::try_from(stamp.size).unwrap_or(usize::MAX);
                if self.files.len() >= policy.max_files
                    || fsize > policy.max_file_bytes
                    || total_source_bytes.saturating_add(fsize) > policy.max_source_bytes
                {
                    self.discovery_budget_exceeded = true;
                    continue;
                }
                if let Some(cached_node) = persisted.get(&rel_str)
                    && cached_node.stamp == Some(stamp)
                {
                    if cached_node.is_config {
                        self.config_hashes
                            .insert(cached_node.path.clone(), cached_node.sha256.clone());
                    }
                    self.files.insert(rel_str, cached_node.clone());
                    total_source_bytes = total_source_bytes.saturating_add(fsize);
                    continue;
                }
                match workspace.read_known_with_limit(rel_path, policy.max_file_bytes) {
                    Ok(bytes) => {
                        let byte_len = bytes.len();
                        if total_source_bytes.saturating_add(byte_len) > policy.max_source_bytes {
                            self.discovery_budget_exceeded = true;
                            continue;
                        }
                        self.insert_file_bytes_with_stamp(&rel_str, &bytes, Some(stamp));
                        total_source_bytes = total_source_bytes.saturating_add(byte_len);
                        total_fresh += 1;
                    }
                    Err(_) => {
                        self.discovery_budget_exceeded = true;
                    }
                }
            } else if std::fs::symlink_metadata(&full_path).is_ok() {
                self.discovery_budget_exceeded = true;
            }
        }

        // Watch every resolved `#cgo CFLAGS: -I...` directory (including workspace root `""` for
        // `-I${SRCDIR}/..`), its existing nested subdirectories (tracking traversal in a separate
        // `inc_visited` set so previously-watched embed dirs never cut off deeper `-I` descent),
        // AND existing ancestors of missing `-I` dirs.
        let mut inc_visited: BTreeSet<String> = BTreeSet::new();
        for inc_dirs in cgo_inc_by_pkg.values() {
            for inc_dir in inc_dirs {
                let abs_inc = if inc_dir.is_empty() {
                    workspace.root.clone()
                } else {
                    workspace.root.join(inc_dir)
                };
                if abs_inc.is_dir() {
                    if !inc_dir.is_empty() {
                        self.go_embed_watch_dirs.insert(inc_dir.clone());
                    }
                    if !inc_visited.insert(inc_dir.clone()) {
                        continue;
                    }
                    let is_root_inc = inc_dir.is_empty();
                    let mut inc_stack = vec![abs_inc];
                    let mut inc_dirs_found = 0usize;
                    while let Some(d) = inc_stack.pop() {
                        let Ok(entries) = std::fs::read_dir(&d) else {
                            continue;
                        };
                        for entry in entries.flatten() {
                            let Ok(ft) = entry.file_type() else {
                                continue;
                            };
                            if ft.is_symlink() || !ft.is_dir() {
                                continue;
                            }
                            let p = entry.path();
                            let dname = entry.file_name();
                            let dname_str = dname.to_str().unwrap_or("");
                            if matches!(dname_str, ".git" | ".hg" | ".svn")
                                || (is_root_inc
                                    && matches!(
                                        dname_str,
                                        "node_modules" | ".venv" | "venv" | "__pycache__" | "target"
                                    ))
                            {
                                continue;
                            }
                            inc_dirs_found += 1;
                            if inc_dirs_found > policy.max_watch_dirs.min(2_048) {
                                self.discovery_budget_exceeded = true;
                                inc_stack.clear();
                                break;
                            }
                            if let Ok(rel) = p.strip_prefix(&workspace.root) {
                                let rel_s = normalize_rel_str(rel);
                                if !rel_s.is_empty() {
                                    self.go_embed_watch_dirs.insert(rel_s.clone());
                                    if inc_visited.insert(rel_s) {
                                        inc_stack.push(p);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Missing `-I` directory: watch its nearest existing ancestor inside workspace.
                    let mut cur = abs_inc.parent();
                    while let Some(anc) = cur {
                        if !anc.starts_with(&workspace.root) {
                            break;
                        }
                        if anc.is_dir() {
                            if let Ok(rel) = anc.strip_prefix(&workspace.root) {
                                let rel_s = normalize_rel_str(rel);
                                if !rel_s.is_empty() {
                                    self.go_embed_watch_dirs.insert(rel_s);
                                }
                            }
                            break;
                        }
                        if anc == workspace.root {
                            break;
                        }
                        cur = anc.parent();
                    }
                }
            }
        }
        self.resolved_abs_c_includes.clear();
        self.missing_c_include_candidates.clear();
        self.missing_data_ref_candidates.clear();
        // Also discover cgo `#include` / `-include` / `__has_include` headers (including generated
        // headers in excluded/gitignored dirs like `-I${SRCDIR}/build/include`, nonstandard
        // extensions like `.inc` or extensionless headers, and their transitive `#include`s).
        // Deduplicate by `(orig_pkg_dir, norm_hdr)` and preserve BOTH `orig_pkg_dir` and `cur_dir`.
        let mut seen_cgo_hdrs: BTreeSet<(String, String)> = BTreeSet::new();
        while let Some((orig_pkg_dir, cur_dir, hdr_mod, is_optional)) = cgo_hdr_worklist.pop_front()
        {
            if hdr_mod == DYNAMIC_MACRO_INCLUDE_SENTINEL {
                self.discovery_budget_exceeded = true;
                continue;
            }
            let mut cands: Vec<std::path::PathBuf> = Vec::new();
            let hdr_p = Path::new(&hdr_mod);
            if hdr_p.is_absolute() {
                if let Ok(rel) = hdr_p.strip_prefix(&workspace.root) {
                    let norm_abs = normalize_rel_str(rel);
                    if !norm_abs.is_empty() {
                        self.resolved_abs_c_includes
                            .insert(hdr_mod.clone(), norm_abs.clone());
                        cands.push(rel.to_path_buf());
                    }
                } else if let Ok(canon_h) = std::fs::canonicalize(hdr_p)
                    && let Ok(rel) = canon_h.strip_prefix(&canon_ws_root)
                {
                    let norm_abs = normalize_rel_str(rel);
                    if !norm_abs.is_empty() {
                        self.resolved_abs_c_includes
                            .insert(hdr_mod.clone(), norm_abs.clone());
                        cands.push(rel.to_path_buf());
                    }
                } else {
                    self.discovery_budget_exceeded = true;
                }
            } else {
                cands.push(Path::new(&cur_dir).join(&hdr_mod));
                if cur_dir != orig_pkg_dir {
                    cands.push(Path::new(&orig_pkg_dir).join(&hdr_mod));
                }
                if let Some(inc_dirs) = cgo_inc_by_pkg.get(&orig_pkg_dir) {
                    for inc_dir in inc_dirs {
                        cands.push(Path::new(inc_dir).join(&hdr_mod));
                    }
                }
            }
            let mut resolved_in_workspace = false;
            for cand in cands {
                let Some(norm_hdr) = normalize_lexical_rel(&cand) else {
                    if hdr_mod.starts_with("../") || hdr_mod.contains("/../") {
                        self.discovery_budget_exceeded = true;
                    }
                    continue;
                };
                if !seen_cgo_hdrs.insert((orig_pkg_dir.clone(), norm_hdr.clone())) {
                    if self.files.contains_key(&norm_hdr) {
                        resolved_in_workspace = true;
                    }
                    continue;
                }
                let full_hdr = workspace.root.join(&norm_hdr);
                if let Ok(meta) = std::fs::symlink_metadata(&full_hdr)
                    && meta.file_type().is_symlink()
                {
                    self.discovery_budget_exceeded = true;
                    continue;
                }
                if !self.files.contains_key(&norm_hdr)
                    && let Some(stamp) = file_stamp(&full_hdr)
                {
                    let fsize = usize::try_from(stamp.size).unwrap_or(usize::MAX);
                    if self.files.len() >= policy.max_files
                        || fsize > policy.max_file_bytes
                        || total_source_bytes.saturating_add(fsize) > policy.max_source_bytes
                    {
                        self.discovery_budget_exceeded = true;
                        continue;
                    }
                    if let Ok(bytes) =
                        workspace.read_known_with_limit(Path::new(&norm_hdr), policy.max_file_bytes)
                    {
                        let byte_len = bytes.len();
                        if total_source_bytes.saturating_add(byte_len) > policy.max_source_bytes {
                            self.discovery_budget_exceeded = true;
                            continue;
                        }
                        self.insert_file_bytes_with_stamp(&norm_hdr, &bytes, Some(stamp));
                        total_source_bytes = total_source_bytes.saturating_add(byte_len);
                        total_fresh += 1;
                    }
                }
                if let Some(hdr_node) = self.files.get(&norm_hdr) {
                    resolved_in_workspace = true;
                    if let Some(hdr_parent) = Path::new(&norm_hdr).parent() {
                        let hp = normalize_rel_str(hdr_parent);
                        if !hp.is_empty() {
                            self.go_embed_watch_dirs.insert(hp);
                        }
                    }
                    let nested_imps: Vec<(String, bool)> = hdr_node
                        .parsed
                        .defined_symbols
                        .iter()
                        .filter_map(|s| {
                            if let Some(h) = s.strip_prefix("c_include_opt:") {
                                Some((h.to_string(), true))
                            } else if let Some(h) = s.strip_prefix("c_include_sys:") {
                                Some((h.to_string(), false))
                            } else {
                                s.strip_prefix("c_include:")
                                    .map(|h| (h.to_string(), false))
                            }
                        })
                        .collect();
                    let hdr_dir = Path::new(&norm_hdr)
                        .parent()
                        .map(normalize_rel_str)
                        .unwrap_or_default();
                    for (nested, nested_opt) in nested_imps {
                        // Preserve `orig_pkg_dir` (for its `-I` search paths) AND `hdr_dir`
                        // (for sibling headers relative to the included header).
                        cgo_hdr_worklist.push_back((
                            orig_pkg_dir.clone(),
                            hdr_dir.clone(),
                            nested,
                            nested_opt,
                        ));
                    }
                } else {
                    // Header candidate does not exist yet (e.g. `__has_include("build/feature.h")`):
                    // record it in `missing_c_include_candidates` and watch its nearest existing
                    // ancestor directory inside workspace so creating it later emits a watcher event.
                    self.missing_c_include_candidates.insert(norm_hdr.clone());
                    let mut cur = full_hdr.parent();
                    while let Some(anc) = cur {
                        if !anc.starts_with(&workspace.root) {
                            break;
                        }
                        if anc.is_dir() {
                            if let Ok(rel) = anc.strip_prefix(&workspace.root) {
                                let rel_s = normalize_rel_str(rel);
                                if !rel_s.is_empty() {
                                    self.go_embed_watch_dirs.insert(rel_s);
                                }
                            }
                            break;
                        }
                        if anc == workspace.root {
                            break;
                        }
                        cur = anc.parent();
                    }
                }
            }
            // Any unresolved `#include` OR `__has_include(...)` (quoted or angle-bracket) that
            // did NOT resolve inside the workspace and is NOT a curated standard C/POSIX system
            // header could resolve or newly appear in `/usr/local/include` or external vendor
            // include search paths; fail closed.
            let _ = is_optional;
            if !resolved_in_workspace && !is_standard_c_system_header(&hdr_mod) {
                self.discovery_budget_exceeded = true;
            }
        }

        // True fixed-point worklist traversal bounded by workspace `max_files` and `max_source_bytes`.
        while let Some(src_path) = worklist.pop_front() {
            let Some(node) = self.files.get(&src_path) else {
                continue;
            };
            if node.parsed.data_refs.is_empty() {
                continue;
            }
            let data_refs = node.parsed.data_refs.clone();
            let parent = Path::new(&src_path)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let rs_crate_dirs = if src_path.ends_with(".rs") {
                owning_rust_crate_dirs(&src_path, &self.files)
            } else {
                Vec::new()
            };
            let fe_pkg_dirs: Vec<String> = if is_frontend_source_path(&src_path) {
                let mut dirs = Vec::new();
                let mut cur = Path::new(&src_path).parent();
                while let Some(d) = cur {
                    let ds = normalize_rel_str(d);
                    if !ds.is_empty() && workspace.root.join(&ds).join("package.json").is_file() {
                        dirs.push(ds);
                    }
                    cur = d.parent();
                }
                dirs
            } else {
                Vec::new()
            };
            let cwd_raw_fs_refs: BTreeSet<&str> = node
                .parsed
                .defined_symbols
                .iter()
                .filter_map(|s| {
                    s.strip_prefix("rust_raw_fs:")
                        .or_else(|| s.strip_prefix("fe_raw_fs:"))
                        .or_else(|| s.strip_prefix("py_raw_fs:"))
                })
                .collect();
            let url_raw_fs_refs: BTreeSet<&str> = node
                .parsed
                .defined_symbols
                .iter()
                .filter_map(|s| s.strip_prefix("fe_raw_url_fs:"))
                .collect();
            let is_rs_source = src_path.ends_with(".rs");
            let mut candidates = BTreeSet::new();
            let mut missing_candidates = BTreeSet::new();
            for data_ref in &data_refs {
                let mut ref_budget_exceeded = false;
                let mut consider_ref = |cand_rel: String,
                                        candidates: &mut BTreeSet<String>,
                                        missing_candidates: &mut BTreeSet<String>| {
                    let p = Path::new(&cand_rel);
                    let full = workspace.root.join(p);
                    if let Ok(sym_meta) = std::fs::symlink_metadata(&full)
                        && sym_meta.file_type().is_symlink()
                    {
                        // Explicitly referenced symlink (to an in- or out-of-workspace fixture, or
                        // even a dangling symlink): `file_stamp` refuses symlinks, so fail closed
                        // rather than silently dropping the dependency!
                        ref_budget_exceeded = true;
                        return;
                    }
                    if !full.exists() {
                        // Record absent referenced fixture paths so creating them later (e.g.
                        // `existsSync("fixtures/ready.bin")`) is admitted by the watcher and
                        // invalidates cached test results. If the absent target is also ignored
                        // or excluded (e.g. `existsSync("generated/ready.bin")`), fail closed
                        // immediately since excluded directories may not be watched.
                        if workspace.is_excluded_or_ignored_rel(p) {
                            ref_budget_exceeded = true;
                        }
                        missing_candidates.insert(cand_rel);
                        return;
                    }
                    if workspace.is_excluded_or_ignored_rel(p) {
                        // Explicit reference points to an existing excluded/gitignored file (e.g.
                        // `cargo::rerun-if-changed=generated.json`, `include_bytes!("ignored.bin")`,
                        // or Python/Frontend `open("fixtures/expected.json")`): keep the exclusion
                        // AND fail closed so edits to the ignored fixture never hit stale cache!
                        if is_rs_source || full.is_file() {
                            ref_budget_exceeded = true;
                        }
                        return;
                    }
                    // Permit existing directories (`cargo::rerun-if-changed=assets`) AND explicit
                    // file reads (`include_bytes!("model.bin")`, `readFileSync("fixtures/input.bin")`,
                    // `os.ReadFile("testdata/sample.bin")`) regardless of extension globs, while
                    // still strictly honoring workspace excludes and size/file budgets.
                    if workspace.is_permitted_data_rel(p)
                        || workspace.is_permitted_rel(p)
                        || full.is_dir()
                        || full.is_file()
                    {
                        candidates.insert(cand_rel);
                    }
                };
                // Check EVERY viable runtime base (workspace root, source parent, owning Rust
                // crate dirs, and owning Frontend package dirs) independently — BOTH for in-repo
                // discovery AND for escaping `..` references outside the workspace root.
                // For cwd-relative runtime I/O (`rust_raw_fs:` / `fe_raw_fs:`), escaping ANY
                // runtime working directory fails closed even while absent (since external file
                // creation cannot be watched); for source-relative refs (e.g. `include_str!("../a")`),
                // fail closed if it escapes all bases or points to an existing external path.
                let raw_path_traverses_symlink = |base_abs: &Path, rel_s: &str| -> bool {
                    let mut cur = base_abs.to_path_buf();
                    for comp in Path::new(rel_s).components() {
                        match comp {
                            std::path::Component::CurDir => {}
                            std::path::Component::ParentDir => {
                                // Traversing `missing/..` fails on disk if `missing` doesn't exist
                                // yet, and creating `missing/` later would make the read succeed
                                // without modifying the normalized target: fail closed when any
                                // intermediate directory before `..` is absent!
                                if !cur.is_dir() {
                                    return true;
                                }
                                cur.pop();
                            }
                            std::path::Component::Normal(c) => {
                                cur.push(c);
                                if std::fs::symlink_metadata(&cur)
                                    .is_ok_and(|m| m.file_type().is_symlink())
                                {
                                    return true;
                                }
                            }
                            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                                return true;
                            }
                        }
                    }
                    false
                };
                let is_cwd_runtime_io = cwd_raw_fs_refs.contains(data_ref.as_str());
                let is_url_source_only =
                    url_raw_fs_refs.contains(data_ref.as_str()) && !is_cwd_runtime_io;
                let mut escaped_ref_exists = false;
                let mut any_base_inside = false;
                if is_url_source_only {
                    // `fe_raw_url_fs:` comes from WHATWG `new URL(..., import.meta.url)` and
                    // resolves EXCLUSIVELY relative to the source file's containing directory.
                    if raw_path_traverses_symlink(&workspace.root.join(parent), data_ref) {
                        escaped_ref_exists = true;
                    } else if let Some(norm_rel) = normalize_lexical_rel(&parent.join(data_ref)) {
                        any_base_inside = true;
                        consider_ref(norm_rel, &mut candidates, &mut missing_candidates);
                    } else {
                        escaped_ref_exists = true;
                    }
                } else {
                    if raw_path_traverses_symlink(&workspace.root, data_ref) {
                        escaped_ref_exists = true;
                    } else if let Some(norm_root) = normalize_lexical_rel(Path::new(data_ref)) {
                        any_base_inside = true;
                        consider_ref(norm_root, &mut candidates, &mut missing_candidates);
                    } else if is_cwd_runtime_io
                        || workspace.root.join(data_ref).exists()
                        || std::fs::symlink_metadata(workspace.root.join(data_ref)).is_ok()
                    {
                        escaped_ref_exists = true;
                    }
                    if !parent.as_os_str().is_empty() {
                        if raw_path_traverses_symlink(&workspace.root.join(parent), data_ref) {
                            escaped_ref_exists = true;
                        } else if let Some(norm_rel) = normalize_lexical_rel(&parent.join(data_ref))
                        {
                            any_base_inside = true;
                            consider_ref(norm_rel, &mut candidates, &mut missing_candidates);
                        } else if workspace.root.join(parent).join(data_ref).exists()
                            || std::fs::symlink_metadata(workspace.root.join(parent).join(data_ref))
                                .is_ok()
                        {
                            escaped_ref_exists = true;
                        }
                    }
                    for base_dir in rs_crate_dirs.iter().chain(fe_pkg_dirs.iter()) {
                        if !base_dir.is_empty() {
                            if raw_path_traverses_symlink(&workspace.root.join(base_dir), data_ref)
                            {
                                escaped_ref_exists = true;
                            } else if let Some(norm_base) =
                                normalize_lexical_rel(&Path::new(base_dir).join(data_ref))
                            {
                                any_base_inside = true;
                                consider_ref(norm_base, &mut candidates, &mut missing_candidates);
                            } else if is_cwd_runtime_io
                                || workspace.root.join(base_dir).join(data_ref).exists()
                                || std::fs::symlink_metadata(
                                    workspace.root.join(base_dir).join(data_ref),
                                )
                                .is_ok()
                            {
                                escaped_ref_exists = true;
                            }
                        }
                    }
                }
                if !any_base_inside {
                    escaped_ref_exists = true;
                }
                drop(consider_ref);
                if ref_budget_exceeded || escaped_ref_exists {
                    self.discovery_budget_exceeded = true;
                }
                // Also resolve workspace package-based tsconfig `extends` (e.g. `@acme/config`,
                // `@acme/config/base`, respecting package.json `tsconfig` and exact/wildcard `exports`).
                for (pkg_name, pkg_dir) in &pkg_to_dir {
                    if data_ref != pkg_name && !data_ref.starts_with(&format!("{pkg_name}/")) {
                        continue;
                    }
                    let pkg_json_path = if pkg_dir.is_empty() {
                        "package.json".to_string()
                    } else {
                        format!("{pkg_dir}/package.json")
                    };
                    let mut sub_cands: Vec<String> = Vec::new();
                    if data_ref == pkg_name {
                        if let Some(pkg_node) = self.files.get(&pkg_json_path) {
                            for sym in &pkg_node.parsed.defined_symbols {
                                if let Some(ts_entry) = sym.strip_prefix("fe_pkg_tsconfig:") {
                                    sub_cands.push(ts_entry.to_string());
                                } else if let Some(exp_rest) =
                                    sym.strip_prefix(&format!("fe_export:{pkg_name}:.->"))
                                {
                                    sub_cands.push(exp_rest.to_string());
                                } else if let Some(main_rest) =
                                    sym.strip_prefix(&format!("fe_pkg_main:{pkg_name}->"))
                                {
                                    sub_cands.push(main_rest.to_string());
                                }
                            }
                        }
                        sub_cands.push("tsconfig.json".to_string());
                    } else {
                        let raw_sub = data_ref.strip_prefix(&format!("{pkg_name}/")).unwrap_or("");
                        if let Some(pkg_node) = self.files.get(&pkg_json_path) {
                            let pkg_exp_pfx = format!("fe_export:{pkg_name}:");
                            for sym in &pkg_node.parsed.defined_symbols {
                                if let Some(rest) = sym.strip_prefix(&pkg_exp_pfx)
                                    && let Some((exp_key, exp_target)) = rest.split_once("->")
                                    && let Some(mapped) =
                                        match_package_export_subpath(raw_sub, exp_key, exp_target)
                                {
                                    sub_cands.push(mapped);
                                }
                            }
                        }
                        sub_cands.push(raw_sub.to_string());
                    }
                    for sub in sub_cands {
                        let base_cand = if pkg_dir.is_empty() {
                            sub
                        } else {
                            format!("{pkg_dir}/{sub}")
                        };
                        for variant in tsconfig_path_variants(&base_cand) {
                            if let Some(norm_pkg) = normalize_lexical_rel(Path::new(&variant)) {
                                let p = Path::new(&norm_pkg);
                                if workspace.is_permitted_data_rel(p) || workspace.is_permitted_rel(p)
                                {
                                    candidates.insert(norm_pkg);
                                }
                            }
                        }
                    }
                }
            }
            for rel_str in candidates {
                if self.files.contains_key(&rel_str) {
                    continue;
                }
                let rel_path = Path::new(&rel_str);
                let full_path = workspace.root.join(rel_path);
                // Recursively expand referenced directories (e.g. `cargo:rerun-if-changed=assets`)
                // so all descendant files inside the directory become tracked dependencies and
                // watch directories.
                if full_path.is_dir() && !std::fs::symlink_metadata(&full_path).is_ok_and(|m| m.file_type().is_symlink()) {
                    self.go_embed_watch_dirs.insert(rel_str.clone());
                    self.referenced_data_dirs.insert(rel_str.clone());
                    let mut dir_stack = vec![full_path];
                    let mut scanned = 0usize;
                    while let Some(d) = dir_stack.pop() {
                        let Ok(entries) = std::fs::read_dir(&d) else {
                            self.discovery_budget_exceeded = true;
                            continue;
                        };
                        for ent_res in entries {
                            let Ok(ent) = ent_res else {
                                self.discovery_budget_exceeded = true;
                                continue;
                            };
                            scanned += 1;
                            if scanned > policy.max_files.max(4_096) {
                                self.discovery_budget_exceeded = true;
                                dir_stack.clear();
                                break;
                            }
                            let Ok(ft) = ent.file_type() else {
                                continue;
                            };
                            if ft.is_symlink() {
                                // A build script watching `assets` may read a symlinked file/dir
                                // beneath it: fail closed rather than silently skipping symlinks.
                                self.discovery_budget_exceeded = true;
                                continue;
                            }
                            let ep = ent.path();
                            let Ok(erel) = ep.strip_prefix(&workspace.root) else {
                                continue;
                            };
                            if workspace.is_excluded_or_ignored_rel(erel) {
                                // A `build.rs` watching this directory (e.g. `cargo::rerun-if-changed=assets`)
                                // may read gitignored descendants we refuse to index; keep the
                                // exclusion intact AND fail closed so stale Rust results are never cached.
                                self.discovery_budget_exceeded = true;
                                continue;
                            }
                            let erel_str = normalize_rel_str(erel);
                            if ft.is_dir() {
                                self.go_embed_watch_dirs.insert(erel_str);
                                dir_stack.push(ep);
                            } else if ft.is_file()
                                && !self.files.contains_key(&erel_str)
                                && let Some(st) = file_stamp(&ep)
                            {
                                let fsz = usize::try_from(st.size).unwrap_or(usize::MAX);
                                if self.files.len() >= policy.max_files
                                    || fsz > policy.max_file_bytes
                                    || total_source_bytes.saturating_add(fsz)
                                        > policy.max_source_bytes
                                {
                                    self.discovery_budget_exceeded = true;
                                    continue;
                                }
                                if let Ok(bytes) =
                                    workspace.read_known_with_limit(erel, policy.max_file_bytes)
                                {
                                    let blen = bytes.len();
                                    if total_source_bytes.saturating_add(blen)
                                        > policy.max_source_bytes
                                    {
                                        self.discovery_budget_exceeded = true;
                                        continue;
                                    }
                                    self.insert_file_bytes_with_stamp(&erel_str, &bytes, Some(st));
                                    total_source_bytes = total_source_bytes.saturating_add(blen);
                                    total_fresh += 1;
                                } else {
                                    self.discovery_budget_exceeded = true;
                                }
                            }
                        }
                    }
                    continue;
                }
                if let Some(stamp) = file_stamp(&full_path) {
                    let fsize = usize::try_from(stamp.size).unwrap_or(usize::MAX);
                    if self.files.len() >= policy.max_files
                        || fsize > policy.max_file_bytes
                        || total_source_bytes.saturating_add(fsize) > policy.max_source_bytes
                    {
                        // Fail closed: a real referenced fixture/config on disk could not be
                        // indexed within policy budgets, so mark the graph incomplete.
                        self.discovery_budget_exceeded = true;
                        continue;
                    }
                    if let Some(cached_node) = persisted.get(&rel_str)
                        && cached_node.stamp == Some(stamp)
                    {
                        if cached_node.is_config {
                            self.config_hashes
                                .insert(cached_node.path.clone(), cached_node.sha256.clone());
                        }
                        let has_refs = !cached_node.parsed.data_refs.is_empty();
                        self.files.insert(rel_str.clone(), cached_node.clone());
                        total_source_bytes = total_source_bytes.saturating_add(fsize);
                        if has_refs && visited_sources.insert(rel_str.clone()) {
                            worklist.push_back(rel_str);
                        }
                        continue;
                    }
                    if let Ok(bytes) =
                        workspace.read_known_with_limit(rel_path, policy.max_file_bytes)
                    {
                        let byte_len = bytes.len();
                        if total_source_bytes.saturating_add(byte_len) > policy.max_source_bytes {
                            self.discovery_budget_exceeded = true;
                            continue;
                        }
                        self.insert_file_bytes_with_stamp(&rel_str, &bytes, Some(stamp));
                        total_source_bytes = total_source_bytes.saturating_add(byte_len);
                        total_fresh += 1;
                        if self
                            .files
                            .get(&rel_str)
                            .is_some_and(|n| !n.parsed.data_refs.is_empty())
                            && visited_sources.insert(rel_str.clone())
                        {
                            worklist.push_back(rel_str);
                        }
                    }
                } else if full_path.exists()
                    || std::fs::symlink_metadata(&full_path).is_ok()
                {
                    // Referenced candidate exists on disk (e.g. symlink or unreadable special file)
                    // but `file_stamp` refused to stamp it: fail closed!
                    self.discovery_budget_exceeded = true;
                }
            }
            for missing_rel in missing_candidates {
                self.missing_data_ref_candidates.insert(missing_rel.clone());
                let full_missing = workspace.root.join(&missing_rel);
                let mut cur = full_missing.parent();
                while let Some(anc) = cur {
                    if !anc.starts_with(&workspace.root) {
                        break;
                    }
                    if anc.is_dir() {
                        if let Ok(rel) = anc.strip_prefix(&workspace.root) {
                            let rel_s = normalize_rel_str(rel);
                            if !rel_s.is_empty() {
                                self.go_embed_watch_dirs.insert(rel_s);
                            }
                        }
                        break;
                    }
                    if anc == workspace.root {
                        break;
                    }
                    cur = anc.parent();
                }
            }
        }
        // Verify every custom `[package] build = "..."` script declared in an indexed `Cargo.toml`
        // resolved to a tracked file in `self.files`; if excluded, missing, or escaping workspace
        // root, fail closed so stale test results are never served!
        for (path, node) in &self.files {
            if path == "Cargo.toml" || path.ends_with("/Cargo.toml") {
                let cdir = path.strip_suffix("/Cargo.toml").unwrap_or("");
                for sym in &node.parsed.defined_symbols {
                    if let Some(rel_build) = sym.strip_prefix("cargo_build_script:")
                        && rel_build != "false"
                    {
                        let full_rel = if cdir.is_empty() {
                            Path::new(rel_build).to_path_buf()
                        } else {
                            Path::new(cdir).join(rel_build)
                        };
                        let tracked = normalize_lexical_rel(&full_rel)
                            .is_some_and(|norm| self.files.contains_key(&norm));
                        // Non-`.rs` custom build scripts (e.g. `build = "scripts/build.gen"`) are
                        // valid in Cargo but not parsed as Rust for `include_str!` / `rerun-if-changed`;
                        // fail closed whenever a custom build script is untracked OR non-`.rs`.
                        if !tracked || !rel_build.ends_with(".rs") {
                            self.discovery_budget_exceeded = true;
                        }
                    }
                }
            }
        }
        total_fresh
    }

}

