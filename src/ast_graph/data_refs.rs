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
        self.referenced_data_dirs.clear();
        self.required_watch_dirs.clear();
        self.missing_data_ref_candidates.clear();
        let mut total_fresh = 0usize;
        let mut total_source_bytes: usize = self
            .files
            .values()
            .filter_map(|n| n.stamp.map(|s| usize::try_from(s.size).unwrap_or(0)))
            .sum();
        let mut pkg_to_dir: BTreeMap<String, String> = BTreeMap::new();
        let mut worklist: VecDeque<String> = VecDeque::new();
        let mut visited_sources: BTreeSet<String> = BTreeSet::new();
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
            if !node.parsed.data_refs.is_empty() && visited_sources.insert(path.clone()) {
                worklist.push_back(path.clone());
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
            let is_c_cpp_source = is_c_cpp_source_path(&src_path)
                || (!src_path.ends_with(".py")
                    && !src_path.ends_with(".rs")
                    && !src_path.ends_with(".cs")
                    && !is_frontend_source_path(&src_path)
                    && !is_stylesheet_path(&src_path)
                    && !node.parsed.imports.is_empty());
            let c_include_dirs: Vec<String> = if is_c_cpp_source {
                let mut dirs = Vec::new();
                let mut cur: Option<&Path> = Some(parent);
                while let Some(d) = cur {
                    for sub in ["include", "inc", "src"] {
                        let cand = if d.as_os_str().is_empty() {
                            sub.to_string()
                        } else {
                            format!("{}/{sub}", normalize_rel_str(d))
                        };
                        if workspace.root.join(&cand).is_dir() && !dirs.contains(&cand) {
                            dirs.push(cand);
                        }
                    }
                    if d.as_os_str().is_empty() {
                        break;
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
                        // C/C++ `#include <stdio.h>` or absent local headers do not disable caching
                        // globally for unrelated Python/Rust/Frontend tests.
                        if is_c_cpp_source {
                            return;
                        }
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
                        // C/C++ files including an excluded/gitignored header must not poison
                        // global cacheability for unrelated Python/Rust/Frontend tests.
                        if is_c_cpp_source {
                            return;
                        }
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
                    for base_dir in rs_crate_dirs
                        .iter()
                        .chain(fe_pkg_dirs.iter())
                        .chain(c_include_dirs.iter())
                    {
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
                if !is_c_cpp_source && (ref_budget_exceeded || escaped_ref_exists) {
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
                    self.required_watch_dirs.insert(rel_str.clone());
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
                                self.required_watch_dirs.insert(erel_str);
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
                                self.required_watch_dirs.insert(rel_s);
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

