use super::*;

pub(super) fn unescape_css_token(raw: &str) -> std::borrow::Cow<'_, str> {
    if !raw.as_bytes().contains(&b'\\') {
        return std::borrow::Cow::Borrowed(raw);
    }
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut p = 0usize;
    while p < len {
        if bytes[p] == b'\\' && p + 1 < len {
            let hex_start = p + 1;
            let mut hex_end = hex_start;
            while hex_end < len && hex_end - hex_start < 6 && bytes[hex_end].is_ascii_hexdigit() {
                hex_end += 1;
            }
            if hex_end > hex_start
                && let Ok(code) = u32::from_str_radix(&raw[hex_start..hex_end], 16)
            {
                let decoded = if code == 0 {
                    '\u{FFFD}'
                } else {
                    char::from_u32(code).unwrap_or('\u{FFFD}')
                };
                out.push(decoded);
                p = hex_end;
                if p < len && bytes[p].is_ascii_whitespace() {
                    if bytes[p] == b'\r' && p + 1 < len && bytes[p + 1] == b'\n' {
                        p += 2;
                    } else {
                        p += 1;
                    }
                }
                continue;
            }
            if bytes[p + 1] == b'\n' {
                p += 2;
                continue;
            }
            if bytes[p + 1] == b'\r' && p + 2 < len && bytes[p + 2] == b'\n' {
                p += 3;
                continue;
            }
            if let Some(ch) = raw[p + 1..].chars().next() {
                out.push(ch);
                p += 1 + ch.len_utf8();
                continue;
            }
        }
        let ch = raw[p..].chars().next().unwrap_or(' ');
        out.push(ch);
        p += ch.len_utf8();
    }
    std::borrow::Cow::Owned(out)
}

/// Parses a CSS / SCSS / Sass / Less stylesheet with a lightweight single-pass scanner
/// (without allocating a full JS/TS token vector) to extract `@import`, `@use`, `@forward`, `url(...)`, and `.className` selectors.
pub fn parse_css_source(source: &str) -> ParsedPythonFile {
    parse_css_source_for_path("styles.scss", source)
}

pub fn parse_css_source_for_path(rel_path: &str, source: &str) -> ParsedPythonFile {
    let is_plain_css = rel_path.ends_with(".css");
    let mut imports = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    let scan_css_string = |start: usize| -> Option<(&str, usize)> {
        let q = *bytes.get(start)?;
        if q != b'"' && q != b'\'' {
            return None;
        }
        let mut p = start + 1;
        while p < len {
            if bytes[p] == b'\\' && p + 1 < len {
                p += 2;
                continue;
            }
            if bytes[p] == q {
                return Some((&source[start + 1..p], p + 1));
            }
            p += 1;
        }
        None
    };

    let skip_ws_and_block_comments = |mut p: usize| -> usize {
        while p < len {
            if bytes[p].is_ascii_whitespace() {
                p += 1;
            } else if bytes[p] == b'/' && p + 1 < len && bytes[p + 1] == b'*' {
                p += 2;
                while p + 1 < len && !(bytes[p] == b'*' && bytes[p + 1] == b'/') {
                    p += 1;
                }
                p = (p + 2).min(len);
            } else {
                break;
            }
        }
        p
    };

    let skip_ws_and_comments = |mut p: usize| -> usize {
        while p < len {
            if bytes[p].is_ascii_whitespace() {
                p += 1;
            } else if bytes[p] == b'/' && p + 1 < len && bytes[p + 1] == b'*' {
                p += 2;
                while p + 1 < len && !(bytes[p] == b'*' && bytes[p + 1] == b'/') {
                    p += 1;
                }
                p = (p + 2).min(len);
            } else if !is_plain_css && bytes[p] == b'/' && p + 1 < len && bytes[p + 1] == b'/' {
                p = memchr::memchr(b'\n', &bytes[p..])
                    .map(|off| p + off + 1)
                    .unwrap_or(len);
            } else {
                break;
            }
        }
        p
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CssBlockKind {
        Rule,
        GroupingAtRule,
    }
    let mut block_stack: Vec<CssBlockKind> = Vec::new();
    let mut pending_grouping_at_rule = false;
    let mut after_local_prefix = false;
    while i < len {
        let b = bytes[i];
        if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(len);
            continue;
        }
        // Plain CSS has no `//` comments (`--url: https://example.com;`), so only SCSS/SASS/LESS
        // treat `//` as a line comment.
        if !is_plain_css && b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            i = memchr::memchr(b'\n', &bytes[i..])
                .map(|off| i + off + 1)
                .unwrap_or(len);
            continue;
        }
        if b == b'{' {
            let kind = if pending_grouping_at_rule {
                CssBlockKind::GroupingAtRule
            } else {
                CssBlockKind::Rule
            };
            block_stack.push(kind);
            pending_grouping_at_rule = false;
            after_local_prefix = false;
            i += 1;
            continue;
        }
        if b == b'}' {
            block_stack.pop();
            pending_grouping_at_rule = false;
            after_local_prefix = false;
            i += 1;
            continue;
        }
        // Selector position holds at top level AND directly inside grouping at-rules
        // (`@media (...) { body { ... } }`, `@supports`, `@container`, `@layer`, `@scope`).
        let in_selector_pos = !pending_grouping_at_rule
            && matches!(block_stack.last(), None | Some(CssBlockKind::GroupingAtRule));
        if b == b':' && source[i..].starts_with(":global") {
            let after = i + ":global".len();
            if after >= len
                || !bytes[after].is_ascii_alphanumeric() && bytes[after] != b'-' && bytes[after] != b'_'
            {
                defined_symbols.insert("css_has_global".to_string());
            }
        }
        if in_selector_pos && b == b':' && source[i..].starts_with(":root") {
            let after = i + ":root".len();
            if after >= len
                || !bytes[after].is_ascii_alphanumeric() && bytes[after] != b'-' && bytes[after] != b'_'
            {
                defined_symbols.insert("css_has_global".to_string());
            }
        }
        if in_selector_pos && !after_local_prefix && matches!(b, b'*' | b'[') {
            // Universal (`*`) or bare attribute (`[hidden] { ... }`) selector applies globally.
            defined_symbols.insert("css_has_global".to_string());
        }
        if in_selector_pos && !after_local_prefix && b == b':' {
            // Bare top-level pseudo-class selector (`:root`, `:where(...)`, `:is(...)`, `:not(...)`, `:host`).
            defined_symbols.insert("css_has_global".to_string());
        }
        if in_selector_pos
            && !after_local_prefix
            && b.is_ascii_alphabetic()
            && (i == 0
                || bytes[i - 1].is_ascii_whitespace()
                || matches!(bytes[i - 1], b',' | b'{' | b'}' | b';' | b'>' | b'+' | b'~'))
        {
            // Bare element/tag selector at top level or inside `@media`/`@supports` (e.g.
            // `@media (...) { body { ... } }`) in a CSS Module applies globally.
            defined_symbols.insert("css_has_global".to_string());
        }
        if b == b'.' || b == b'#' {
            after_local_prefix = true;
        } else if b.is_ascii_whitespace() || matches!(b, b',' | b';' | b'>' | b'+' | b'~') {
            if b == b';' {
                pending_grouping_at_rule = false;
            }
            after_local_prefix = false;
        }
        if b == b'@' {
            let kw_start = i + 1;
            let mut p = kw_start;
            while p < len && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'-') {
                p += 1;
            }
            let kw = &source[kw_start..p];
            let kw_lower = kw.to_ascii_lowercase();
            // Match CSS at-rule names case-insensitively (`@MEDIA`, `@Supports`) and treat
            // unknown block at-rules conservatively as grouping rules so nested selectors are checked.
            if matches!(
                kw_lower.as_str(),
                "font-face" | "keyframes" | "property" | "layer" | "page" | "import"
            ) || kw_lower.ends_with("-keyframes")
            {
                defined_symbols.insert("css_has_global".to_string());
            } else if !matches!(kw_lower.as_str(), "charset" | "namespace" | "use" | "forward") {
                pending_grouping_at_rule = true;
            }
            if matches!(kw_lower.as_str(), "import" | "use" | "forward" | "require") {
                p = skip_ws_and_comments(p);
                if let Some((spec, next_p)) = scan_css_string(p) {
                    let decoded = unescape_css_token(spec);
                    if let Some(clean) = clean_frontend_import_specifier(&decoded) {
                        let module = if clean.starts_with('.')
                            || clean.starts_with('/')
                            || clean.starts_with('@')
                            || clean.starts_with('~')
                        {
                            clean
                        } else {
                            format!("./{clean}")
                        };
                        imports.insert(ImportSpec {
                            level: 0,
                            module,
                            symbols: Vec::new(),
                        });
                    }
                    i = next_p;
                    continue;
                }
            }
            i = p.max(i + 1);
            continue;
        }
        if (b == b'u' || b == b'U')
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'-'
                    || bytes[i - 1] >= 0x80))
            && source[i..].len() >= 4
            && source.as_bytes()[i..i + 4].eq_ignore_ascii_case(b"url(")
        {
            // Never treat `//` as a line comment when entering `url(...)` (e.g. `url(//cdn/x.css)`).
            let mut p = skip_ws_and_block_comments(i + 4);
            let spec_opt = if let Some((s, next_p)) = scan_css_string(p) {
                Some((unescape_css_token(s), next_p))
            } else {
                let start = p;
                while p < len && bytes[p] != b')' && !bytes[p].is_ascii_whitespace() {
                    if bytes[p] == b'\\' && p + 1 < len {
                        let hex_start = p + 1;
                        let mut hex_end = hex_start;
                        while hex_end < len
                            && hex_end - hex_start < 6
                            && bytes[hex_end].is_ascii_hexdigit()
                        {
                            hex_end += 1;
                        }
                        if hex_end > hex_start {
                            p = hex_end;
                            if p < len && bytes[p].is_ascii_whitespace() {
                                if bytes[p] == b'\r' && p + 1 < len && bytes[p + 1] == b'\n' {
                                    p += 2;
                                } else {
                                    p += 1;
                                }
                            }
                        } else if let Some(ch) = source[p + 1..].chars().next() {
                            p += 1 + ch.len_utf8();
                        } else {
                            p += 1;
                        }
                    } else {
                        p += 1;
                    }
                }
                if p > start {
                    Some((unescape_css_token(&source[start..p]), p))
                } else {
                    None
                }
            };
            if let Some((spec, next_p)) = spec_opt {
                if let Some(clean) = clean_frontend_import_specifier(&spec) {
                    if let Some(clean_ref) = normalize_data_ref(&clean) {
                        data_refs.insert(clean_ref);
                    }
                    let module = if clean.starts_with('.') || clean.starts_with('/') {
                        clean
                    } else {
                        format!("./{clean}")
                    };
                    imports.insert(ImportSpec {
                        level: 0,
                        module,
                        symbols: Vec::new(),
                    });
                }
                i = next_p;
                continue;
            }
            i += 4;
            continue;
        }
        if b == b'.' && i + 1 < len && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_' || bytes[i + 1] == b'-') {
            let start = i + 1;
            let mut p = start + 1;
            while p < len
                && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'_' || bytes[p] == b'-')
            {
                p += 1;
            }
            let cls_name = &source[start..p];
            defined_symbols.insert(format!("css_class:{cls_name}"));
            defined_symbols.insert(cls_name.to_string());
            i = p;
            continue;
        }
        if (b == b'"' || b == b'\'')
            && let Some((_, next_p)) = scan_css_string(i)
        {
            i = next_p;
            continue;
        }
        i += 1;
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}


