use super::*;

#[derive(Default)]
pub(super) struct BridgeProviderIndex {
    pub(super) providers_by_tag: BTreeMap<String, Vec<String>>,
}

impl BridgeProviderIndex {
    pub(super) fn build(files: &BTreeMap<String, FileNode>) -> Self {
        let mut providers_by_tag: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, node) in files {
            for sym in &node.parsed.defined_symbols {
                if sym.starts_with("api_route:")
                    || sym.starts_with("ctx_def:")
                    || sym.starts_with("wasm:")
                    || sym.starts_with("proto:")
                {
                    providers_by_tag
                        .entry(sym.clone())
                        .or_default()
                        .push(path.clone());
                }
            }
        }
        Self { providers_by_tag }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn compute_direct_dependencies(
    path: &str,
    node: &FileNode,
    files: &BTreeMap<String, FileNode>,
    module_index: &ModuleIndex,
    rust_index: &RustModuleIndex,
    frontend_index: &FrontendModuleIndex,
    fixture_providers: &BTreeMap<String, Vec<String>>,
    bridge_index: &BridgeProviderIndex,
    runtime_deps_for_test: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    if path.ends_with(".rs") {
        deps.extend(rust_index.resolve_file_dependencies(path, node, files));
    } else if is_frontend_source_path(path)
        || is_stylesheet_path(path)
        || (node.is_config && is_js_or_ts_file_ext(path))
    {
        deps.extend(frontend_index.resolve_file_dependencies(path, node, files));
    } else if is_polyglot_source_path(path)
        || (!path.ends_with(".py") && !node.is_config && !node.parsed.imports.is_empty())
    {
        deps.extend(resolve_polyglot_dependencies(path, node, files));
    } else {
        for spec in &node.parsed.imports {
            for resolved in module_index.resolve_import(path, spec, files) {
                if resolved != path {
                    deps.insert(resolved);
                }
            }
            for resolved in rust_index.resolve_pyo3_import(spec, files) {
                if resolved != path {
                    deps.insert(resolved);
                }
            }
        }
        for fixture_name in &node.parsed.fixtures_used {
            if let Some(providers) = fixture_providers.get(fixture_name) {
                for provider in providers {
                    if provider != path && is_provider_applicable(provider, path) {
                        deps.insert(provider.clone());
                    }
                }
            }
        }
    }
    // Cross-language & symbol-level bridges via O(1) BridgeProviderIndex lookup:
    // 1) WebAssembly (`#[wasm_bindgen]` Rust -> Frontend TS/JS)
    // 2) Full-Stack API routes (`api_route:/api/...` -> `api_call:/api/...`)
    // 3) React Context (`ctx_def:<Name>` -> `ctx_use:<Name>`)
    // 4) Protobuf (`.proto` -> generated `_pb2.py`, `.pb.go`, `_pb.ts`, Rust prost modules)
    for sym in &node.parsed.defined_symbols {
        let target_tag = if let Some(api_path) = sym.strip_prefix("api_call:") {
            Some(format!("api_route:{api_path}"))
        } else if let Some(ctx_name) = sym.strip_prefix("ctx_use:") {
            Some(format!("ctx_def:{ctx_name}"))
        } else if let Some(wasm_sym) = sym.strip_prefix("wasm_use:") {
            Some(format!("wasm:{wasm_sym}"))
        } else {
            sym.strip_prefix("proto_use:")
                .map(|proto_stem| format!("proto:{proto_stem}"))
        };
        if let Some(tag) = target_tag
            && let Some(providers) = bridge_index.providers_by_tag.get(&tag)
        {
            for other_path in providers {
                if other_path != path {
                    deps.insert(other_path.clone());
                }
            }
        }
    }
    let parent = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
    let rs_crate_dirs = if path.ends_with(".rs") {
        owning_rust_crate_dirs(path, files)
    } else {
        Vec::new()
    };
    let fe_pkg_dirs: Vec<String> = if is_frontend_source_path(path) {
        let mut dirs = Vec::new();
        let mut cur = Path::new(path).parent();
        while let Some(d) = cur {
            let ds = normalize_rel_str(d);
            let pkg_json = if ds.is_empty() {
                "package.json".to_string()
            } else {
                format!("{ds}/package.json")
            };
            if files.contains_key(&pkg_json) {
                dirs.push(ds);
            }
            cur = d.parent();
        }
        dirs
    } else {
        Vec::new()
    };
    let url_raw_fs_refs: BTreeSet<&str> = node
        .parsed
        .defined_symbols
        .iter()
        .filter_map(|s| s.strip_prefix("fe_raw_url_fs:"))
        .collect();
    let cwd_raw_fs_refs: BTreeSet<&str> = node
        .parsed
        .defined_symbols
        .iter()
        .filter_map(|s| {
            s.strip_prefix("rust_raw_fs:")
                .or_else(|| s.strip_prefix("fe_raw_fs:"))
        })
        .collect();
    for data_ref in &node.parsed.data_refs {
        let is_url_source_only =
            url_raw_fs_refs.contains(data_ref.as_str()) && !cwd_raw_fs_refs.contains(data_ref.as_str());
        if is_url_source_only {
            if let Some(relative) = normalize_lexical_rel(&parent.join(data_ref))
                && files.contains_key(&relative)
                && relative != path
            {
                deps.insert(relative);
            }
            continue;
        }
        if let Some(norm_root) = normalize_lexical_rel(Path::new(data_ref))
            && files.contains_key(&norm_root)
            && norm_root != path
        {
            deps.insert(norm_root);
        }
        if !parent.as_os_str().is_empty()
            && let Some(relative) = normalize_lexical_rel(&parent.join(data_ref))
        {
            if files.contains_key(&relative) && relative != path {
                deps.insert(relative.clone());
            }
            let dir_pfx = format!("{relative}/");
            for (k, _) in files
                .range(dir_pfx.clone()..)
                .take_while(|(k, _)| k.starts_with(&dir_pfx))
            {
                if k != path {
                    deps.insert(k.clone());
                }
            }
        }
        for base_dir in rs_crate_dirs.iter().chain(fe_pkg_dirs.iter()) {
            if !base_dir.is_empty()
                && let Some(base_rel) =
                    normalize_lexical_rel(&Path::new(base_dir).join(data_ref))
            {
                if files.contains_key(&base_rel) && base_rel != path {
                    deps.insert(base_rel);
                } else {
                    let dir_pfx = format!("{base_rel}/");
                    for (k, _) in files
                        .range(dir_pfx.clone()..)
                        .take_while(|(k, _)| k.starts_with(&dir_pfx))
                    {
                        if k != path {
                            deps.insert(k.clone());
                        }
                    }
                }
            }
        }
        // Also link all descendant files when `data_ref` refers to a workspace directory
        // (e.g. `cargo:rerun-if-changed=assets` or `cargo::rerun-if-changed=proto/`).
        if let Some(norm_dir) = normalize_lexical_rel(Path::new(data_ref)) {
            let dir_pfx = format!("{norm_dir}/");
            for (k, _) in files
                .range(dir_pfx.clone()..)
                .take_while(|(k, _)| k.starts_with(&dir_pfx))
            {
                if k != path {
                    deps.insert(k.clone());
                }
            }
        }
    }
    if let Some(runtime_deps) = runtime_deps_for_test.get(path) {
        deps.extend(
            runtime_deps
                .iter()
                .filter(|dependency| files.contains_key(*dependency))
                .cloned(),
        );
    }
    deps
}


