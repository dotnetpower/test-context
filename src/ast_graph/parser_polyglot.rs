use super::*;

/// Parses polyglot source files within the supported scope:
/// C/C++ (`.c`, `.cpp`, `.cc`, `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx`),
/// C# (`.cs`), Protobuf (`.proto`), and GraphQL (`.graphql`, `.gql`).
pub fn parse_polyglot_source(rel_path: &str, source: &str) -> ParsedPythonFile {
    let mut imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();

    if rel_path.ends_with(".proto") {
        if let Some(stem) = Path::new(rel_path).file_stem().and_then(|s| s.to_str()) {
            defined_symbols.insert(format!("proto:{stem}"));
        }
        for raw_line in source.lines() {
            let line = raw_line.split("//").next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("import ") {
                let target = rest
                    .trim_start_matches("public ")
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .trim_matches(['"', '\'']);
                if !target.is_empty() {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: target.to_string(),
                        symbols: Vec::new(),
                    });
                }
            } else if let Some(rest) = line
                .strip_prefix("message ")
                .or_else(|| line.strip_prefix("service "))
            {
                let name = rest.split(['{', ' ']).next().unwrap_or("").trim();
                if !name.is_empty() {
                    defined_symbols.insert(name.to_string());
                }
            }
        }
        return ParsedPythonFile {
            imports: imports.into_iter().collect(),
            defined_symbols: defined_symbols.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }

    if rel_path.ends_with(".graphql") || rel_path.ends_with(".gql") {
        for raw_line in source.lines() {
            let trimmed = raw_line.trim();
            if let Some(rest) = trimmed.strip_prefix("#import ") {
                let spec = rest.trim().trim_matches(['"', '\'', ';']);
                if !spec.is_empty() {
                    imports.insert(ImportSpec {
                        level: 0,
                        module: spec.to_string(),
                        symbols: Vec::new(),
                    });
                }
            }
        }
        return ParsedPythonFile {
            imports: imports.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }

    if is_c_cpp_source_path(rel_path) {
        for (hdr, _is_quoted) in extract_c_includes_from_source(source) {
            if !Path::new(&hdr).is_absolute() {
                data_refs.insert(hdr.clone());
            }
            imports.insert(ImportSpec {
                level: 0,
                module: hdr,
                symbols: Vec::new(),
            });
        }
    }

    let is_csharp = rel_path.ends_with(".cs");
    let mut pending_test_attr = false;
    for raw_line in source.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if (line.starts_with("TEST(") || line.starts_with("TEST_F("))
            && let Some(open_p) = line.find('(')
            && let Some(close_p) = line[open_p + 1..].find(')')
        {
            let args: Vec<&str> = line[open_p + 1..open_p + 1 + close_p]
                .split(',')
                .map(str::trim)
                .collect();
            if args.len() == 2 {
                test_symbols.insert(format!("{}.{}", args[0], args[1]));
            }
        }
        // C# namespace declarations (`namespace Acme.Billing;` or `namespace Acme.Billing {`)
        if is_csharp && let Some(rest) = line.strip_prefix("namespace ") {
            let ns = rest
                .split([';', '{', ' '])
                .next()
                .unwrap_or("")
                .trim();
            if !ns.is_empty() {
                defined_symbols.insert(format!("cs_ns:{ns}"));
            }
            continue;
        }
        // C# `[Fact]`, `[Theory]`, `[Test]`, `[TestMethod]`
        if is_csharp
            && (line.starts_with("[Fact")
                || line.starts_with("[Theory")
                || line.starts_with("[Test"))
        {
            pending_test_attr = true;
            continue;
        }
        // C# `using` directives (standard namespace, `global using`, `using static`, `using Alias = ...`)
        if is_csharp {
            let (is_global_using, using_line) = match line.strip_prefix("global ") {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            if let Some(rest) = using_line.strip_prefix("using ") {
                let trimmed_rest = rest.trim_end_matches(';').trim();
                if !trimmed_rest.is_empty()
                    && !trimmed_rest.contains('(')
                    && !trimmed_rest.starts_with("var ")
                {
                    if let Some(static_target) = trimmed_rest.strip_prefix("static ") {
                        let target = static_target.trim();
                        if !target.is_empty() && !target.starts_with("System") {
                            if is_global_using {
                                defined_symbols.insert(format!("cs_global_using_static:{target}"));
                            }
                            imports.insert(ImportSpec {
                                level: 0,
                                module: target.to_string(),
                                symbols: vec!["__cs_static__".to_string()],
                            });
                        }
                    } else if let Some((_, rhs)) = trimmed_rest.split_once('=') {
                        let target = rhs.trim();
                        if !target.is_empty() && !target.starts_with("System") {
                            if is_global_using {
                                defined_symbols.insert(format!("cs_global_using_alias:{target}"));
                            }
                            imports.insert(ImportSpec {
                                level: 0,
                                module: target.to_string(),
                                symbols: vec!["__cs_alias__".to_string()],
                            });
                        }
                    } else if !trimmed_rest.starts_with("System") {
                        if is_global_using {
                            defined_symbols.insert(format!("cs_global_using:{trimmed_rest}"));
                        }
                        imports.insert(ImportSpec {
                            level: 0,
                            module: trimmed_rest.to_string(),
                            symbols: Vec::new(),
                        });
                    }
                }
            }
        }
        for kw in ["class ", "interface ", "struct ", "record ", "enum "] {
            if let Some(idx) = line.find(kw) {
                let after = &line[idx + kw.len()..];
                let name = after
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                if !name.is_empty() {
                    defined_symbols.insert(name.to_string());
                }
            }
        }
        if pending_test_attr && let Some(paren_idx) = line.find('(') {
            let before_paren = line[..paren_idx].trim();
            if let Some(fn_name) = before_paren.split_whitespace().last() {
                let clean_name = fn_name.trim();
                if !clean_name.is_empty() {
                    test_symbols.insert(clean_name.to_string());
                    defined_symbols.insert(clean_name.to_string());
                }
            }
            pending_test_attr = false;
        }
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}

/// Joins C/C++ preprocessor backslash-newline continuations (`\\\n`, `\\\r\n`, and trailing spaces
/// before newline) so split `#include` directives parse as single logical lines.
pub(super) fn splice_c_continuation_lines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\\') {
        return std::borrow::Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' {
            let mut j = i + 1;
            while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j < len && bytes[j] == b'\n' {
                i = j + 1;
                continue;
            }
            if j + 1 < len && bytes[j] == b'\r' && bytes[j + 1] == b'\n' {
                i = j + 2;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    std::borrow::Cow::Owned(out)
}

/// Replaces C `/* ... */` and `// ...` comments with spaces (preserving `\n`) while respecting
/// string/char literals, so `#include /* comment */ "foo.h"` parses cleanly without false matches
/// inside comments.
pub(super) fn strip_c_comments_preserving_newlines(source: &str) -> String {
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
        if bytes[i] == b'"' || bytes[i] == b'\'' {
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

/// Extracts `#include` and `__has_include` header targets from C/C++ source after splicing
/// `\` continuations and stripping comments. Returns `(header_path, is_quoted)`.
pub(super) fn extract_c_includes_from_source(source: &str) -> Vec<(String, bool)> {
    let spliced = splice_c_continuation_lines(source);
    let cleaned = strip_c_comments_preserving_newlines(&spliced);
    let mut out = Vec::new();
    for raw_line in cleaned.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(after_hash) = trimmed
            .strip_prefix('#')
            .or_else(|| trimmed.strip_prefix("%:"))
        {
            let dir = after_hash.trim_start();
            let rest_opt = dir
                .strip_prefix("include_next")
                .or_else(|| dir.strip_prefix("include"));
            if let Some(after_inc) = rest_opt
                && (after_inc.starts_with('"')
                    || after_inc.starts_with('<')
                    || after_inc
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_whitespace))
            {
                let operand = after_inc.trim_start();
                if let Some(after_q) = operand.strip_prefix('"')
                    && let Some(end_q) = after_q.find('"')
                {
                    let hdr = &after_q[..end_q];
                    if !hdr.is_empty() {
                        out.push((hdr.to_string(), true));
                    }
                } else if let Some(after_lt) = operand.strip_prefix('<')
                    && let Some(end_gt) = after_lt.find('>')
                {
                    let hdr = &after_lt[..end_gt];
                    if !hdr.is_empty() {
                        out.push((hdr.to_string(), false));
                    }
                }
            }
        }
        for (i, ch) in trimmed.char_indices() {
            if ch != '_' {
                continue;
            }
            let tail = &trimmed[i..];
            let after_kw = tail
                .strip_prefix("__has_include_next")
                .or_else(|| tail.strip_prefix("__has_include"));
            if let Some(after) = after_kw
                && let Some(after_p) = after.trim_start().strip_prefix('(')
            {
                let inner = after_p.trim_start();
                if let Some(after_q) = inner.strip_prefix('"')
                    && let Some(end_q) = after_q.find('"')
                {
                    let hdr = after_q[..end_q].trim();
                    if !hdr.is_empty() {
                        out.push((hdr.to_string(), true));
                    }
                } else if let Some(after_lt) = inner.strip_prefix('<')
                    && let Some(end_gt) = after_lt.find('>')
                {
                    let hdr = after_lt[..end_gt].trim();
                    if !hdr.is_empty() {
                        out.push((hdr.to_string(), false));
                    }
                }
            }
        }
    }
    out
}
