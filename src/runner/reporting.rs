use super::*;

/// Extracts structured failure locations (`file:line`, `assertion`, and noise-folded stack frames) from test output.
pub fn extract_structured_failures(
    test_file: &str,
    failed_nodeids: &[String],
    output: &str,
) -> Vec<StructuredFailure> {
    let mut failures = Vec::new();
    let user_frames = fold_stack_trace_noise(output);
    let mut detected_file = test_file.to_string();
    let mut detected_line = 1usize;
    let mut detected_msg = String::new();

    for raw_line in output.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("E   ") && detected_msg.is_empty() {
            detected_msg = trimmed.trim_start_matches("E   ").trim().to_string();
        } else if (trimmed.contains("AssertionError")
            || trimmed.contains("panicked at")
            || trimmed.contains("AssertionFailed"))
            && detected_msg.is_empty()
        {
            detected_msg = trimmed.to_string();
        }
        if let Some((left, right)) = trimmed.split_once(':')
            && (left.ends_with(".py")
                || left.ends_with(".rs")
                || left.ends_with(".ts")
                || left.ends_with(".tsx")
                || left.ends_with(".js")
                || left.ends_with(".go"))
            && !left.contains("node_modules")
            && !left.contains("site-packages")
        {
            let line_str = right.split(':').next().unwrap_or("");
            if let Ok(line_num) = line_str.trim().parse::<usize>() {
                detected_file = left
                    .trim_start_matches("-->")
                    .trim()
                    .trim_start_matches("./")
                    .to_string();
                detected_line = line_num;
            }
        }
    }
    if detected_msg.is_empty() {
        detected_msg = "Test assertion failed".to_string();
    }
    if failed_nodeids.is_empty() {
        failures.push(StructuredFailure {
            test_file: test_file.to_string(),
            nodeid: test_file.to_string(),
            file: detected_file,
            line: detected_line,
            message: detected_msg,
            user_frames,
        });
    } else {
        for nid in failed_nodeids {
            failures.push(StructuredFailure {
                test_file: test_file.to_string(),
                nodeid: nid.clone(),
                file: detected_file.clone(),
                line: detected_line,
                message: detected_msg.clone(),
                user_frames: user_frames.clone(),
            });
        }
    }
    failures
}

/// Folds external library / runtime stack frames (`node_modules`, `site-packages`, `tokio::runtime`, `std::panicking`)
/// and keeps only workspace-local user frames.
pub fn fold_stack_trace_noise(output: &str) -> Vec<String> {
    let mut frames = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.contains("node_modules/")
            || trimmed.contains("site-packages/")
            || trimmed.contains("_pytest/")
            || trimmed.contains("tokio::runtime")
            || trimmed.contains("std::panicking")
            || trimmed.contains("node:internal/")
        {
            continue;
        }
        if (trimmed.contains(".py:")
            || trimmed.contains(".rs:")
            || trimmed.contains(".ts:")
            || trimmed.contains(".tsx:")
            || trimmed.contains(".go:"))
            && frames.len() < 8
        {
            frames.push(trimmed.to_string());
        }
    }
    frames
}

pub fn extract_missing_prerequisites(output: &str) -> Vec<String> {
    let mut prereqs = BTreeSet::new();
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("skip")
            && (lower.contains("env")
                || lower.contains("url")
                || lower.contains("database")
                || lower.contains("redis")
                || lower.contains("docker")
                || lower.contains("token")
                || lower.contains("key")
                || lower.contains("requires"))
        {
            prereqs.insert(line.trim().to_string());
        }
    }
    prereqs.into_iter().take(8).collect()
}

pub fn clean_path_or_target(raw: &str) -> String {
    let mut s = raw.trim();
    while s.len() >= 2
        && ((s.starts_with('`') && s.ends_with('`'))
            || (s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s = s[1..s.len() - 1].trim();
    }
    if s.starts_with('[')
        && let Some((_, rest)) = s.split_once("](")
        && let Some(target) = rest.strip_suffix(')')
    {
        s = target.trim();
        if s.starts_with('<') && s.ends_with('>') && s.len() >= 2 {
            s = s[1..s.len() - 1].trim();
        }
    }
    if let Some(rest) = s.strip_prefix("file://") {
        let lower = rest.to_ascii_lowercase();
        if (lower.starts_with("wsl.localhost/") || lower.starts_with("wsl$/"))
            && let Some((_, after_host)) = rest.split_once('/')
            && let Some((_, distro_rel)) = after_host.split_once('/')
        {
            return format!("/{distro_rel}");
        }
        s = rest;
    }
    let norm = s.replace('\\', "/");
    let lower_norm = norm.to_ascii_lowercase();
    if (lower_norm.starts_with("//wsl.localhost/") || lower_norm.starts_with("//wsl$/"))
        && let Some(after_prefix) = norm.strip_prefix("//")
        && let Some((_, after_host)) = after_prefix.split_once('/')
        && let Some((_, distro_rel)) = after_host.split_once('/')
    {
        return format!("/{distro_rel}");
    }
    s.to_string()
}

pub(super) fn is_line_or_anchor_fragment(base: &str, frag: &str) -> bool {
    if frag.is_empty() {
        return false;
    }
    // Line anchors like `#L10`, `#L10-L20`, `#L10C3-L20C8`, `#42`, `#42-50`.
    let is_line_anchor = frag.split('-').enumerate().all(|(idx, part)| {
        if idx > 1 || part.is_empty() {
            return false;
        }
        let rest = part.strip_prefix('L').unwrap_or(part);
        match rest.split_once('C') {
            Some((line, col)) => {
                !line.is_empty()
                    && !col.is_empty()
                    && line.bytes().all(|b| b.is_ascii_digit())
                    && col.bytes().all(|b| b.is_ascii_digit())
            }
            None => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        }
    });
    if is_line_anchor {
        return true;
    }
    // Doc/symbol anchor after a file extension (e.g. `guide.md#intro`, `mod.ts#fn`), never
    // stripping literal `#` inside directory/file names like `/repo#one/a.ts` or `a#b.ts`.
    if frag.contains('/') || frag.contains('\\') || frag.contains('.') {
        return false;
    }
    if let Some((_, ext)) = base.rsplit_once('.') {
        !ext.is_empty()
            && ext.len() <= 12
            && !ext.contains('/')
            && !ext.contains('\\')
            && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    } else {
        false
    }
}

pub fn strip_line_or_fragment_suffix(file_part: &str) -> &str {
    let without_frag = match file_part.rsplit_once('#') {
        Some((base, frag)) if !base.is_empty() && is_line_or_anchor_fragment(base, frag) => base,
        _ => file_part,
    };
    if let Some((base, suffix)) = without_frag.split_once(':')
        && base.len() > 1
        && !base.contains("://")
        && suffix.as_bytes().first().is_some_and(u8::is_ascii_digit)
    {
        return base;
    }
    without_frag
}

/// Splits a target string that may contain `::` in EITHER the literal filename/directory (e.g.
/// `tests/foo::bar.py`, `tests/a::b`, or `tests/foo::bar.py::test_a`) OR the node selector by
/// preferring the longest prefix that exists on disk under `root` (as a file OR directory).
pub(super) fn split_target_file_and_node_on_disk<'a>(root: &Path, trimmed: &'a str) -> (&'a str, Option<&'a str>) {
    if !trimmed.contains("::") {
        return (trimmed, None);
    }
    let exists_on_disk = |cand: &str| -> bool {
        if cand.is_empty() {
            return false;
        }
        let p = Path::new(cand);
        if p.is_absolute() {
            p.exists()
        } else {
            root.join(cand.trim_start_matches("./")).exists()
        }
    };
    if exists_on_disk(trimmed) {
        return (trimmed, None);
    }
    let mut split_positions = Vec::new();
    let mut pos = 0usize;
    while let Some(rel_idx) = trimmed[pos..].find("::") {
        let abs_idx = pos + rel_idx;
        split_positions.push(abs_idx);
        pos = abs_idx + 2;
    }
    for &idx in split_positions.iter().rev() {
        let cand = trimmed[..idx].trim();
        if exists_on_disk(cand) {
            return (cand, Some(trimmed[idx + 2..].trim()));
        }
    }
    match trimmed.split_once("::") {
        Some((f, n)) => (f.trim(), Some(n.trim())),
        None => (trimmed, None),
    }
}

pub fn normalize_target_for_root(root: &Path, raw: &str) -> String {
    let cleaned = clean_path_or_target(raw);
    let trimmed = cleaned.trim();
    let (raw_file_part, node_part) = split_target_file_and_node_on_disk(root, trimmed);
    let file_part = strip_line_or_fragment_suffix(raw_file_part);
    let norm_slashes = file_part.replace('\\', "/");
    let root_str = root.to_string_lossy().replace('\\', "/");
    let root_trimmed = root_str.trim_end_matches('/');
    let root_prefix = format!("{root_trimmed}/");
    let mut rel_file = if norm_slashes == root_trimmed
        || norm_slashes == "."
        || norm_slashes == "./"
    {
        ".".to_string()
    } else if let Some(stripped) = norm_slashes.strip_prefix(&root_prefix) {
        let s = stripped.trim_start_matches("./");
        if s.is_empty() {
            ".".to_string()
        } else {
            s.to_string()
        }
    } else {
        let s = norm_slashes.trim_start_matches("./");
        if s.is_empty() && !norm_slashes.is_empty() {
            ".".to_string()
        } else {
            s.to_string()
        }
    };
    if !rel_file.starts_with('/')
        && let Some(root_name) = root.file_name().and_then(|n| n.to_str())
        && !root_name.is_empty()
        && let Some(stripped_base) = rel_file.strip_prefix(&format!("{root_name}/"))
        && !stripped_base.is_empty()
        // Never strip `{root_name}/` when `stripped_base` would escape `root` (e.g. `repo/../other/test.py`)
        // OR when the unstripped path itself exists directly under `root`.
        && crate::ast_graph::normalize_lexical_rel(Path::new(stripped_base)).is_some()
        && !root.join(&rel_file).exists()
    {
        rel_file = stripped_base.to_string();
    }
    match node_part {
        Some(node) if !node.is_empty() => format!("{rel_file}::{node}"),
        _ => rel_file,
    }
}

pub fn normalize_targets_for_root(root: &Path, raw_targets: &[String]) -> Vec<String> {
    raw_targets
        .iter()
        .map(|raw| normalize_target_for_root(root, raw))
        .collect()
}

pub fn compact_passed_delta_stdout(raw: &str) -> String {
    let mut kept = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if (trimmed.starts_with("test ") && trimmed.ends_with(" ... ok"))
            || (trimmed.starts_with("running ") && trimmed.ends_with(" tests"))
            || trimmed == "running 1 test"
            || trimmed.starts_with("test result: ok.")
            || trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Finished `")
            || trimmed.starts_with("Running unittests ")
            || trimmed.starts_with("Running tests/")
            || trimmed.starts_with("Doc-tests ")
            || (trimmed.starts_with("=== ")
                && trimmed.ends_with(" ===")
                && trimmed.contains(" passed"))
            || trimmed.chars().all(|c| c == '.' || c == ' ')
        {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

pub fn extract_warnings_summary(output: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if (trimmed.contains("DeprecationWarning:")
            || trimmed.contains("UserWarning:")
            || trimmed.contains("RuntimeWarning:")
            || trimmed.contains("Warning: An update to ")
            || trimmed.starts_with("warning:"))
            && warnings.len() < 10
            && !warnings.contains(&trimmed.to_string())
        {
            warnings.push(trimmed.to_string());
        }
    }
    warnings
}

pub(super) fn escape_js_regex_literal(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len() + 8);
    for ch in literal.chars() {
        if matches!(
            ch,
            '\\' | '^' | '$' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '/'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

pub(super) fn format_xml_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Formats a `TestRunResponse` as a standard JUnit XML report (`--report-format=junit`).
pub fn format_junit_xml_report(resp: &TestRunResponse) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    xml.push_str(&format!(
        "<testsuites tests=\"{}\" failures=\"{}\" skipped=\"{}\" errors=\"{}\" time=\"{:.3}\">\n",
        resp.passed + resp.failed + resp.skipped + resp.errors,
        resp.failed,
        resp.skipped,
        resp.errors,
        resp.duration_ms as f64 / 1000.0
    ));
    for f in resp.cached_files.iter().chain(resp.executed_files.iter()) {
        let escaped_file = format_xml_attr(&f.test_file);
        xml.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" errors=\"{}\" time=\"{:.3}\">\n",
            escaped_file,
            f.passed + f.failed + f.skipped + f.errors,
            f.failed,
            f.skipped,
            f.errors,
            f.duration_ms as f64 / 1000.0
        ));
        if f.failed_nodeids.is_empty() {
            match f.outcome {
                TestOutcome::Skipped => {
                    xml.push_str(&format!(
                        "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">\n      <skipped />\n    </testcase>\n",
                        escaped_file,
                        escaped_file,
                        f.duration_ms as f64 / 1000.0
                    ));
                }
                TestOutcome::Error => {
                    xml.push_str(&format!(
                        "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">\n      <error message=\"test error\" />\n    </testcase>\n",
                        escaped_file,
                        escaped_file,
                        f.duration_ms as f64 / 1000.0
                    ));
                }
                TestOutcome::Failed => {
                    xml.push_str(&format!(
                        "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">\n      <failure message=\"test failed\" />\n    </testcase>\n",
                        escaped_file,
                        escaped_file,
                        f.duration_ms as f64 / 1000.0
                    ));
                }
                TestOutcome::Passed => {
                    xml.push_str(&format!(
                        "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\" />\n",
                        escaped_file,
                        escaped_file,
                        f.duration_ms as f64 / 1000.0
                    ));
                }
            }
        } else {
            for nid in &f.failed_nodeids {
                let escaped_nid = format_xml_attr(nid);
                let tag = if f.outcome == TestOutcome::Error {
                    "<error message=\"test error\" />"
                } else {
                    "<failure message=\"test failed\" />"
                };
                xml.push_str(&format!(
                    "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">\n      {}\n    </testcase>\n",
                    escaped_file,
                    escaped_nid,
                    f.duration_ms as f64 / 1000.0,
                    tag
                ));
            }
        }
        xml.push_str("  </testsuite>\n");
    }
    xml.push_str("</testsuites>\n");
    xml
}

/// Formats a `TestRunResponse` (including Ruff/ESLint/Clippy diagnostics and structured test failures) as SARIF v2.1.0 JSON (`--report-format=sarif`).
pub fn format_sarif_report(resp: &TestRunResponse) -> serde_json::Value {
    let mut results = Vec::new();
    for diag in &resp.ruff_diagnostics {
        results.push(serde_json::json!({
            "ruleId": diag.code,
            "level": "warning",
            "message": { "text": diag.message },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": diag.file },
                    "region": { "startLine": diag.row.max(1), "startColumn": diag.column.max(1) }
                }
            }]
        }));
    }
    for fail in &resp.structured_failures {
        results.push(serde_json::json!({
            "ruleId": "tctx/test-failure",
            "level": "error",
            "message": { "text": format!("{}: {}", fail.nodeid, fail.message) },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": fail.file },
                    "region": { "startLine": fail.line.max(1), "startColumn": 1 }
                }
            }]
        }));
    }
    serde_json::json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "test-context",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            },
            "results": results,
        }]
    })
}

pub(super) fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[derive(Default, Debug)]
pub(super) struct FileXmlStats {
    pub(super) passed: usize,
    pub(super) failed: usize,
    pub(super) skipped: usize,
    pub(super) errors: usize,
    pub(super) duration_ms: u64,
    pub(super) failed_nodeids: Vec<String>,
    pub(super) node_results: BTreeMap<String, CachedNodeResult>,
    pub(super) dynamic_dependencies: BTreeSet<String>,
}

#[derive(Deserialize)]
pub(super) struct DirectReport {
    pub(super) nodeid: String,
    pub(super) path: String,
    pub(super) when: String,
    pub(super) outcome: String,
    pub(super) duration_ms: u64,
    pub(super) longrepr: String,
    #[serde(default)]
    pub(super) dynamic_deps: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct DirectPayload {
    pub(super) reports: Vec<DirectReport>,
    #[serde(default)]
    pub(super) dynamic_deps: Vec<String>,
    #[serde(default)]
    pub(super) shared_dynamic_deps: Vec<String>,
    #[serde(default)]
    pub(super) module_dynamic_deps: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub(super) dynamic_deps_incomplete: bool,
}

pub(super) type StructuredReportResult = (
    BTreeMap<String, FileXmlStats>,
    Vec<String>,
    Vec<String>,
    BTreeMap<String, Vec<String>>,
    bool,
);

pub(super) fn parse_structured_reports(
    bytes: &[u8],
    known_files: &BTreeMap<String, String>,
) -> StructuredReportResult {
    let Ok(payload) = serde_json::from_slice::<DirectPayload>(bytes) else {
        return (BTreeMap::new(), Vec::new(), Vec::new(), BTreeMap::new(), true);
    };
    let mut stats: BTreeMap<String, FileXmlStats> = BTreeMap::new();
    for report in payload.reports {
        // Prefer the longest matching `known_files` prefix on `report.nodeid` (or `report.path`)
        // so literal `::` filenames (e.g. `tests/foo::bar.py::test_a`) are attributed accurately.
        let file_hint = {
            let mut matched: Option<&str> = None;
            let nid = report.nodeid.as_str();
            if known_files.contains_key(nid) {
                matched = Some(nid);
            } else {
                let mut pos = 0usize;
                let mut splits = Vec::new();
                while let Some(rel) = nid[pos..].find("::") {
                    let abs = pos + rel;
                    splits.push(abs);
                    pos = abs + 2;
                }
                for &idx in splits.iter().rev() {
                    let cand = &nid[..idx];
                    if known_files.contains_key(cand) {
                        matched = Some(cand);
                        break;
                    }
                }
            }
            matched.unwrap_or_else(|| {
                if !report.path.is_empty() {
                    report.path.as_str()
                } else {
                    report.nodeid.split("::").next().unwrap_or("")
                }
            })
        };
        let Some(test_file) = match_testcase_to_file(Some(file_hint), "", known_files) else {
            continue;
        };
        let entry = stats.entry(test_file.clone()).or_default();
        entry
            .dynamic_dependencies
            .extend(report.dynamic_deps.iter().cloned());
        entry.duration_ms = entry.duration_ms.saturating_add(report.duration_ms);
        match report.outcome.as_str() {
            "passed" if report.when == "call" => {
                entry.passed += 1;
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Passed,
                        duration_ms: report.duration_ms,
                        output_snippet: String::new(),
                    },
                );
            }
            "skipped" => {
                entry.skipped += 1;
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Skipped,
                        duration_ms: report.duration_ms,
                        output_snippet: report.longrepr.clone(),
                    },
                );
            }
            "failed" if report.when == "call" => {
                entry.failed += 1;
                entry.failed_nodeids.push(report.nodeid.clone());
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Failed,
                        duration_ms: report.duration_ms,
                        output_snippet: report.longrepr.clone(),
                    },
                );
            }
            "failed" => {
                entry.errors += 1;
                entry.failed_nodeids.push(report.nodeid.clone());
                entry.node_results.insert(
                    report.nodeid.clone(),
                    CachedNodeResult {
                        nodeid: report.nodeid.clone(),
                        outcome: TestOutcome::Error,
                        duration_ms: report.duration_ms,
                        output_snippet: report.longrepr.clone(),
                    },
                );
            }
            _ => {}
        }
    }
    (
        stats,
        payload.dynamic_deps,
        payload.shared_dynamic_deps,
        payload.module_dynamic_deps,
        payload.dynamic_deps_incomplete,
    )
}

pub(super) fn find_xml_tag_end(xml: &str, start_idx: usize) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut i = start_idx;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_quote {
            if b == q {
                in_quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            in_quote = Some(b);
        } else if b == b'>' {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub(super) fn parse_junit_xml_per_file(
    xml: &str,
    known_files: &BTreeMap<String, String>,
) -> BTreeMap<String, FileXmlStats> {
    let mut stats: BTreeMap<String, FileXmlStats> = BTreeMap::new();
    if xml.is_empty() {
        return stats;
    }

    let mut pos = 0usize;
    while let Some(rel_start) = xml[pos..].find("<testcase ") {
        let start = pos + rel_start;
        let Some(tag_end) = find_xml_tag_end(xml, start) else {
            break;
        };
        let self_closing = xml[..tag_end].ends_with('/');
        let header = &xml[start..=tag_end];

        let file_attr = extract_xml_attr(header, "file");
        let classname_attr = extract_xml_attr(header, "classname").unwrap_or_default();
        let name_attr = extract_xml_attr(header, "name").unwrap_or_default();
        let time_secs: f64 = extract_xml_attr(header, "time")
            .and_then(|t| t.parse::<f64>().ok())
            .unwrap_or(0.0);

        let body = if self_closing {
            pos = tag_end + 1;
            ""
        } else if let Some(rel_close) = xml[tag_end + 1..].find("</testcase>") {
            let close_start = tag_end + 1 + rel_close;
            pos = close_start + "</testcase>".len();
            &xml[tag_end + 1..close_start]
        } else {
            pos = tag_end + 1;
            ""
        };

        let Some(matched_file) =
            match_testcase_to_file(file_attr.as_deref(), &classname_attr, known_files)
        else {
            continue;
        };

        let entry = stats.entry(matched_file.clone()).or_default();
        let case_ms = (time_secs * 1000.0).round() as u64;
        entry.duration_ms = entry.duration_ms.saturating_add(case_ms);

        if body.contains("<failure") {
            entry.failed += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.failed_nodeids.push(nodeid.clone());
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Failed,
                    duration_ms: case_ms,
                    output_snippet: body.chars().take(8192).collect(),
                },
            );
        } else if body.contains("<error") {
            entry.errors += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.failed_nodeids.push(nodeid.clone());
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Error,
                    duration_ms: case_ms,
                    output_snippet: body.chars().take(8192).collect(),
                },
            );
        } else if body.contains("<skipped") {
            entry.skipped += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Skipped,
                    duration_ms: case_ms,
                    output_snippet: String::new(),
                },
            );
        } else {
            entry.passed += 1;
            let nodeid = if name_attr.is_empty() {
                matched_file
            } else {
                format!("{matched_file}::{name_attr}")
            };
            entry.node_results.insert(
                nodeid.clone(),
                CachedNodeResult {
                    nodeid,
                    outcome: TestOutcome::Passed,
                    duration_ms: case_ms,
                    output_snippet: String::new(),
                },
            );
        }
    }

    stats
}

pub(super) fn unescape_xml_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    raw.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub(super) fn extract_xml_attr(tag: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{attr_name}=\"");
    let mut search_from = 0usize;
    while let Some(rel_idx) = tag[search_from..].find(&pattern) {
        let idx = search_from + rel_idx;
        if idx == 0 || tag.as_bytes()[idx - 1].is_ascii_whitespace() {
            let val_start = idx + pattern.len();
            let val_end = tag[val_start..].find('"')? + val_start;
            let raw_val = &tag[val_start..val_end];
            return Some(unescape_xml_entities(raw_val).replace('\\', "/"));
        }
        search_from = idx + pattern.len();
    }
    None
}

pub(super) fn path_matches_suffix_on_boundary(full: &str, suffix: &str) -> bool {
    if full == suffix {
        return true;
    }
    if full.len() > suffix.len() && full.ends_with(suffix) {
        return full.as_bytes()[full.len() - suffix.len() - 1] == b'/';
    }
    false
}

pub(super) fn match_testcase_to_file(
    file_attr: Option<&str>,
    classname: &str,
    known_files: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(f) = file_attr {
        let norm = f.trim_start_matches("./").replace('\\', "/");
        if known_files.contains_key(&norm) {
            return Some(norm);
        }
        let mut best: Option<(usize, &str)> = None;
        let mut ambiguous = false;
        for k in known_files.keys() {
            if path_matches_suffix_on_boundary(k, &norm)
                || path_matches_suffix_on_boundary(&norm, k)
            {
                let score = k.len().min(norm.len());
                match best {
                    Some((best_len, _)) if score > best_len => {
                        best = Some((score, k.as_str()));
                        ambiguous = false;
                    }
                    Some((best_len, best_k)) if score == best_len && best_k != k => {
                        ambiguous = true;
                    }
                    None => best = Some((score, k.as_str())),
                    _ => {}
                }
            }
        }
        if !ambiguous && let Some((_, matched)) = best {
            return Some(matched.to_string());
        }
    }
    if !classname.is_empty() {
        let parts: Vec<&str> = classname.split('.').collect();
        for len in (1..=parts.len()).rev() {
            let candidate_rel = format!("{}.py", parts[..len].join("/"));
            if known_files.contains_key(&candidate_rel) {
                return Some(candidate_rel);
            }
            let mut matched: Option<&str> = None;
            let mut ambiguous = false;
            for k in known_files.keys() {
                if path_matches_suffix_on_boundary(k, &candidate_rel) {
                    if matched.is_some() {
                        ambiguous = true;
                        break;
                    }
                    matched = Some(k.as_str());
                }
            }
            if !ambiguous && let Some(m) = matched {
                return Some(m.to_string());
            }
        }
    }
    if known_files.len() == 1 {
        return known_files.keys().next().cloned();
    }
    None
}


