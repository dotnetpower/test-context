use super::*;

/// Returns `(name, required_features)` for `[[example]]` targets declared with `test = true` in
/// `Cargo.toml` (plus crate default features in the returned tuple so callers can check feature
/// eligibility without failing on unmet `required-features`).
pub fn collect_rust_test_enabled_examples(
    crate_root: &Path,
) -> (Vec<(String, Vec<String>)>, BTreeSet<String>) {
    let Some(text) = read_normalized_cargo_toml(crate_root) else {
        return (Vec::new(), BTreeSet::new());
    };
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut default_features: BTreeSet<String> = BTreeSet::new();
    let mut section = String::new();
    let mut cur_name: Option<String> = None;
    let mut cur_test = false;
    let mut cur_req_feats: Vec<String> = Vec::new();
    let mut collecting_req = false;
    let mut collecting_default_feats = false;
    let parse_quoted_items = parse_toml_string_array_items;
    let flush = |out: &mut Vec<(String, Vec<String>)>,
                 name: &mut Option<String>,
                 is_test: &mut bool,
                 reqs: &mut Vec<String>| {
        if *is_test && let Some(n) = name.take() {
            out.push((n, std::mem::take(reqs)));
        }
        *name = None;
        *is_test = false;
        reqs.clear();
    };
    for raw in text.lines() {
        let line = strip_toml_comment_outside_quotes(raw).trim();
        if line.is_empty() {
            continue;
        }
        if !collecting_req
            && !collecting_default_feats
            && let Some(h) = normalize_toml_table_header(line)
        {
            if section == "[[example]]" {
                flush(&mut out, &mut cur_name, &mut cur_test, &mut cur_req_feats);
            }
            section = h;
            continue;
        }
        if section == "[features]" {
            if collecting_default_feats {
                let mut items = Vec::new();
                parse_quoted_items(line, &mut items);
                default_features.extend(items);
                if line.contains(']') {
                    collecting_default_feats = false;
                }
            } else if let Some((k, v)) = split_toml_kv_outside_quotes(line)
                && unquote_toml_scalar(k.trim()).as_deref().unwrap_or(k.trim()) == "default"
            {
                let mut items = Vec::new();
                parse_quoted_items(v, &mut items);
                default_features.extend(items);
                if v.contains('[') && !v.contains(']') {
                    collecting_default_feats = true;
                }
            }
            continue;
        }
        if section != "[[example]]" {
            continue;
        }
        if collecting_req {
            parse_quoted_items(line, &mut cur_req_feats);
            if line.contains(']') {
                collecting_req = false;
            }
            continue;
        }
        if let Some((k, v)) = split_toml_kv_outside_quotes(line) {
            let kt = unquote_toml_scalar(k.trim()).unwrap_or_else(|| k.trim().to_string());
            match kt.as_str() {
                "name" => {
                    let n = unquote_toml_scalar(v.trim())
                        .unwrap_or_else(|| v.trim().trim_matches('"').trim_matches('\'').to_string());
                    let trimmed = n.trim();
                    if !trimmed.is_empty() {
                        cur_name = Some(trimmed.to_string());
                    }
                }
                "test" => {
                    cur_test = v.trim() == "true";
                }
                "required-features" => {
                    parse_quoted_items(v, &mut cur_req_feats);
                    if v.contains('[') && !v.contains(']') {
                        collecting_req = true;
                    }
                }
                _ => {}
            }
        }
    }
    if section == "[[example]]" {
        flush(&mut out, &mut cur_name, &mut cur_test, &mut cur_req_feats);
    }
    out.sort();
    out.dedup();
    (out, default_features)
}

/// Lists all binary `(bin_name, root_rel)` pairs for a crate (`[[bin]]`, default `src/main.rs`,
/// and auto-discovered `src/bin/*` when `autobins` is enabled) for `--bin <glob>` expansion.
pub fn collect_rust_bin_roots_and_names(crate_root: &Path) -> Vec<(String, String)> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (p, names) in &manifest.custom_bins {
        if crate_root.join(p).is_file() {
            for name in names {
                if seen.insert(name.clone()) {
                    out.push((name.clone(), p.clone()));
                }
            }
        }
    }
    if let Some(ref main_bin) = manifest.main_bin
        && crate_root.join("src/main.rs").is_file()
        && seen.insert(main_bin.clone())
    {
        out.push((main_bin.clone(), "src/main.rs".to_string()));
    }
    if manifest.autobins {
        for (p, name) in auto_discovered_rust_bin_roots(crate_root) {
            if seen.insert(name.clone()) {
                out.push((name, p));
            }
        }
    }
    out.sort();
    out
}

/// Extracts `[package]` `(name, optional_version)` from `crate_root/Cargo.toml`, resolving
/// `version.workspace = true` from an ancestor workspace `Cargo.toml` when present.
pub fn cargo_manifest_package_name_and_version(crate_root: &Path) -> Option<(String, Option<String>)> {
    let text = read_normalized_cargo_toml(crate_root)?;
    let mut in_package = false;
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut workspace_ver = false;
    for raw in text.lines() {
        let line = strip_toml_comment_outside_quotes(raw).trim();
        if let Some(h) = normalize_toml_table_header(line) {
            in_package = h == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some((k, v)) = split_toml_kv_outside_quotes(line) {
            let kt = unquote_toml_scalar(k.trim()).unwrap_or_else(|| k.trim().to_string());
            match kt.as_str() {
                "name" => {
                    if let Some(decoded) = unquote_toml_scalar(v.trim()) {
                        let n = decoded.trim();
                        if !n.is_empty() {
                            name = Some(n.to_string());
                        }
                    }
                }
                "version" => {
                    let vt = v.trim();
                    if vt.contains("workspace") && vt.contains("true") {
                        workspace_ver = true;
                    } else if let Some(decoded) = unquote_toml_scalar(vt) {
                        let clean = decoded.trim();
                        if !clean.is_empty() {
                            version = Some(clean.to_string());
                        }
                    }
                }
                "version.workspace" if v.trim() == "true" => {
                    workspace_ver = true;
                }
                _ => {}
            }
        }
    }
    if version.is_none() && workspace_ver {
        // Start at `crate_root` itself so a workspace-root `[package]` with
        // `version.workspace = true` resolves `[workspace.package] version` in the same manifest!
        let mut cur = Some(crate_root);
        while let Some(anc) = cur {
            if let Some(ws_text) = read_normalized_cargo_toml(anc) {
                let mut in_ws_pkg = false;
                for raw in ws_text.lines() {
                    let line = strip_toml_comment_outside_quotes(raw).trim();
                    if let Some(h) = normalize_toml_table_header(line) {
                        in_ws_pkg = h == "[workspace.package]";
                        continue;
                    }
                    if in_ws_pkg
                        && let Some((k, v)) = split_toml_kv_outside_quotes(line)
                        && unquote_toml_scalar(k.trim()).as_deref().unwrap_or(k.trim()) == "version"
                        && let Some(decoded) = unquote_toml_scalar(v.trim())
                    {
                        let clean = decoded.trim();
                        if !clean.is_empty() {
                            version = Some(clean.to_string());
                            break;
                        }
                    }
                }
                if version.is_some() {
                    break;
                }
            }
            cur = anc.parent();
        }
    }
    if name.is_none() {
        name = cargo_manifest_package_name_via_metadata(crate_root);
    }
    name.map(|n| (n, version))
}

/// Returns true if the crate declares any `[[example]]` target or has auto-discovered examples
/// under `examples/` (when `autoexamples` is not disabled in `Cargo.toml`).
pub fn cargo_manifest_has_examples(crate_root: &Path) -> bool {
    let mut autoexamples = true;
    if let Some(text) = read_normalized_cargo_toml(crate_root) {
        let mut in_package = false;
        for raw in text.lines() {
            let line = strip_toml_comment_outside_quotes(raw).trim();
            if let Some(h) = normalize_toml_table_header(line) {
                if h == "[[example]]" {
                    return true;
                }
                in_package = h == "[package]";
                continue;
            }
            if in_package
                && let Some((k, v)) = line.split_once('=')
                && k.trim() == "autoexamples"
                && v.trim() == "false"
            {
                autoexamples = false;
            }
        }
    }
    if autoexamples
        && let Ok(entries) = std::fs::read_dir(crate_root.join("examples"))
    {
        for ent in entries.flatten() {
            let p = ent.path();
            if (p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("rs"))
                || (p.is_dir() && p.join("main.rs").is_file())
            {
                return true;
            }
        }
    }
    false
}

/// Parses a Cargo package ID specification (`name`, `name@version`, legacy `name:version`, glob
/// `test-*`, or full pkgid URL `path+file:///...#name@version`) into `(name_or_glob, optional_ver)`.
pub fn parse_cargo_pkg_spec(spec: &str) -> (&str, Option<&str>) {
    let s = spec.trim();
    // Full Cargo pkgid URL form: `scheme+...#name@ver` or `scheme+...#ver`
    let tail = if let Some((url_part, frag)) = s.split_once('#') {
        if frag.contains('@') || frag.contains(':') {
            frag
        } else if frag.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            // `path+file:///.../foo#0.1.0` -> name comes from last path segment, ver is `frag`
            let name = url_part
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(frag);
            return (name.trim(), Some(frag.trim()));
        } else {
            frag
        }
    } else {
        s
    };
    if let Some((n, v)) = tail.split_once('@') {
        (n.trim(), Some(v.trim()))
    } else if let Some((n, v)) = tail.split_once(':') {
        // Legacy Cargo pkgid syntax: `name:0.1.4`
        (n.trim(), Some(v.trim()))
    } else {
        (tail.trim(), None)
    }
}

/// Resolves a Cargo `-p` / `--package` specification (exact name, glob pattern `'test-*'`,
/// `name@version`, legacy `name:version`, or full pkgid) against workspace packages via
/// `cargo metadata --no-deps --format-version 1`, returning `(workspace_root, matched_dirs)`
/// relative to `workspace_root` (so sibling packages work even when invoked from inside a member
/// directory!). Returns `Some((ws_root, empty))` when metadata ran authoritatively and matched
/// nothing, or `None` when `cargo metadata` could not run.
pub fn resolve_cargo_package_dirs_via_metadata(
    root: &Path,
    pkg_spec: &str,
) -> Option<(PathBuf, Vec<String>)> {
    let spec_trimmed = pkg_spec.trim();
    if spec_trimmed.is_empty() {
        return Some((root.to_path_buf(), Vec::new()));
    }
    let (name_spec, want_ver) = parse_cargo_pkg_spec(spec_trimmed);
    let glob_matcher = if name_spec.contains(['*', '?', '[']) {
        globset::GlobBuilder::new(name_spec)
            .literal_separator(false)
            .build()
            .ok()
            .map(|g| g.compile_matcher())
    } else {
        None
    };
    let cargo_cmd = crate::catalog::detect_cargo_cmd(root);
    let (prog, base_args) = cargo_cmd.split_first()?;
    let mut cmd = Command::new(prog);
    cmd.current_dir(root);
    cmd.args(base_args);
    cmd.args(["metadata", "--no-deps", "--format-version", "1"]);
    let out = run_command_with_timeout_tracked(
        &mut cmd,
        Duration::from_secs(10),
        8 * 1024 * 1024,
        None,
        None,
    )
    .ok()?;
    if out.timed_out || out.cancelled || !out.status.success() {
        return None;
    }
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let ws_root_str = val.get("workspace_root").and_then(serde_json::Value::as_str);
    let ws_root = ws_root_str
        .map(|s| PathBuf::from(s).canonicalize().unwrap_or_else(|_| PathBuf::from(s)))
        .unwrap_or_else(|| root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
    let ws_members: BTreeSet<&str> = val
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .collect()
        })
        .unwrap_or_default();
    let pkgs = val.get("packages").and_then(serde_json::Value::as_array)?;
    let mut matched_dirs: BTreeSet<String> = BTreeSet::new();
    for pkg in pkgs {
        let Some(id) = pkg.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !ws_members.is_empty() && !ws_members.contains(id) {
            continue;
        }
        let exact_id_match = id == spec_trimmed;
        if !exact_id_match {
            let Some(mp) = pkg.get("manifest_path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(dir) = Path::new(mp).parent() else {
                continue;
            };
            let canon_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
            // For URL-qualified package ID specs (e.g. `path+file:///other#name@ver` or
            // `registry+https://...#name@ver`), require the source URI to actually match this
            // package's manifest directory or ID so nonexistent URLs are rejected just like Cargo!
            if spec_trimmed.contains("://") {
                let src_part = spec_trimmed
                    .split_once('#')
                    .map_or(spec_trimmed, |(s, _)| s);
                let src_matches = if let Some(parsed_path) = cargo_file_url_to_path(src_part) {
                    let spec_dir = parsed_path
                        .canonicalize()
                        .unwrap_or(parsed_path);
                    spec_dir == canon_dir
                } else {
                    id.starts_with(src_part)
                };
                if !src_matches {
                    continue;
                }
            }
            let Some(name) = pkg.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let name_ok = if let Some(ref gm) = glob_matcher {
                gm.is_match(name)
            } else {
                name == name_spec
            };
            if !name_ok {
                continue;
            }
            if let Some(wv) = want_ver {
                let actual = pkg.get("version").and_then(serde_json::Value::as_str).unwrap_or("");
                if !cargo_version_spec_matches(actual, wv) {
                    continue;
                }
            }
        }
        let Some(mp) = pkg.get("manifest_path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(dir) = Path::new(mp).parent() else {
            continue;
        };
        let canon_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if let Ok(rel) = canon_dir.strip_prefix(&ws_root) {
            let s = rel.to_string_lossy().replace('\\', "/");
            matched_dirs.insert(if s.is_empty() { ".".to_string() } else { s });
        }
    }
    Some((ws_root, matched_dirs.into_iter().collect()))
}

/// Checks whether a Cargo package `actual` version (e.g. `"0.1.4"`) matches a Cargo pkgid
/// `@version` spec (exact `"0.1.4"`, or prefix `"0.1"` / `"0"`).
pub fn cargo_version_spec_matches(actual: &str, want: &str) -> bool {
    let want = want.trim();
    if want.is_empty() || actual.is_empty() {
        return false;
    }
    actual == want || actual.starts_with(&format!("{want}."))
}

/// Lists all integration test `(target_name, root_rel)` pairs for a crate (`[[test]]` + auto-discovered
/// `tests/*.rs` and `tests/*/main.rs` when `autotests` is enabled).
pub fn collect_rust_integration_test_roots_and_names(crate_root: &Path) -> Vec<(String, String)> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (p, names) in &manifest.custom_tests {
        if crate_root.join(p).is_file() {
            for name in names {
                if seen.insert(name.clone()) {
                    out.push((name.clone(), p.clone()));
                }
            }
        }
    }
    if manifest.autotests
        && let Ok(entries) = std::fs::read_dir(crate_root.join("tests"))
    {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("rs")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && seen.insert(stem.to_string())
            {
                out.push((stem.to_string(), format!("tests/{stem}.rs")));
            } else if p.is_dir()
                && let Some(dname) = p.file_name().and_then(|n| n.to_str())
                && p.join("main.rs").is_file()
                && seen.insert(dname.to_string())
            {
                out.push((dname.to_string(), format!("tests/{dname}/main.rs")));
            }
        }
    }
    out.sort();
    out
}

/// Extracts `[package] name = "..."` from `crate_root/Cargo.toml`, if present.
pub fn cargo_manifest_package_name(crate_root: &Path) -> Option<String> {
    let text = read_normalized_cargo_toml(crate_root)?;
    let mut in_package = false;
    for raw in text.lines() {
        let line = strip_toml_comment_outside_quotes(raw).trim();
        if let Some(h) = normalize_toml_table_header(line) {
            in_package = h == "[package]";
            continue;
        }
        if in_package
            && let Some((k, v)) = split_toml_kv_outside_quotes(line)
            && unquote_toml_scalar(k.trim()).as_deref().unwrap_or(k.trim()) == "name"
            && let Some(decoded) = unquote_toml_scalar(v.trim())
        {
            let trimmed = decoded.trim();
            if !trimmed.is_empty() && !trimmed.contains(['"', '\'', '\n', '\r']) {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub(super) fn cargo_manifest_package_name_via_metadata(crate_root: &Path) -> Option<String> {
    let target_manifest = crate_root
        .join("Cargo.toml")
        .canonicalize()
        .unwrap_or_else(|_| crate_root.join("Cargo.toml"));
    let cargo_cmd = crate::catalog::detect_cargo_cmd(crate_root);
    let (prog, base_args) = cargo_cmd.split_first()?;
    let mut cmd = Command::new(prog);
    cmd.current_dir(crate_root);
    cmd.args(base_args);
    cmd.args(["metadata", "--no-deps", "--format-version", "1"]);
    let out = run_command_with_timeout_tracked(
        &mut cmd,
        Duration::from_secs(10),
        8 * 1024 * 1024,
        None,
        None,
    )
    .ok()?;
    if out.timed_out || out.cancelled || !out.status.success() {
        return None;
    }
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    for pkg in val.get("packages")?.as_array()? {
        let mp = pkg.get("manifest_path")?.as_str()?;
        let mp_canon = Path::new(mp)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(mp));
        if mp_canon == target_manifest
            && let Some(name) = pkg.get("name").and_then(serde_json::Value::as_str)
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }
    None
}

/// Resolves Cargo's actual `workspace_root` AND workspace member directories relative to that
/// `workspace_root` via `cargo metadata --no-deps --format-version 1`. Unlike
/// `discover_cargo_workspace_member_dirs`, this handles running `--workspace` from inside a
/// nested workspace member directory without discarding sibling packages!
pub fn discover_cargo_workspace_root_and_members(
    dir: &Path,
    default_only: bool,
) -> Option<(PathBuf, Vec<String>)> {
    let cargo_cmd = crate::catalog::detect_cargo_cmd(dir);
    let (prog, base_args) = cargo_cmd.split_first()?;
    let mut cmd = Command::new(prog);
    cmd.current_dir(dir);
    cmd.args(base_args);
    cmd.args(["metadata", "--no-deps", "--format-version", "1"]);
    let out = run_command_with_timeout_tracked(
        &mut cmd,
        Duration::from_secs(10),
        8 * 1024 * 1024,
        None,
        None,
    )
    .ok()?;
    if out.timed_out || out.cancelled || !out.status.success() {
        return None;
    }
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let ws_root_str = val.get("workspace_root").and_then(serde_json::Value::as_str)?;
    let ws_root = PathBuf::from(ws_root_str)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(ws_root_str));
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
    if selected_ids.is_empty() {
        return None;
    }
    let pkgs = val.get("packages").and_then(serde_json::Value::as_array)?;
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for pkg in pkgs {
        let Some(id) = pkg.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !selected_ids.contains(id) {
            continue;
        }
        let Some(mp) = pkg.get("manifest_path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(pdir) = Path::new(mp).parent() else {
            continue;
        };
        let canon_dir = pdir.canonicalize().unwrap_or_else(|_| pdir.to_path_buf());
        if let Ok(rel) = canon_dir.strip_prefix(&ws_root) {
            let s = rel.to_string_lossy().replace('\\', "/");
            dirs.insert(if s.is_empty() { ".".to_string() } else { s });
        }
    }
    if dirs.is_empty() {
        return None;
    }
    Some((ws_root, dirs.into_iter().collect()))
}

/// Returns true if the crate has a `[lib]` target (honoring `test = false` when `default_run` is
/// true) that needs direct Cargo execution (0 static `#[test]` functions or module-level macros).
pub fn rust_lib_target_needs_direct_cargo_mode(crate_root: &Path, default_run: bool) -> bool {
    let manifest = parse_cargo_manifest_targets(crate_root);
    if default_run && !manifest.lib_test_enabled {
        return false;
    }
    let Some(lib_root) = manifest.lib_path else {
        return false;
    };
    collect_rust_target_tree_files(crate_root, &lib_root, true).is_empty()
        || rust_target_tree_may_have_dynamic_tests(crate_root, &lib_root)
}

pub fn rust_lib_target_needs_direct_cargo(crate_root: &Path) -> bool {
    rust_lib_target_needs_direct_cargo_mode(crate_root, false)
}

/// Returns true if a named `--bin <name>` target needs direct Cargo execution (0 static tests or
/// module-level macros that may generate tests).
pub fn rust_named_bin_needs_direct_cargo(crate_root: &Path, name: &str) -> bool {
    let Some(root_rel) = resolve_rust_named_bin_target_path(crate_root, name) else {
        return false;
    };
    collect_rust_target_tree_files(crate_root, &root_rel, true).is_empty()
        || rust_target_tree_may_have_dynamic_tests(crate_root, &root_rel)
}

/// Returns true if any binary target in `crate_root` contains module-level macros that may generate tests.
pub fn rust_any_bin_has_dynamic_tests(crate_root: &Path) -> bool {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for p in manifest.custom_bins.keys() {
        if crate_root.join(p).is_file() {
            roots.insert(p.clone());
        }
    }
    if manifest.main_bin.is_some() && crate_root.join("src/main.rs").is_file() {
        roots.insert("src/main.rs".to_string());
    }
    if manifest.autobins {
        for (p, _) in auto_discovered_rust_bin_roots(crate_root) {
            roots.insert(p);
        }
    }
    roots
        .iter()
        .any(|r| rust_target_tree_may_have_dynamic_tests(crate_root, r))
}

/// Returns true if `--bins` (or default-run binaries when `default_run` is true, honoring
/// `test = false` and `required-features`) needs direct Cargo execution.
pub fn rust_all_bins_need_direct_cargo_mode(
    crate_root: &Path,
    default_run: bool,
    extra_args: &[String],
) -> bool {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for (p, names) in &manifest.custom_bins {
        let any_active = names.iter().any(|bn| {
            (!default_run || !manifest.bin_test_disabled.contains(bn))
                && manifest
                    .custom_bin_required_features
                    .get(bn)
                    .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
        });
        if any_active && crate_root.join(p).is_file() {
            roots.insert(p.clone());
        }
    }
    if let Some(ref main_bin) = manifest.main_bin
        && (!default_run || !manifest.bin_test_disabled.contains(main_bin))
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
            if (!default_run || !manifest.bin_test_disabled.contains(&bn))
                && manifest
                    .custom_bin_required_features
                    .get(&bn)
                    .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
            {
                roots.insert(p);
            }
        }
    }
    !roots.is_empty()
        && roots.iter().any(|r| {
            collect_rust_target_tree_files(crate_root, r, true).is_empty()
                || rust_target_tree_may_have_dynamic_tests(crate_root, r)
        })
}

pub fn rust_all_bins_need_direct_cargo(crate_root: &Path) -> bool {
    rust_all_bins_need_direct_cargo_mode(crate_root, false, &[])
}

/// Collects all test-bearing crate-relative `.rs` files owned by a single package (`[lib]`, all
/// binaries, and all feature-eligible integration tests including `tests/common.rs` and
/// `tests/common/main.rs`), honoring `[lib] test = false` and `[[bin]] test = false` on default runs.
pub fn collect_rust_package_all_target_files_for_args(
    crate_root: &Path,
    extra_args: &[String],
) -> Vec<String> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut out: BTreeSet<String> = BTreeSet::new();
    if manifest.lib_test_enabled
        && let Some(ref lib_root) = manifest.lib_path
    {
        out.extend(collect_rust_target_tree_files(crate_root, lib_root, true));
    }
    for (p, names) in &manifest.custom_bins {
        let any_eligible = names.iter().any(|bn| {
            !manifest.bin_test_disabled.contains(bn)
                && manifest
                    .custom_bin_required_features
                    .get(bn)
                    .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
        });
        if any_eligible && crate_root.join(p).is_file() {
            out.extend(collect_rust_target_tree_files(crate_root, p, true));
        }
    }
    if let Some(ref main_bin) = manifest.main_bin
        && !manifest.bin_test_disabled.contains(main_bin)
        && manifest
            .custom_bin_required_features
            .get(main_bin)
            .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
        && crate_root.join("src/main.rs").is_file()
    {
        out.extend(collect_rust_target_tree_files(
            crate_root,
            "src/main.rs",
            true,
        ));
    }
    if manifest.autobins {
        for (p, bn) in auto_discovered_rust_bin_roots(crate_root) {
            let eligible = !manifest.bin_test_disabled.contains(&bn)
                && manifest
                    .custom_bin_required_features
                    .get(&bn)
                    .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args));
            if eligible {
                out.extend(collect_rust_target_tree_files(crate_root, &p, true));
            }
        }
    }
    for (p, names) in &manifest.custom_tests {
        let any_eligible = names.iter().any(|tn| {
            !manifest.test_disabled.contains(tn)
                && manifest
                    .custom_test_required_features
                    .get(tn)
                    .is_none_or(|rf| cargo_required_features_satisfied(crate_root, rf, extra_args))
        });
        if any_eligible && crate_root.join(p).is_file() {
            out.insert(p.clone());
            out.extend(collect_rust_target_tree_files(crate_root, p, true));
        }
    }
    if manifest.autotests
        && let Ok(entries) = std::fs::read_dir(crate_root.join("tests"))
    {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("rs")
                && let Some(fname) = p.file_name().and_then(|n| n.to_str())
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                let eligible = !manifest.test_disabled.contains(stem)
                    && manifest
                        .custom_test_required_features
                        .get(stem)
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(crate_root, rf, extra_args)
                        });
                if eligible {
                    let rel = format!("tests/{fname}");
                    out.insert(rel.clone());
                    out.extend(collect_rust_target_tree_files(crate_root, &rel, true));
                }
            } else if p.is_dir()
                && let Some(dname) = p.file_name().and_then(|n| n.to_str())
                && p.join("main.rs").is_file()
            {
                let eligible = !manifest.test_disabled.contains(dname)
                    && manifest
                        .custom_test_required_features
                        .get(dname)
                        .is_none_or(|rf| {
                            cargo_required_features_satisfied(crate_root, rf, extra_args)
                        });
                if eligible {
                    let rel = format!("tests/{dname}/main.rs");
                    out.insert(rel.clone());
                    out.extend(collect_rust_target_tree_files(crate_root, &rel, true));
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Collects Cargo target-selector invocation groups (`[--lib, --bin <name>, --test <name>, ...]`,
/// `[--doc]`, `[--examples, --no-run]`) for enabled testable targets in `crate_root` that either:
/// - have NO source files in `covered_files` (e.g., a testless `src/main.rs` with `compile_error!`,
///   a testless `harness = false` target, or a `test = true` example), OR
/// - may contain macro-generated tests (`rust_target_tree_may_have_dynamic_tests`) in a partially
///   covered target tree, OR
/// - have doctests or compile-checked examples on a package-wide run!
pub(super) fn collect_uncovered_rust_targets_for_crate(
    crate_root: &Path,
    crate_rel: &str,
    covered_files: &BTreeSet<String>,
    extra_args: &[String],
) -> Vec<Vec<String>> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut main_group: Vec<String> = Vec::new();
    // Precompute complete target file sets for every enabled target so if TWO targets share ANY
    // source file (e.g. `src/main.rs` mounts `src/lib.rs` via `mod lib;` or vice versa), NEITHER
    // target can mark the other covered and mask a target-only failure!
    let mut target_trees: Vec<(String, String, BTreeSet<String>)> = Vec::new();
    let mut file_owner_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut register_target = |kind_id: String, path: String| {
        if !crate_root.join(&path).is_file() {
            return;
        }
        let tree: BTreeSet<String> = collect_rust_target_tree_files(crate_root, &path, false)
            .into_iter()
            .collect();
        for f in &tree {
            *file_owner_counts.entry(f.clone()).or_insert(0) += 1;
        }
        target_trees.push((kind_id, path, tree));
    };
    if manifest.lib_test_enabled
        && let Some(ref lp) = manifest.lib_path
    {
        register_target("lib".to_string(), lp.clone());
    }
    let mut seen_bins: BTreeSet<String> = BTreeSet::new();
    let mut collect_bin = |path: &str, name: &str| {
        if !seen_bins.insert(name.to_string()) || manifest.bin_test_disabled.contains(name) {
            return;
        }
        if let Some(rf) = manifest.custom_bin_required_features.get(name)
            && !cargo_required_features_satisfied(crate_root, rf, extra_args)
        {
            return;
        }
        register_target(format!("bin:{name}"), path.to_string());
    };
    for (p, names) in &manifest.custom_bins {
        for bn in names {
            collect_bin(p, bn);
        }
    }
    if let Some(ref mb) = manifest.main_bin {
        collect_bin("src/main.rs", mb);
    }
    if manifest.autobins {
        for (p, bn) in auto_discovered_rust_bin_roots(crate_root) {
            collect_bin(&p, &bn);
        }
    }
    let mut seen_tests: BTreeSet<String> = BTreeSet::new();
    let mut collect_test = |path: &str, name: &str| {
        if !seen_tests.insert(name.to_string()) || manifest.test_disabled.contains(name) {
            return;
        }
        if let Some(rf) = manifest.custom_test_required_features.get(name)
            && !cargo_required_features_satisfied(crate_root, rf, extra_args)
        {
            return;
        }
        register_target(format!("test:{name}"), path.to_string());
    };
    for (p, names) in &manifest.custom_tests {
        for tn in names {
            collect_test(p, tn);
        }
    }
    if manifest.autotests
        && let Ok(rd) = std::fs::read_dir(crate_root.join("tests"))
    {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some("rs")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                collect_test(&format!("tests/{stem}.rs"), stem);
            } else if p.is_dir()
                && p.join("main.rs").is_file()
                && let Some(stem) = p.file_name().and_then(|s| s.to_str())
            {
                collect_test(&format!("tests/{stem}/main.rs"), stem);
            }
        }
    }
    for (kind_id, path, tree) in &target_trees {
        let is_covered_uniquely = tree.iter().any(|f| {
            let full = if crate_rel == "." || crate_rel.is_empty() {
                f.clone()
            } else {
                format!("{crate_rel}/{f}")
            };
            covered_files.contains(&full) && file_owner_counts.get(f).copied().unwrap_or(0) == 1
        });
        let shares_any_file = tree
            .iter()
            .any(|f| file_owner_counts.get(f).copied().unwrap_or(0) > 1);
        let is_custom_harness = manifest.harness_false_targets.contains(kind_id);
        if !is_covered_uniquely
            || shares_any_file
            || is_custom_harness
            || rust_target_tree_may_have_dynamic_tests(crate_root, path)
        {
            if kind_id == "lib" {
                main_group.push("--lib".to_string());
            } else if let Some(bn) = kind_id.strip_prefix("bin:") {
                main_group.push("--bin".to_string());
                main_group.push(bn.to_string());
            } else if let Some(tn) = kind_id.strip_prefix("test:") {
                main_group.push("--test".to_string());
                main_group.push(tn.to_string());
            }
        }
    }
    let (test_examples, _) = collect_rust_test_enabled_examples(crate_root);
    for (ename, req_feats) in test_examples {
        if cargo_required_features_satisfied(crate_root, &req_feats, extra_args) {
            main_group.push("--example".to_string());
            main_group.push(ename);
        }
    }
    let mut groups: Vec<Vec<String>> = Vec::new();
    if !main_group.is_empty() {
        groups.push(main_group);
    }
    let has_no_run = extra_args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--no-run");
    // Cargo's `[lib] doctest` setting is independent of `[lib] test = false`:
    // `rust_lib_has_potential_doctests` already checks `doctest = false`, so run `--doc`
    // even when `[lib] test = false`!
    if !has_no_run && rust_lib_has_potential_doctests(crate_root) {
        groups.push(vec!["--doc".to_string()]);
    }
    // Default `cargo test` compiles ordinary examples WITHOUT `#[cfg(test)]` (identical to
    // `cargo build --examples`), NOT `cargo test --examples --no-run`!
    if cargo_manifest_has_examples(crate_root) {
        groups.push(vec!["__cargo_build__".to_string(), "--examples".to_string()]);
    }
    groups
}

/// Resolves `(target_args, target_root_rel, tree_files)` tuples (preserving each target's exact
/// Cargo target selector AND actual root file, while honoring `autoexamples` / `autobenches` and
/// explicit manifest overrides!) for a Cargo target selector (`--lib`, `--doc`, `--bins`, `--bin`,
/// `--test`, `--tests`, `--all-targets`, `--example`, `--examples`, `--bench`, `--benches`).
pub fn resolve_rust_target_trees_for_selector(
    crate_root: &Path,
    selector: &str,
    name: Option<&str>,
    extra_args: &[String],
) -> Vec<(Vec<String>, String, Vec<String>)> {
    let manifest = parse_cargo_manifest_targets(crate_root);
    let mut targets: Vec<(Vec<String>, String)> = Vec::new();
    let parse_named_section_paths = |section_header: &str,
                                     auto_key: &str,
                                     default_dir: &str|
     -> Vec<(String, String)> {
        let mut items: Vec<(String, String)> = Vec::new();
        let mut explicit_names: BTreeSet<String> = BTreeSet::new();
        let mut auto_enabled = true;
        if let Some(text) = read_normalized_cargo_toml(crate_root) {
            let mut cur_sec = String::new();
            let mut cur_name: Option<String> = None;
            let mut cur_path: Option<String> = None;
            let flush = |items: &mut Vec<(String, String)>,
                         explicit_names: &mut BTreeSet<String>,
                         cname: &mut Option<String>,
                         cpath: &mut Option<String>| {
                if let Some(n) = cname.take() {
                    let p = cpath.take().unwrap_or_else(|| {
                        let c1 = format!("{default_dir}/{n}.rs");
                        if crate_root.join(&c1).is_file() {
                            c1
                        } else {
                            format!("{default_dir}/{n}/main.rs")
                        }
                    });
                    explicit_names.insert(n.clone());
                    items.push((n, p));
                } else {
                    *cpath = None;
                }
            };
            for raw_line in text.lines() {
                let line = strip_toml_comment_outside_quotes(raw_line).trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(h) = normalize_toml_table_header(line) {
                    if cur_sec == section_header {
                        flush(&mut items, &mut explicit_names, &mut cur_name, &mut cur_path);
                    }
                    cur_sec = h;
                    continue;
                }
                if cur_sec == "[package]" {
                    if let Some((k, v)) = split_toml_kv_outside_quotes(line)
                        && unquote_toml_scalar(k.trim()).as_deref().unwrap_or(k.trim()) == auto_key
                        && v.trim() == "false"
                    {
                        auto_enabled = false;
                    }
                    continue;
                }
                if cur_sec != section_header {
                    continue;
                }
                if let Some((k, v)) = split_toml_kv_outside_quotes(line) {
                    let key_owned =
                        unquote_toml_scalar(k.trim()).unwrap_or_else(|| k.trim().to_string());
                    let key = key_owned.as_str();
                    if let Some(u) = unquote_toml_scalar(v.trim()) {
                        if key == "name" {
                            cur_name = Some(u);
                        } else if key == "path" {
                            cur_path = Some(u.trim_start_matches("./").replace('\\', "/"));
                        }
                    }
                }
            }
            if cur_sec == section_header {
                flush(&mut items, &mut explicit_names, &mut cur_name, &mut cur_path);
            }
        }
        if auto_enabled
            && let Ok(rd) = std::fs::read_dir(crate_root.join(default_dir))
        {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_file()
                    && p.extension().and_then(|e| e.to_str()) == Some("rs")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    && !explicit_names.contains(stem)
                {
                    items.push((stem.to_string(), format!("{default_dir}/{stem}.rs")));
                } else if p.is_dir()
                    && p.join("main.rs").is_file()
                    && let Some(stem) = p.file_name().and_then(|s| s.to_str())
                    && !explicit_names.contains(stem)
                {
                    items.push((stem.to_string(), format!("{default_dir}/{stem}/main.rs")));
                }
            }
        }
        items
    };
    let collect_bins = |targets: &mut Vec<(Vec<String>, String)>| {
        let mut seen_bins: BTreeSet<String> = BTreeSet::new();
        for (p, names) in &manifest.custom_bins {
            for bn in names {
                if seen_bins.insert(bn.clone()) {
                    targets.push((vec!["--bin".to_string(), bn.clone()], p.clone()));
                }
            }
        }
        if let Some(ref mb) = manifest.main_bin
            && crate_root.join("src/main.rs").is_file()
            && seen_bins.insert(mb.clone())
        {
            targets.push((
                vec!["--bin".to_string(), mb.clone()],
                "src/main.rs".to_string(),
            ));
        }
        if manifest.autobins {
            for (p, bn) in auto_discovered_rust_bin_roots(crate_root) {
                if seen_bins.insert(bn.clone()) {
                    targets.push((vec!["--bin".to_string(), bn], p));
                }
            }
        }
    };
    match selector {
        "--lib" => {
            if let Some(ref lp) = manifest.lib_path {
                targets.push((vec!["--lib".to_string()], lp.clone()));
            }
        }
        "--doc" => {
            if let Some(ref lp) = manifest.lib_path {
                targets.push((vec!["--doc".to_string()], lp.clone()));
            }
        }
        "--bins" => {
            collect_bins(&mut targets);
        }
        "--bin" => {
            if let Some(bn) = name
                && let Some(p) = resolve_rust_named_bin_target_path(crate_root, bn)
            {
                targets.push((vec!["--bin".to_string(), bn.to_string()], p));
            }
        }
        "--test" => {
            if let Some(tn) = name
                && let Some(p) = resolve_rust_named_test_target_path(crate_root, tn)
            {
                targets.push((vec!["--test".to_string(), tn.to_string()], p));
            }
        }
        "--example" => {
            if let Some(en) = name {
                for (n, p) in parse_named_section_paths("[[example]]", "autoexamples", "examples") {
                    if n == en {
                        targets.push((vec!["--example".to_string(), n], p));
                    }
                }
            }
        }
        "--examples" => {
            for (n, p) in parse_named_section_paths("[[example]]", "autoexamples", "examples") {
                targets.push((vec!["--example".to_string(), n], p));
            }
        }
        "--bench" => {
            if let Some(bn) = name {
                for (n, p) in parse_named_section_paths("[[bench]]", "autobenches", "benches") {
                    if n == bn {
                        targets.push((vec!["--bench".to_string(), n], p));
                    }
                }
            }
        }
        "--benches" => {
            for (n, p) in parse_named_section_paths("[[bench]]", "autobenches", "benches") {
                targets.push((vec!["--bench".to_string(), n], p));
            }
        }
        "--tests" | "--all-targets" => {
            if let Some(ref lp) = manifest.lib_path {
                targets.push((vec!["--lib".to_string()], lp.clone()));
            }
            collect_bins(&mut targets);
            for (tn, p) in collect_rust_integration_test_roots_and_names(crate_root) {
                targets.push((vec!["--test".to_string(), tn], p));
            }
            if selector == "--all-targets" {
                for (n, p) in parse_named_section_paths("[[example]]", "autoexamples", "examples") {
                    targets.push((vec!["--example".to_string(), n], p));
                }
                for (n, p) in parse_named_section_paths("[[bench]]", "autobenches", "benches") {
                    targets.push((vec!["--bench".to_string(), n], p));
                }
            }
        }
        _ => {
            let _ = extra_args;
        }
    }
    let mut out: Vec<(Vec<String>, String, Vec<String>)> = Vec::new();
    let mut seen: BTreeSet<(Vec<String>, String)> = BTreeSet::new();
    for (targs, r) in targets {
        if crate_root.join(&r).is_file() && seen.insert((targs.clone(), r.clone())) {
            let files = collect_rust_target_tree_files(crate_root, &r, false);
            out.push((targs, r, files));
        }
    }
    out
}

pub fn resolve_rust_target_tree_files_for_selector(
    crate_root: &Path,
    selector: &str,
    name: Option<&str>,
    extra_args: &[String],
) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for (_, _, files) in
        resolve_rust_target_trees_for_selector(crate_root, selector, name, extra_args)
    {
        for f in files {
            out.insert(f);
        }
    }
    out.into_iter().collect()
}


