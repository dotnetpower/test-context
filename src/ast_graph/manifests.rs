use super::*;

pub(super) fn ancestor_conftests_for(test_path: &str, conftests: &BTreeSet<String>) -> Vec<String> {
    let mut result = Vec::new();
    if is_frontend_source_path(test_path) {
        for cf in conftests {
            if !is_frontend_source_path(cf) || cf == test_path {
                continue;
            }
            let scope = conftest_scope_dir(cf);
            if scope.is_empty() || test_path.starts_with(&format!("{scope}/")) {
                result.push(cf.clone());
            }
        }
        return result;
    }
    if test_path.ends_with(".rs") {
        let mut prefix = String::new();
        let parent = Path::new(test_path)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        for comp in parent.components() {
            if let std::path::Component::Normal(os) = comp
                && let Some(seg) = os.to_str()
            {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(seg);
                for cand in [
                    format!("{prefix}/common/mod.rs"),
                    format!("{prefix}/common.rs"),
                ] {
                    if conftests.contains(&cand) && cand != test_path {
                        result.push(cand);
                    }
                }
            }
        }
        return result;
    }
    if conftests.contains("conftest.py") {
        result.push("conftest.py".to_string());
    }
    let mut prefix = String::new();
    let parent = Path::new(test_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    for comp in parent.components() {
        if let std::path::Component::Normal(os) = comp
            && let Some(seg) = os.to_str()
        {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let candidate = format!("{prefix}/conftest.py");
            if conftests.contains(&candidate) {
                result.push(candidate);
            }
        }
    }
    result
}

pub(super) fn build_fixture_provider_index(
    files: &BTreeMap<String, FileNode>,
) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, node) in files {
        // Only conftest.py files implicitly provide fixtures without an explicit Python import
        if node.is_conftest && !node.parsed.fixtures_defined.is_empty() {
            for fx in &node.parsed.fixtures_defined {
                index.entry(fx.clone()).or_default().push(path.clone());
            }
        }
    }
    index
}

pub(super) fn is_provider_applicable(provider_path: &str, consumer_path: &str) -> bool {
    let parent = Path::new(provider_path)
        .parent()
        .map(normalize_rel_str)
        .unwrap_or_default();
    parent.is_empty() || consumer_path.starts_with(&format!("{parent}/"))
}

pub(super) fn extract_package_export_targets(val: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    pub(super) fn visit(val: &serde_json::Value, out: &mut Vec<String>) {
        match val {
            serde_json::Value::String(s) => {
                // Preserve leading `./` so subpath imports (`"#util": "./utils"` vs `"#util": "@org/util"`)
                // never confuse a local relative target with a bare workspace package specifier.
                let clean = s.trim();
                if !clean.is_empty() && !clean.ends_with(".d.ts") && !out.iter().any(|e| e == clean)
                {
                    out.push(clean.to_string());
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    visit(item, out);
                }
            }
            serde_json::Value::Object(map) => {
                for (k, sub) in map {
                    if k == "types" {
                        continue;
                    }
                    visit(sub, out);
                }
            }
            _ => {}
        }
    }
    visit(val, &mut out);
    out
}

pub(super) fn strip_jsonc_comments_and_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut no_comments = String::with_capacity(len);
    let mut i = 0usize;
    let mut in_str = false;
    while i < len {
        if in_str {
            if bytes[i] == b'\\' && i + 1 < len {
                no_comments.push('\\');
                let ch = text[i + 1..].chars().next().unwrap_or(' ');
                no_comments.push(ch);
                i += 1 + ch.len_utf8();
                continue;
            }
            if bytes[i] == b'"' {
                in_str = false;
                no_comments.push('"');
                i += 1;
                continue;
            }
            let ch = text[i..].chars().next().unwrap_or(' ');
            no_comments.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if bytes[i] == b'"' {
            in_str = true;
            no_comments.push('"');
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            i = memchr::memchr(b'\n', &bytes[i + 2..])
                .map(|off| i + 2 + off)
                .unwrap_or(len);
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(len);
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or(' ');
        no_comments.push(ch);
        i += ch.len_utf8();
    }
    // Strip trailing commas before `}` or `]` outside strings.
    let nb = no_comments.as_bytes();
    let nlen = nb.len();
    let mut out = String::with_capacity(nlen);
    let mut j = 0usize;
    let mut in_s = false;
    while j < nlen {
        if in_s {
            if nb[j] == b'\\' && j + 1 < nlen {
                out.push('\\');
                let ch = no_comments[j + 1..].chars().next().unwrap_or(' ');
                out.push(ch);
                j += 1 + ch.len_utf8();
                continue;
            }
            if nb[j] == b'"' {
                in_s = false;
                out.push('"');
                j += 1;
                continue;
            }
            let ch = no_comments[j..].chars().next().unwrap_or(' ');
            out.push(ch);
            j += ch.len_utf8();
            continue;
        }
        if nb[j] == b'"' {
            in_s = true;
            out.push('"');
            j += 1;
            continue;
        }
        if nb[j] == b',' {
            let mut k = j + 1;
            while k < nlen && nb[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < nlen && matches!(nb[k], b'}' | b']') {
                j += 1;
                continue;
            }
        }
        let ch = no_comments[j..].chars().next().unwrap_or(' ');
        out.push(ch);
        j += ch.len_utf8();
    }
    out
}

/// Strips `fe_extends:` (including optional zero-padded array order prefix `fe_extends:001:...`).
pub(super) fn strip_fe_extends_symbol(sym: &str) -> Option<&str> {
    let rest = sym.strip_prefix("fe_extends:")?;
    if rest.len() > 4
        && rest.as_bytes()[3] == b':'
        && rest.as_bytes()[..3].iter().all(u8::is_ascii_digit)
    {
        Some(&rest[4..])
    } else {
        Some(rest)
    }
}

/// Matches a package subpath against a `package.json` `exports` key (exact or wildcard `*`),
/// substituting the wildcard capture into `exp_target` when present.
pub(super) fn match_package_export_subpath(raw_sub: &str, exp_key: &str, exp_target: &str) -> Option<String> {
    if exp_key == "." {
        return None;
    }
    if let Some((kpfx, ksfx)) = exp_key.split_once('*') {
        if raw_sub.starts_with(kpfx)
            && raw_sub.ends_with(ksfx)
            && raw_sub.len() >= kpfx.len() + ksfx.len()
        {
            let mid = &raw_sub[kpfx.len()..raw_sub.len() - ksfx.len()];
            return if exp_target.contains('*') {
                // Substitute EVERY wildcard occurrence (e.g. `./src/*/*.ts`).
                Some(exp_target.replace('*', mid))
            } else {
                Some(exp_target.to_string())
            };
        }
        return None;
    }
    if raw_sub == exp_key {
        return Some(exp_target.to_string());
    }
    if exp_key.ends_with('/') && raw_sub.starts_with(exp_key) {
        let rest = &raw_sub[exp_key.len()..];
        return Some(format!("{exp_target}{rest}"));
    }
    None
}

/// Returns a specificity rank for `package.json` `exports` matching per Node.js spec:
/// exact matches win over wildcard patterns, and longer literal prefix+suffix wins among wildcards.
pub(super) fn package_export_match_specificity(exp_key: &str) -> (u8, usize, usize) {
    if let Some((kpfx, ksfx)) = exp_key.split_once('*') {
        (1, kpfx.len(), ksfx.len())
    } else if exp_key.ends_with('/') {
        (0, exp_key.len(), 0)
    } else {
        (2, exp_key.len(), 0)
    }
}

pub(super) fn tsconfig_path_variants(raw: &str) -> Vec<String> {
    let mut out = vec![raw.to_string()];
    if !raw.ends_with(".json") && !raw.ends_with(".jsonc") {
        out.push(format!("{raw}.json"));
        out.push(format!("{raw}.jsonc"));
        out.push(format!("{raw}/tsconfig.json"));
    }
    out
}

pub(super) fn resolve_tsconfig_extends_target(
    from_path: &str,
    ext_rel: &str,
    files: &BTreeMap<String, FileNode>,
    pkg_to_dir: &BTreeMap<String, String>,
) -> Option<String> {
    let parent_dir = Path::new(from_path).parent().unwrap_or_else(|| Path::new(""));
    // 1. Relative path (`./...`, `../...`, or adjacent file)
    if let Some(ext_norm) = normalize_lexical_rel(&parent_dir.join(ext_rel)) {
        for cand in tsconfig_path_variants(&ext_norm) {
            if files.contains_key(&cand) {
                return Some(cand);
            }
        }
    }
    // 2. Workspace package specifier (e.g. `@acme/config/base.json` or `@acme/config`)
    if !ext_rel.starts_with('.') && !ext_rel.starts_with('/') {
        let mut best: Option<(&str, &str)> = None;
        for (pkg_name, pkg_dir) in pkg_to_dir {
            if (ext_rel == pkg_name || ext_rel.starts_with(&format!("{pkg_name}/")))
                && best.is_none_or(|(prev, _)| pkg_name.len() > prev.len())
            {
                best = Some((pkg_name.as_str(), pkg_dir.as_str()));
            }
        }
        if let Some((pkg_name, pkg_dir)) = best {
            let pkg_json_path = if pkg_dir.is_empty() {
                "package.json".to_string()
            } else {
                format!("{pkg_dir}/package.json")
            };
            let mut sub_candidates: Vec<String> = Vec::new();
            if ext_rel == pkg_name {
                // Check package.json `"tsconfig"` entry and `"exports": { ".": ... }` before
                // falling back to `tsconfig.json`.
                if let Some(pkg_node) = files.get(&pkg_json_path) {
                    for sym in &pkg_node.parsed.defined_symbols {
                        if let Some(ts_entry) = sym.strip_prefix("fe_pkg_tsconfig:") {
                            sub_candidates.push(ts_entry.to_string());
                        } else if let Some(exp_rest) =
                            sym.strip_prefix(&format!("fe_export:{pkg_name}:.->"))
                        {
                            sub_candidates.push(exp_rest.to_string());
                        } else if let Some(main_rest) =
                            sym.strip_prefix(&format!("fe_pkg_main:{pkg_name}->"))
                        {
                            sub_candidates.push(main_rest.to_string());
                        }
                    }
                }
                sub_candidates.push("tsconfig.json".to_string());
            } else {
                let raw_sub = ext_rel.strip_prefix(&format!("{pkg_name}/")).unwrap_or("");
                if let Some(pkg_node) = files.get(&pkg_json_path) {
                    let pkg_exp_pfx = format!("fe_export:{pkg_name}:");
                    let mut ranked: Vec<((u8, usize, usize), String)> = Vec::new();
                    for sym in &pkg_node.parsed.defined_symbols {
                        if let Some(rest) = sym.strip_prefix(&pkg_exp_pfx)
                            && let Some((exp_key, exp_target)) = rest.split_once("->")
                            && let Some(mapped) =
                                match_package_export_subpath(raw_sub, exp_key, exp_target)
                        {
                            ranked.push((package_export_match_specificity(exp_key), mapped));
                        }
                    }
                    // Apply Node.js exports specificity: exact matches before wildcards, longer prefix first.
                    ranked.sort_by_key(|a| std::cmp::Reverse(a.0));
                    for (_, mapped) in ranked {
                        sub_candidates.push(mapped);
                    }
                }
                sub_candidates.push(raw_sub.to_string());
            }
            for sub in sub_candidates {
                let joined = if pkg_dir.is_empty() {
                    sub
                } else {
                    format!("{pkg_dir}/{sub}")
                };
                if let Some(norm) = normalize_lexical_rel(Path::new(&joined)) {
                    for cand in tsconfig_path_variants(&norm) {
                        if files.contains_key(&cand) {
                            return Some(cand);
                        }
                    }
                }
            }
        }
    }
    None
}

pub(super) fn parse_tsconfig_jsonc(text: &str) -> ParsedPythonFile {
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let clean = strip_jsonc_comments_and_trailing_commas(text);
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&clean) else {
        return ParsedPythonFile::default();
    };
    if let Some(ext) = val.get("extends") {
        let mut ext_list = Vec::new();
        if let Some(s) = ext.as_str() {
            ext_list.push(s);
        } else if let Some(arr) = ext.as_array() {
            for item in arr {
                if let Some(s) = item.as_str() {
                    ext_list.push(s);
                }
            }
        }
        for (idx, raw) in ext_list.iter().enumerate() {
            let ext_val = raw.trim().trim_start_matches("./");
            if !ext_val.is_empty() {
                // Preserve TypeScript `extends` array order via zero-padded index prefix so
                // later entries override earlier entries per TS 5.0+ precedence rules.
                defined_symbols.insert(format!("fe_extends:{idx:03}:{ext_val}"));
                for cand in tsconfig_path_variants(ext_val) {
                    data_refs.insert(cand);
                }
            }
        }
    }
    if let Some(opts) = val.get("compilerOptions").and_then(|v| v.as_object()) {
        if let Some(bu_raw) = opts.get("baseUrl").and_then(|v| v.as_str()) {
            let bu = bu_raw.trim().trim_start_matches("./").trim_end_matches('/');
            let stored = if bu.is_empty() { "." } else { bu };
            defined_symbols.insert(format!("fe_baseurl:{stored}"));
        }
        if let Some(paths_obj) = opts.get("paths").and_then(|v| v.as_object()) {
            // Always record `fe_has_paths` even for an explicit empty `"paths": {}` so a child
            // config clears inherited parent `paths` per TypeScript semantics!
            defined_symbols.insert("fe_has_paths".to_string());
            for (raw_key, raw_targets) in paths_obj {
                let Some(arr) = raw_targets.as_array() else {
                    continue;
                };
                // Emit raw wildcard/exact path mappings; effective `baseUrl` (including inherited
                // `baseUrl` from `extends`) is applied in `FrontendModuleIndex::build`.
                let alias_pat = raw_key.trim();
                if alias_pat.is_empty() {
                    continue;
                }
                let mut ordered_targets: Vec<String> = Vec::new();
                for item in arr {
                    let Some(raw_target) = item.as_str() else {
                        continue;
                    };
                    let target_pat = raw_target.trim().trim_start_matches("./");
                    if !target_pat.is_empty() {
                        ordered_targets.push(target_pat.to_string());
                    }
                }
                if !ordered_targets.is_empty() {
                    // Encode all fallback targets in one `\t`-delimited symbol so `BTreeSet`
                    // deduplication preserves the exact JSON array order (e.g. `["@generated/*", "src/*"]`).
                    defined_symbols
                        .insert(format!("fe_alias:{alias_pat}->{}", ordered_targets.join("\t")));
                }
            }
        }
    }
    ParsedPythonFile {
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}

/// Strips JS `//` and `/* ... */` comments while respecting `'...'`, `"..."`, AND `` `...` ``
/// template literals (so ``base: `https://example/` `` never loses the rest of the line to `//`).
pub(super) fn strip_js_comments_preserving_newlines(source: &str) -> String {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    let mut in_quote: Option<u8> = None;
    while i < len {
        if let Some(q) = in_quote {
            if bytes[i] == b'\\' && i + 1 < len {
                let ch = source[i + 1..].chars().next().unwrap_or(' ');
                out.push('\\');
                out.push(ch);
                i += 1 + ch.len_utf8();
                continue;
            }
            if bytes[i] == q {
                in_quote = None;
            }
            let ch = source[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            out.push(' ');
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            out.push(' ');
            i += 2;
            while i < len {
                if bytes[i] == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' || bytes[i] == b'`' {
            in_quote = Some(bytes[i]);
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Checks whether `s` is a SINGLE cleanly-quoted JS string literal (`'...'` or `"..."`) with
/// no unescaped inner quotes (so `'a' + 'b'` is rejected rather than mistaken for a literal!).
pub(super) fn unquote_single_js_literal(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.len() < 2 || (b[0] != b'\'' && b[0] != b'"') || b[b.len() - 1] != b[0] {
        return None;
    }
    let q = b[0];
    let mut i = 1usize;
    while i + 1 < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            return None;
        }
        i += 1;
    }
    Some(&s[1..s.len() - 1])
}

/// Parses common static Vite config path expressions:
/// - single quoted relative string literals: `'./src'` / `"src/components"` (rejecting bare
///   identifiers, string concatenations `'a' + 'b'`, and absolute paths like `/repo/client`!)
/// - `path.resolve(__dirname, 'src', 'components')` / `path.join(import.meta.dirname, 'src', 'ui')`
///   (rejecting `process.cwd()` which depends on launch cwd rather than the config file's dir!)
/// - `fileURLToPath(new URL('./src', import.meta.url))`
///
/// Returns `None` for dynamic, bare-identifier, `process.cwd()`, or absolute expressions so
/// callers fail closed.
pub(super) fn extract_vite_static_path_expr(rhs: &str) -> Option<String> {
    let trimmed = rhs.trim().trim_end_matches(';').trim();
    if let Some(inner) = unquote_single_js_literal(trimmed) {
        if inner.starts_with('/') {
            return None;
        }
        let v = inner.trim_start_matches("./");
        return (!v.is_empty()).then(|| v.to_string());
    }
    if trimmed.contains("resolve(") || trimmed.contains("join(") {
        let open_idx = trimmed.find('(')?;
        let close_idx = trimmed.rfind(')')?;
        if close_idx <= open_idx {
            return None;
        }
        let inner = &trimmed[open_idx + 1..close_idx];
        let mut joined: Vec<String> = Vec::new();
        for (idx, raw_arg) in inner.split(',').enumerate() {
            let arg = raw_arg.trim();
            if arg.is_empty() {
                continue;
            }
            if idx == 0 && matches!(arg, "__dirname" | "import.meta.dirname") {
                continue;
            }
            let raw_seg = unquote_single_js_literal(arg)?;
            if raw_seg.starts_with('/') {
                return None;
            }
            let seg = raw_seg.trim_start_matches("./").trim_end_matches('/');
            if !seg.is_empty() {
                joined.push(seg.to_string());
            }
        }
        if joined.is_empty() {
            return None;
        }
        return Some(joined.join("/"));
    }
    if let Some((_, after)) = trimmed.split_once("new URL(") {
        let first_arg = after.split(',').next().unwrap_or("").trim();
        if let Some(raw_s) = unquote_single_js_literal(first_arg) {
            if raw_s.starts_with('/') {
                return None;
            }
            let s = raw_s.trim_start_matches("./").trim_end_matches('/');
            return (!s.is_empty()).then(|| s.to_string());
        }
    }
    None
}

pub(super) fn parse_manifest_metadata(text: &str, file_name: &str) -> ParsedPythonFile {
    let mut defined_symbols = BTreeSet::new();
    let mut module_markers = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    if file_name == "package.json" {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(text) {
            let pkg_name = val
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            // Always record legacy `main`, `module`, and string `browser` entrypoints for
            // directory imports (`require('./lib')` -> `lib/package.json` `main`), even when the
            // directory manifest omits `"name"`. Distinguish `"main"` (`fe_dir_main_exact:`) from
            // bundler-only `"module"`/`"browser"` so Node CJS `index.js` fallback is preserved!
            for entry_field in ["main", "module", "browser"] {
                if let Some(entry_val) = val
                    .get(entry_field)
                    .and_then(serde_json::Value::as_str)
                    .map(|s| s.trim().trim_start_matches("./"))
                    .filter(|s| !s.is_empty())
                {
                    if entry_field == "main" {
                        defined_symbols.insert(format!("fe_dir_main_exact:{entry_val}"));
                    }
                    defined_symbols.insert(format!("fe_dir_main:{entry_val}"));
                    if let Some(ref name) = pkg_name {
                        defined_symbols.insert(format!("fe_pkg_main:{name}->{entry_val}"));
                    }
                }
            }
            if let Some(ref name) = pkg_name {
                defined_symbols.insert(format!("fe_pkg:{name}"));
            }
            if let Some(ts_entry) = val
                .get("tsconfig")
                .and_then(serde_json::Value::as_str)
                .map(|s| s.trim().trim_start_matches("./"))
                .filter(|s| !s.is_empty())
            {
                defined_symbols.insert(format!("fe_pkg_tsconfig:{ts_entry}"));
                for cand in tsconfig_path_variants(ts_entry) {
                    data_refs.insert(cand);
                }
            }
            // Parse Node.js Subpath Imports (`"imports": { "#utils/*": "./src/utils/*.js" }`)
            // preserving the full wildcard pattern (with suffix!) for accurate substitution.
            if let Some(imports_map) = val.get("imports").and_then(serde_json::Value::as_object) {
                for (sub_key, sub_val) in imports_map {
                    if sub_key.starts_with('#') {
                        for target in extract_package_export_targets(sub_val) {
                            defined_symbols.insert(format!("fe_subpath:{sub_key}->{target}"));
                        }
                    }
                }
            }
            // Parse package `exports` map (`".": "./src/index.ts"`, `"./button": "./src/components/Button.tsx"`,
            // root string `"./actual.js"`, root array `["./actual.js"]`, or `"exports": {}`).
            // Note: in Node.js, `"exports": null` is equivalent to omitting `"exports"` and falls
            // back to `"main"`, so ignore explicit null here!
            if let Some(ref name) = pkg_name
                && let Some(exports_val) = val.get("exports").filter(|v| !v.is_null())
            {
                // Always record that `"exports"` is declared on this package so Node's strict
                // `exports` precedence overrides legacy `main` even for empty/null/array forms!
                defined_symbols.insert(format!("fe_has_exports:{name}"));
                if let Some(s) = exports_val.as_str() {
                    let clean = s.trim().trim_start_matches("./");
                    if !clean.is_empty() {
                        defined_symbols.insert(format!("fe_export:{name}:.->{clean}"));
                    }
                } else if exports_val.is_array() {
                    for root_target in extract_package_export_targets(exports_val) {
                        let clean_target = root_target.trim_start_matches("./").to_string();
                        defined_symbols.insert(format!("fe_export:{name}:.->{clean_target}"));
                        if (clean_target.ends_with(".json") || clean_target.ends_with(".jsonc"))
                            && !clean_target.contains('*')
                        {
                            data_refs.insert(clean_target);
                        }
                    }
                } else if let Some(exports_map) = exports_val.as_object() {
                    let has_subpath_keys = exports_map
                        .keys()
                        .any(|k| k == "." || k.starts_with("./"));
                    if has_subpath_keys {
                        for (exp_key, exp_val) in exports_map {
                            let clean_sub = if exp_key == "." {
                                ".".to_string()
                            } else {
                                exp_key.trim_start_matches("./").to_string()
                            };
                            for target in extract_package_export_targets(exp_val) {
                                let clean_target = target.trim_start_matches("./").to_string();
                                defined_symbols.insert(format!(
                                    "fe_export:{name}:{clean_sub}->{clean_target}"
                                ));
                                if (clean_target.ends_with(".json")
                                    || clean_target.ends_with(".jsonc"))
                                    && !clean_target.contains('*')
                                {
                                    data_refs.insert(clean_target);
                                }
                            }
                        }
                    } else {
                        for root_target in extract_package_export_targets(exports_val) {
                            let clean_target = root_target.trim_start_matches("./").to_string();
                            defined_symbols.insert(format!("fe_export:{name}:.->{clean_target}"));
                            if (clean_target.ends_with(".json") || clean_target.ends_with(".jsonc"))
                                && !clean_target.contains('*')
                            {
                                data_refs.insert(clean_target);
                            }
                        }
                    }
                }
            }
        }
        return ParsedPythonFile {
            defined_symbols: defined_symbols.into_iter().collect(),
            data_refs: data_refs.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }
    if (file_name.starts_with("tsconfig") || file_name.starts_with("jsconfig"))
        && (file_name.ends_with(".json") || file_name.ends_with(".jsonc"))
    {
        return parse_tsconfig_jsonc(text);
    }
    if file_name.starts_with("vite.config.")
        || file_name.starts_with("vitest.config.")
        || file_name.starts_with("vitest.workspace.")
        || file_name.starts_with("jest.config.")
    {
        let mut parsed = parse_frontend_source(text);
        // Helper: record BOTH exact alias (`@shared->shared`) AND subpath prefix (`@shared/->shared/`)
        // using a dedicated `fe_vite_alias:` / `fe_vite_alias_cwd:` tag so TypeScript `baseUrl`
        // never redirects Vite aliases and cwd-relative `path.resolve('./generated')` in nested
        // configs is distinguished from config-dir-relative `path.resolve(__dirname, './src')`!
        let emit_vite_alias =
            |raw_key: &str, target: &str, is_dir_rel: bool, syms: &mut Vec<String>| {
                let clean_k = raw_key.trim_end_matches('/');
                let clean_t = target.trim_end_matches('/');
                if !clean_k.is_empty() && !clean_t.is_empty() {
                    let tag = if is_dir_rel {
                        "fe_vite_alias"
                    } else {
                        "fe_vite_alias_cwd"
                    };
                    syms.push(format!("{tag}:{clean_k}->{clean_t}"));
                    syms.push(format!("{tag}:{clean_k}/->{clean_t}/"));
                }
            };
        // Strip JS comments (respecting `'...'`, `"..."`, and backtick template literals) so
        // inline `defineConfig({ base: `https://example/`, root: './client' })`, quoted property
        // keys (`'root' : './client'`), and spread configs (`{ ...shared }`) parse accurately.
        let cleaned_cfg = strip_js_comments_preserving_newlines(text);
        if cleaned_cfg.contains("...") {
            // Spread object in Vite config (`defineConfig({ ...shared })`) can supply arbitrary
            // `alias`, `root`, or `envDir` from another module -> fail closed for affected FE tests.
            parsed.defined_symbols.push("fe_dynamic_import".to_string());
        }
        // Extract property value at a specific object nesting depth (e.g. `top_level_only = true`
        // for `root` / `envDir` so a nested `alias: { root: ... }` never masquerades as Vite's
        // top-level `root`!).
        let extract_prop_rhs_scoped = |src: &str, prop_name: &str, top_level_only: bool| -> Option<String> {
            let bytes = src.as_bytes();
            let len = bytes.len();
            let mut brace_depth = 0usize;
            let mut in_q: Option<u8> = None;
            let mut idx = 0usize;
            while idx < len {
                if let Some(q) = in_q {
                    if bytes[idx] == b'\\' && idx + 1 < len {
                        idx += 1 + src[idx + 1..].chars().next().map_or(1, char::len_utf8);
                        continue;
                    }
                    if bytes[idx] == q {
                        in_q = None;
                    }
                    idx += src[idx..].chars().next().map_or(1, char::len_utf8);
                    continue;
                }
                match bytes[idx] {
                    b'{' => {
                        brace_depth += 1;
                        idx += 1;
                        continue;
                    }
                    b'}' => {
                        brace_depth = brace_depth.saturating_sub(1);
                        idx += 1;
                        continue;
                    }
                    b'`' => {
                        in_q = Some(b'`');
                        idx += 1;
                        continue;
                    }
                    _ => {}
                }
                // Check for bare `prop_name` or quoted `'prop_name'` / `"prop_name"` at allowed depth
                // (always slicing on UTF-8 char boundaries so Unicode identifiers like `café` never panic!).
                let depth_ok = !top_level_only || brace_depth <= 1;
                if depth_ok && src.is_char_boundary(idx) {
                    let quoted_match = matches!(bytes[idx], b'\'' | b'"')
                        && idx + 1 + prop_name.len() < len
                        && src.is_char_boundary(idx + 1)
                        && src[idx + 1..].starts_with(prop_name)
                        && bytes[idx + 1 + prop_name.len()] == bytes[idx];
                    let bare_match = !matches!(bytes[idx], b'\'' | b'"')
                        && src[idx..].starts_with(prop_name)
                        && (idx == 0
                            || (!bytes[idx - 1].is_ascii_alphanumeric() && bytes[idx - 1] != b'_'));
                    if quoted_match || bare_match {
                        let mut colon_pos = if quoted_match {
                            idx + 2 + prop_name.len()
                        } else {
                            idx + prop_name.len()
                        };
                        while colon_pos < len && bytes[colon_pos].is_ascii_whitespace() {
                            colon_pos += 1;
                        }
                        if colon_pos < len && bytes[colon_pos] == b':' {
                            let after = &src[colon_pos + 1..];
                            let ab = after.as_bytes();
                            let mut depth = 0usize;
                            let mut q: Option<u8> = None;
                            let mut end = after.len();
                            let mut j = 0usize;
                            while j < ab.len() {
                                if let Some(qc) = q {
                                    if ab[j] == b'\\' && j + 1 < ab.len() {
                                        j += 1 + after[j + 1..].chars().next().map_or(1, char::len_utf8);
                                        continue;
                                    }
                                    if ab[j] == qc {
                                        q = None;
                                    }
                                    j += after[j..].chars().next().map_or(1, char::len_utf8);
                                    continue;
                                }
                                match ab[j] {
                                    b'\'' | b'"' | b'`' => q = Some(ab[j]),
                                    b'(' | b'[' | b'{' => depth += 1,
                                    b')' | b']' | b'}' => {
                                        if depth == 0 {
                                            end = j;
                                            break;
                                        }
                                        depth -= 1;
                                    }
                                    b',' | b';' | b'\n' if depth == 0 => {
                                        end = j;
                                        break;
                                    }
                                    _ => {}
                                }
                                j += after[j..].chars().next().map_or(1, char::len_utf8);
                            }
                            let val = after[..end].trim();
                            if !val.is_empty() {
                                return Some(val.to_string());
                            }
                        }
                    }
                }
                if matches!(bytes[idx], b'\'' | b'"') {
                    in_q = Some(bytes[idx]);
                    idx += 1;
                } else {
                    idx += src[idx..].chars().next().map_or(1, char::len_utf8);
                }
            }
            None
        };
        let extract_prop_rhs = |src: &str, prop_name: &str| -> Option<String> {
            extract_prop_rhs_scoped(src, prop_name, false)
        };
        // Only treat `path.resolve(__dirname, ...)` / `import.meta.dirname` / `new URL(..., import.meta.url)`
        // as config-dir-relative; bare `path.resolve('app')` (without `__dirname`) is cwd-relative!
        let expr_is_config_dir_relative = |expr: &str| -> bool {
            expr.contains("__dirname")
                || expr.contains("import.meta.dirname")
                || expr.contains("new URL(")
        };
        if let Some(root_rhs) = extract_prop_rhs_scoped(&cleaned_cfg, "root", true) {
            let root_trimmed = root_rhs.trim();
            if let Some(rv) = extract_vite_static_path_expr(root_trimmed) {
                if expr_is_config_dir_relative(root_trimmed) {
                    parsed.defined_symbols.push(format!("fe_vite_root:{rv}"));
                } else {
                    parsed.defined_symbols.push(format!("fe_vite_root_cwd:{rv}"));
                }
            } else {
                parsed.defined_symbols.push("fe_dynamic_import".to_string());
            }
        }
        if let Some(env_dir_rhs) = extract_prop_rhs_scoped(&cleaned_cfg, "envDir", true)
            && env_dir_rhs != "false"
        {
            let trimmed_ed = env_dir_rhs.trim();
            if let Some(ed) = extract_vite_static_path_expr(trimmed_ed) {
                if expr_is_config_dir_relative(trimmed_ed) {
                    parsed.defined_symbols.push(format!("fe_vite_env_dir_cfg:{ed}"));
                } else {
                    parsed.defined_symbols.push(format!("fe_vite_env_dir:{ed}"));
                }
            } else {
                parsed.defined_symbols.push("fe_dynamic_env".to_string());
            }
        }
        if let Some(alias_rhs) = extract_prop_rhs(&cleaned_cfg, "alias") {
            let trimmed_a = alias_rhs.trim();
            if let Some(after_brace) = trimmed_a.strip_prefix('{')
                && let Some(brace_end) = after_brace.rfind('}')
            {
                let obj_body = after_brace[..brace_end].trim();
                let mut entries: Vec<&str> = Vec::new();
                let mut depth = 0usize;
                let mut q: Option<u8> = None;
                let mut start_e = 0usize;
                let ob = obj_body.as_bytes();
                for idx in 0..ob.len() {
                    if let Some(qc) = q {
                        if ob[idx] == qc && (idx == 0 || ob[idx - 1] != b'\\') {
                            q = None;
                        }
                        continue;
                    }
                    match ob[idx] {
                        b'\'' | b'"' => q = Some(ob[idx]),
                        b'(' | b'[' | b'{' => depth += 1,
                        b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                        b',' if depth == 0 => {
                            entries.push(&obj_body[start_e..idx]);
                            start_e = idx + 1;
                        }
                        _ => {}
                    }
                }
                entries.push(&obj_body[start_e..]);
                for entry in entries {
                    let e = entry.trim();
                    if e.is_empty() {
                        continue;
                    }
                    if let Some((ek, ev)) = e.split_once(':') {
                        let k = ek.trim().trim_matches(['"', '\'', ' ']);
                        let ev_trimmed = ev.trim();
                        if let Some(v) = extract_vite_static_path_expr(ev_trimmed) {
                            emit_vite_alias(
                                k,
                                &v,
                                expr_is_config_dir_relative(ev_trimmed),
                                &mut parsed.defined_symbols,
                            );
                        } else {
                            parsed.defined_symbols.push("fe_dynamic_import".to_string());
                        }
                    } else {
                        parsed.defined_symbols.push("fe_dynamic_import".to_string());
                    }
                }
            } else if let Some(after_bracket) = trimmed_a.strip_prefix('[')
                && let Some(bracket_end) = after_bracket.rfind(']')
            {
                let arr_body = after_bracket[..bracket_end].trim();
                let mut items: Vec<&str> = Vec::new();
                let mut depth = 0usize;
                let mut q: Option<u8> = None;
                let mut start_i = 0usize;
                let ab = arr_body.as_bytes();
                for idx in 0..ab.len() {
                    if let Some(qc) = q {
                        if ab[idx] == qc && (idx == 0 || ab[idx - 1] != b'\\') {
                            q = None;
                        }
                        continue;
                    }
                    match ab[idx] {
                        b'\'' | b'"' => q = Some(ab[idx]),
                        b'(' | b'[' | b'{' => depth += 1,
                        b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                        b',' if depth == 0 => {
                            items.push(&arr_body[start_i..idx]);
                            start_i = idx + 1;
                        }
                        _ => {}
                    }
                }
                items.push(&arr_body[start_i..]);
                for item in items {
                    let it = item.trim();
                    if it.is_empty() {
                        continue;
                    }
                    if let Some(find_rhs) = extract_prop_rhs(it, "find")
                        && let Some(rep_rhs) = extract_prop_rhs(it, "replacement")
                        && let Some(find_key) = unquote_single_js_literal(&find_rhs)
                        && let Some(v) = extract_vite_static_path_expr(&rep_rhs)
                    {
                        emit_vite_alias(
                            find_key,
                            &v,
                            expr_is_config_dir_relative(&rep_rhs),
                            &mut parsed.defined_symbols,
                        );
                    } else {
                        parsed.defined_symbols.push("fe_dynamic_import".to_string());
                    }
                }
            } else if !trimmed_a.is_empty() {
                // Dynamic `alias: aliases` variable or function call -> fail closed!
                parsed.defined_symbols.push("fe_dynamic_import".to_string());
            }
        }
        // Parse Vitest/Jest configured `setupFiles` and `setupFilesAfterEnv` (single string or
        // array of static paths, supporting `<rootDir>/` Jest prefix) and attach them directly as
        // config imports so editing a configured setup file or any module it imports invalidates
        // all governed frontend tests; fail closed if dynamic!
        for setup_prop in ["setupFiles", "setupFilesAfterEnv"] {
            if let Some(setup_rhs) = extract_prop_rhs(&cleaned_cfg, setup_prop) {
                let trimmed_s = setup_rhs.trim();
                let add_setup_entry = |raw_expr: &str, parsed: &mut ParsedPythonFile| {
                    let norm_expr = raw_expr.trim().replace("<rootDir>/", "./");
                    if let Some(p) = extract_vite_static_path_expr(&norm_expr) {
                        let rel_mod = if p.starts_with("./") || p.starts_with("../") {
                            p
                        } else {
                            format!("./{p}")
                        };
                        let imp = ImportSpec {
                            level: 0,
                            module: rel_mod,
                            symbols: Vec::new(),
                        };
                        if !parsed.imports.contains(&imp) {
                            parsed.imports.push(imp);
                        }
                    } else if !norm_expr.is_empty() {
                        parsed.defined_symbols.push("fe_dynamic_import".to_string());
                    }
                };
                if let Some(after_b) = trimmed_s.strip_prefix('[')
                    && let Some(end_b) = after_b.rfind(']')
                {
                    let inner = after_b[..end_b].trim();
                    let mut depth = 0usize;
                    let mut q: Option<u8> = None;
                    let mut start_s = 0usize;
                    let sb = inner.as_bytes();
                    let mut k = 0usize;
                    while k < sb.len() {
                        if let Some(qc) = q {
                            if sb[k] == b'\\' && k + 1 < sb.len() {
                                k += 1 + inner[k + 1..].chars().next().map_or(1, char::len_utf8);
                                continue;
                            }
                            if sb[k] == qc {
                                q = None;
                            }
                            k += inner[k..].chars().next().map_or(1, char::len_utf8);
                            continue;
                        }
                        match sb[k] {
                            b'\'' | b'"' | b'`' => q = Some(sb[k]),
                            b'(' | b'[' | b'{' => depth += 1,
                            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                            b',' if depth == 0 => {
                                add_setup_entry(&inner[start_s..k], &mut parsed);
                                start_s = k + 1;
                            }
                            _ => {}
                        }
                        k += inner[k..].chars().next().map_or(1, char::len_utf8);
                    }
                    if start_s < inner.len() {
                        add_setup_entry(&inner[start_s..], &mut parsed);
                    }
                } else if !trimmed_s.is_empty() {
                    add_setup_entry(trimmed_s, &mut parsed);
                }
            }
        }
        for lit in &parsed.data_refs {
            data_refs.insert(lit.clone());
        }
        parsed.data_refs = data_refs.into_iter().collect();
        return parsed;
    }
    pub(super) fn strip_toml_comment_outside_quotes(raw: &str) -> &str {
        let bytes = raw.as_bytes();
        let mut i = 0usize;
        let mut quote: Option<u8> = None;
        while i < bytes.len() {
            if let Some(q) = quote {
                if q == b'"' && bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == q {
                    quote = None;
                }
            } else if bytes[i] == b'"' || bytes[i] == b'\'' {
                quote = Some(bytes[i]);
            } else if bytes[i] == b'#' {
                return &raw[..i];
            }
            i += 1;
        }
        raw
    }
    let mut section = String::new();
    for raw in text.lines() {
        let line = strip_toml_comment_outside_quotes(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(['[', ']']).trim().to_string();
            if file_name == "pyproject.toml" && section == "tool.maturin" {
                module_markers.insert("maturin".to_string());
            }
            continue;
        }
        if file_name == "Cargo.toml" {
            if matches!(section.as_str(), "package" | "lib")
                && let Some(rest) = line.strip_prefix("name")
                && let Some(eq_rest) = rest.trim_start().strip_prefix('=')
            {
                let val = eq_rest.trim().trim_matches(['"', '\'']);
                if !val.is_empty() {
                    let norm_crate = val.replace('-', "_");
                    defined_symbols.insert(norm_crate.clone());
                    if section == "lib" {
                        defined_symbols.insert(format!("pyo3:{norm_crate}"));
                    }
                }
            }
            if section == "package"
                && let Some(rest) = line.strip_prefix("build")
                && let Some(eq_rest) = rest.trim_start().strip_prefix('=')
            {
                let raw_val = eq_rest.trim();
                if raw_val == "false" {
                    defined_symbols.insert("cargo_build_script:false".to_string());
                } else {
                    let clean = raw_val
                        .trim_matches(['"', '\''])
                        .trim_start_matches("./")
                        .replace('\\', "/");
                    if !clean.is_empty() {
                        defined_symbols.insert(format!("cargo_build_script:{clean}"));
                        data_refs.insert(clean);
                    }
                }
            }
            if section == "lib" && line.contains("cdylib") {
                module_markers.insert("cdylib".to_string());
            }
            if section == "lib" && line.contains("proc-macro") && line.contains("true") {
                module_markers.insert("proc_macro".to_string());
            }
        } else if file_name == "pyproject.toml" {
            if section == "tool.maturin"
                && let Some(rest) = line.strip_prefix("module-name")
                && let Some(eq_rest) = rest.trim_start().strip_prefix('=')
            {
                let val = eq_rest.trim().trim_matches(['"', '\'']);
                if !val.is_empty() {
                    defined_symbols.insert(format!("pyo3:{val}"));
                    if let Some(last) = val.rsplit('.').next() {
                        defined_symbols.insert(format!("pyo3:{last}"));
                    }
                }
            } else if section == "project"
                && let Some(rest) = line.strip_prefix("name")
                && let Some(eq_rest) = rest.trim_start().strip_prefix('=')
            {
                let val = eq_rest.trim().trim_matches(['"', '\'']).replace('-', "_");
                if !val.is_empty() {
                    defined_symbols.insert(val);
                }
            }
        }
    }
    ParsedPythonFile {
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}


