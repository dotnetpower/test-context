use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PyToken<'a> {
    Ident(&'a str),
    StringLit(String),
    Dot,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Assign,
    At,
    Star,
    Slash,
    Newline,
    StartTypeChecking,
    EndTypeChecking,
    Other,
}

pub(super) fn is_type_checking_header(line_rem: &str) -> bool {
    let code = line_rem.split('#').next().unwrap_or("").trim();
    matches!(
        code,
        "if TYPE_CHECKING:" | "if typing.TYPE_CHECKING:" | "if t.TYPE_CHECKING:"
    )
}

pub(super) fn push_py_other(tokens: &mut Vec<PyToken<'_>>) {
    if !matches!(tokens.last(), Some(PyToken::Other)) {
        tokens.push(PyToken::Other);
    }
}

pub(super) fn push_py_newline(tokens: &mut Vec<PyToken<'_>>) {
    if !matches!(tokens.last(), Some(PyToken::Newline)) {
        tokens.push(PyToken::Newline);
    }
}

pub(super) fn tokenize_python(source: &str) -> Vec<PyToken<'_>> {
    tokenize_python_with_indents(source).0
}

pub(super) fn tokenize_python_with_indents(source: &str) -> (Vec<PyToken<'_>>, Vec<usize>) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::with_capacity((len / 6).clamp(16, 65_536));
    let mut token_indents = Vec::with_capacity((len / 6).clamp(16, 65_536));
    let mut at_line_start = true;
    let mut line_indent = 0usize;
    let mut tc_indent: Option<usize> = None;

    while i < len {
        let b = bytes[i];
        if at_line_start {
            if b == b' ' {
                line_indent += 1;
                i += 1;
                continue;
            } else if b == b'\t' {
                line_indent += 4;
                i += 1;
                continue;
            } else if b == b'\r' {
                i += 1;
                continue;
            } else if b == b'\n' {
                push_py_newline(&mut tokens);
                while token_indents.len() < tokens.len() {
                    token_indents.push(line_indent);
                }
                line_indent = 0;
                i += 1;
                continue;
            } else if b == b'#' {
                i = memchr::memchr(b'\n', &bytes[i..])
                    .map(|off| i + off)
                    .unwrap_or(len);
                continue;
            } else {
                at_line_start = false;
                if let Some(base_indent) = tc_indent
                    && line_indent <= base_indent
                {
                    tokens.push(PyToken::EndTypeChecking);
                    tc_indent = None;
                }
                if tc_indent.is_none() && (b == b'i' || b == b't') {
                    let line_end = memchr::memchr(b'\n', &bytes[i..])
                        .map(|off| i + off)
                        .unwrap_or(len);
                    if is_type_checking_header(&source[i..line_end]) {
                        tc_indent = Some(line_indent);
                        tokens.push(PyToken::StartTypeChecking);
                    }
                }
            }
        }
        match b {
            b' ' | b'\t' | b'\r' => {
                i += 1;
            }
            b'\\' => {
                // Line continuation `\<newline>`
                i += 1;
                if i < len && bytes[i] == b'\r' {
                    i += 1;
                }
                if i < len && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                i = memchr::memchr(b'\n', &bytes[i..])
                    .map(|off| i + off)
                    .unwrap_or(len);
            }
            b'\n' => {
                push_py_newline(&mut tokens);
                while token_indents.len() < tokens.len() {
                    token_indents.push(line_indent);
                }
                at_line_start = true;
                line_indent = 0;
                i += 1;
            }
            b';' => {
                push_py_newline(&mut tokens);
                i += 1;
            }
            b'.' => {
                tokens.push(PyToken::Dot);
                i += 1;
            }
            b',' => {
                tokens.push(PyToken::Comma);
                i += 1;
            }
            b'(' => {
                tokens.push(PyToken::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(PyToken::RParen);
                i += 1;
            }
            b'[' => {
                tokens.push(PyToken::LBracket);
                i += 1;
            }
            b']' => {
                tokens.push(PyToken::RBracket);
                i += 1;
            }
            b':' => {
                tokens.push(PyToken::Colon);
                i += 1;
            }
            b'=' => {
                if i + 1 < len && bytes[i + 1] == b'=' {
                    push_py_other(&mut tokens);
                    i += 2;
                } else {
                    tokens.push(PyToken::Assign);
                    i += 1;
                }
            }
            b'@' => {
                tokens.push(PyToken::At);
                i += 1;
            }
            b'*' => {
                tokens.push(PyToken::Star);
                i += 1;
            }
            b'/' => {
                if i + 1 < len && bytes[i + 1] == b'/' {
                    push_py_other(&mut tokens);
                    i += 2;
                } else {
                    tokens.push(PyToken::Slash);
                    i += 1;
                }
            }
            b'\'' | b'"' => {
                let (lit, next_i) = scan_string_literal(source, i, false, false, false);
                tokens.push(PyToken::StringLit(lit));
                i = next_i;
            }
            _ if is_ident_start(b) => {
                let start = i;
                i += 1;
                while i < len && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let word = &source[start..i];
                // Check for string prefix like r"...", f"...", b"...", rf"..."
                if i < len
                    && (bytes[i] == b'\'' || bytes[i] == b'"')
                    && word.len() <= 2
                    && word
                        .bytes()
                        .all(|c| matches!(c, b'r' | b'R' | b'f' | b'F' | b'b' | b'B' | b'u' | b'U'))
                {
                    let is_fstring = word.bytes().any(|c| c == b'f' || c == b'F');
                    let is_raw = word.bytes().any(|c| c == b'r' || c == b'R');
                    let is_bytes = word.bytes().any(|c| c == b'b' || c == b'B');
                    let (lit, next_i) = scan_string_literal(source, i, is_fstring, is_raw, is_bytes);
                    tokens.push(PyToken::StringLit(lit));
                    if is_fstring {
                        // Also tokenize `{...}` interpolation expressions inside f-strings so
                        // `os.environ[...]`, dynamic imports, and symbol references enter the graph.
                        append_python_fstring_expr_tokens(source, i, next_i, &mut tokens);
                    }
                    i = next_i;
                } else {
                    tokens.push(PyToken::Ident(word));
                }
            }
            _ => {
                push_py_other(&mut tokens);
                i += 1;
            }
        }
        while token_indents.len() < tokens.len() {
            token_indents.push(line_indent);
        }
    }
    if tc_indent.is_some() {
        tokens.push(PyToken::EndTypeChecking);
        token_indents.push(0);
    }
    (tokens, token_indents)
}

pub(super) fn is_fstring_quote_at(bytes: &[u8], idx: usize) -> bool {
    if idx == 0 || !matches!(bytes.get(idx), Some(b'"' | b'\'')) {
        return false;
    }
    let prev = bytes[idx - 1];
    if matches!(prev, b'f' | b'F') {
        if idx == 1 {
            return true;
        }
        let prev2 = bytes[idx - 2];
        if !(prev2.is_ascii_alphanumeric() || prev2 == b'_') {
            return true;
        }
        if matches!(prev2, b'r' | b'R') {
            return idx == 2 || !(bytes[idx - 3].is_ascii_alphanumeric() || bytes[idx - 3] == b'_');
        }
        return false;
    }
    if matches!(prev, b'r' | b'R') && idx >= 2 && matches!(bytes[idx - 2], b'f' | b'F') {
        return idx == 2 || !(bytes[idx - 3].is_ascii_alphanumeric() || bytes[idx - 3] == b'_');
    }
    false
}

/// Per-replacement-field lexical state stack for Python f-strings (`f"{expr:fmt{width}}"`):
/// each frame tracks `(paren_depth, in_format_spec, dict_brace_depth, expr_start)` so nested
/// replacement fields inside format specifiers tokenize independently without treating format-spec
/// `#` (e.g. `#0{WIDTH}x`) as a Python comment or losing quotes inside nested fields.
pub(super) fn append_python_fstring_expr_tokens<'a>(
    source: &'a str,
    start_quote_idx: usize,
    end_idx: usize,
    out: &mut Vec<PyToken<'a>>,
) {
    let bytes = source.as_bytes();
    if start_quote_idx >= end_idx || end_idx > bytes.len() {
        return;
    }
    let quote = bytes[start_quote_idx];
    let is_triple = start_quote_idx + 2 < end_idx
        && bytes[start_quote_idx + 1] == quote
        && bytes[start_quote_idx + 2] == quote;
    let body_start = if is_triple {
        start_quote_idx + 3
    } else {
        start_quote_idx + 1
    };
    let body_end = if is_triple {
        end_idx.saturating_sub(3)
    } else {
        end_idx.saturating_sub(1)
    };
    if body_start >= body_end {
        return;
    }
    let mut emit_expr = |expr_slice: &'a str| {
        if !expr_slice.trim().is_empty() && expr_slice.len() < source.len() {
            for tok in tokenize_python(expr_slice) {
                if !matches!(tok, PyToken::Newline) {
                    out.push(tok);
                }
            }
        }
    };
    let mut p = body_start;
    while p < body_end {
        // In Python f-strings (`f"\{x}"` and `rf"\{x}"`), `\` does NOT escape `{`;
        // only skip escaped backslashes `\\` and escaped quotes `\"`/`\'`.
        if bytes[p] == b'\\' && p + 1 < body_end && matches!(bytes[p + 1], b'\\' | b'"' | b'\'') {
            p += 2;
            continue;
        }
        if bytes[p] == b'{' {
            if p + 1 < body_end && bytes[p + 1] == b'{' {
                p += 2;
                continue;
            }
            // Stack frames: (expr_start, expr_end_opt, paren_depth, dict_brace_depth, in_format_spec)
            let mut fields: Vec<(usize, Option<usize>, usize, usize, bool)> =
                vec![(p + 1, None, 0, 0, false)];
            let mut q = p + 1;
            let mut in_q: Option<(u8, bool)> = None;
            while q < body_end && !fields.is_empty() {
                if let Some((iq, iq_triple)) = in_q {
                    if bytes[q] == b'\\' && q + 1 < body_end {
                        q += 2;
                        continue;
                    }
                    if iq_triple {
                        if q + 2 < body_end
                            && bytes[q] == iq
                            && bytes[q + 1] == iq
                            && bytes[q + 2] == iq
                        {
                            in_q = None;
                            q += 3;
                            continue;
                        }
                    } else if bytes[q] == iq || bytes[q] == b'\n' {
                        in_q = None;
                    }
                    q += 1;
                    continue;
                }
                let in_fmt = fields.last().is_some_and(|f| f.4);
                if in_fmt {
                    // Inside a format specifier, only `{` (nested replacement field) and `}`
                    // (closing this field) are structural; `#`, quotes, etc. are literal format text.
                    if bytes[q] == b'{' {
                        fields.push((q + 1, None, 0, 0, false));
                        q += 1;
                        continue;
                    }
                    if bytes[q] == b'}' {
                        if let Some((es, ee_opt, _, _, _)) = fields.pop() {
                            let ee = ee_opt.unwrap_or(q);
                            if ee > es {
                                emit_expr(&source[es..ee]);
                            }
                        }
                        q += 1;
                        continue;
                    }
                    q += 1;
                    continue;
                }
                if bytes[q] == b'#' {
                    q = memchr::memchr(b'\n', &bytes[q..body_end])
                        .map(|off| q + off + 1)
                        .unwrap_or(body_end);
                    continue;
                }
                if bytes[q] == b'"' || bytes[q] == b'\'' {
                    if is_fstring_quote_at(bytes, q) {
                        let (_, next_q) = scan_string_literal(source, q, true, false, false);
                        q = next_q.min(body_end);
                        continue;
                    }
                    let iq = bytes[q];
                    let iq_triple = q + 2 < body_end && bytes[q + 1] == iq && bytes[q + 2] == iq;
                    in_q = Some((iq, iq_triple));
                    q += if iq_triple { 3 } else { 1 };
                    continue;
                }
                if let Some(top) = fields.last_mut() {
                    if matches!(bytes[q], b'(' | b'[') {
                        top.2 += 1;
                    } else if matches!(bytes[q], b')' | b']') {
                        top.2 = top.2.saturating_sub(1);
                    } else if top.2 == 0 && top.3 == 0 && bytes[q] == b':' {
                        // Record end of expression before format specifier so format text (`#0x`)
                        // is never retokenized as a Python comment.
                        top.1 = Some(q);
                        top.4 = true;
                    } else if bytes[q] == b'{' {
                        top.3 += 1;
                    } else if bytes[q] == b'}' {
                        if top.3 > 0 {
                            top.3 -= 1;
                        } else if let Some((es, ee_opt, _, _, _)) = fields.pop() {
                            let ee = ee_opt.unwrap_or(q);
                            if ee > es {
                                emit_expr(&source[es..ee]);
                            }
                        }
                    }
                }
                q += 1;
            }
            p = q;
            continue;
        }
        p += 1;
    }
}

pub(super) fn decode_python_string_escapes(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' && i + 1 < len {
            match bytes[i + 1] {
                b'\\' => {
                    out.push('\\');
                    i += 2;
                    continue;
                }
                b'\'' => {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                b'"' => {
                    out.push('"');
                    i += 2;
                    continue;
                }
                b'a' => {
                    out.push('\x07');
                    i += 2;
                    continue;
                }
                b'b' => {
                    out.push('\x08');
                    i += 2;
                    continue;
                }
                b'f' => {
                    out.push('\x0c');
                    i += 2;
                    continue;
                }
                b'n' => {
                    out.push('\n');
                    i += 2;
                    continue;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                    continue;
                }
                b'v' => {
                    out.push('\x0b');
                    i += 2;
                    continue;
                }
                b'\n' => {
                    i += 2;
                    continue;
                }
                b'\r' => {
                    i += if i + 2 < len && bytes[i + 2] == b'\n' { 3 } else { 2 };
                    continue;
                }
                b'x' if i + 3 < len
                    && bytes[i + 2..i + 4].iter().all(u8::is_ascii_hexdigit) =>
                {
                    if let Ok(v) = u8::from_str_radix(&raw[i + 2..i + 4], 16) {
                        out.push(v as char);
                        i += 4;
                        continue;
                    }
                }
                b'u' if i + 5 < len
                    && bytes[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit) =>
                {
                    if let Some(ch) = u32::from_str_radix(&raw[i + 2..i + 6], 16)
                        .ok()
                        .and_then(char::from_u32)
                    {
                        out.push(ch);
                        i += 6;
                        continue;
                    }
                }
                b'U' if i + 9 < len
                    && bytes[i + 2..i + 10].iter().all(u8::is_ascii_hexdigit) =>
                {
                    if let Some(ch) = u32::from_str_radix(&raw[i + 2..i + 10], 16)
                        .ok()
                        .and_then(char::from_u32)
                    {
                        out.push(ch);
                        i += 10;
                        continue;
                    }
                }
                b'0'..=b'7' => {
                    let mut end = i + 2;
                    while end < len && end < i + 4 && matches!(bytes[end], b'0'..=b'7') {
                        end += 1;
                    }
                    if let Some(ch) = u32::from_str_radix(&raw[i + 1..end], 8)
                        .ok()
                        .and_then(char::from_u32)
                    {
                        out.push(ch);
                        i = end;
                        continue;
                    }
                }
                _ => {
                    // Python preserves unrecognized escapes (e.g. `\d`, `\N{...}`) verbatim.
                    out.push('\\');
                    i += 1;
                    continue;
                }
            }
        }
        let ch = raw[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub(super) fn decode_python_bytes_literal(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out: Vec<u8> = Vec::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' && i + 1 < len {
            match bytes[i + 1] {
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                    continue;
                }
                b'\'' => {
                    out.push(b'\'');
                    i += 2;
                    continue;
                }
                b'"' => {
                    out.push(b'"');
                    i += 2;
                    continue;
                }
                b'a' => {
                    out.push(0x07);
                    i += 2;
                    continue;
                }
                b'b' => {
                    out.push(0x08);
                    i += 2;
                    continue;
                }
                b'f' => {
                    out.push(0x0c);
                    i += 2;
                    continue;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                    continue;
                }
                b'v' => {
                    out.push(0x0b);
                    i += 2;
                    continue;
                }
                b'\n' => {
                    i += 2;
                    continue;
                }
                b'\r' => {
                    i += if i + 2 < len && bytes[i + 2] == b'\n' { 3 } else { 2 };
                    continue;
                }
                b'x' if i + 3 < len
                    && bytes[i + 2..i + 4].iter().all(u8::is_ascii_hexdigit) =>
                {
                    if let Ok(v) = u8::from_str_radix(&raw[i + 2..i + 4], 16) {
                        out.push(v);
                        i += 4;
                        continue;
                    }
                }
                b'0'..=b'7' => {
                    let mut end = i + 2;
                    while end < len && end < i + 4 && matches!(bytes[end], b'0'..=b'7') {
                        end += 1;
                    }
                    if let Ok(v) = u32::from_str_radix(&raw[i + 1..end], 8) {
                        out.push((v & 0xff) as u8);
                        i = end;
                        continue;
                    }
                }
                _ => {
                    out.push(b'\\');
                    i += 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Valid UTF-8 byte paths (e.g. `b"caf\xc3\xa9.bin"` -> `"café.bin"`) resolve to their exact
    // UTF-8 filename; non-UTF-8 raw OS byte paths return a `\0`-marked sentinel so path consumers
    // fail closed instead of tracking a mangled Latin-1 string!
    String::from_utf8(out).unwrap_or_else(|_| "\0non_utf8_bytes".to_string())
}

pub(super) fn scan_string_literal(
    source: &str,
    start_quote_idx: usize,
    is_fstring: bool,
    is_raw: bool,
    is_bytes: bool,
) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let quote = bytes[start_quote_idx];
    let is_triple = start_quote_idx + 2 < len
        && bytes[start_quote_idx + 1] == quote
        && bytes[start_quote_idx + 2] == quote;
    let content_start = if is_triple {
        start_quote_idx + 3
    } else {
        start_quote_idx + 1
    };
    let mut i = content_start;
    // Per-field stack for f-strings: (paren_depth, dict_brace_depth, in_format_spec)
    let mut fstring_fields: Vec<(usize, usize, bool)> = Vec::new();
    let mut expr_quote: Option<(u8, bool)> = None;
    while i < len {
        if let Some((eq, eq_triple)) = expr_quote {
            if bytes[i] == b'\\' {
                i += 1;
                if i < len {
                    i += source[i..].chars().next().map_or(1, char::len_utf8);
                }
                continue;
            }
            if eq_triple {
                if i + 2 < len && bytes[i] == eq && bytes[i + 1] == eq && bytes[i + 2] == eq {
                    expr_quote = None;
                    i += 3;
                    continue;
                }
            } else if bytes[i] == eq || bytes[i] == b'\n' {
                expr_quote = None;
            }
            i += 1;
            continue;
        }
        if is_fstring && let Some(top) = fstring_fields.last_mut() {
            if top.2 {
                // Inside format specifier: only `{` (nested replacement field) and `}` are structural
                if bytes[i] == b'{' {
                    fstring_fields.push((0, 0, false));
                    i += 1;
                    continue;
                }
                if bytes[i] == b'}' {
                    fstring_fields.pop();
                    i += 1;
                    continue;
                }
                i += 1;
                continue;
            }
            if bytes[i] == b'#' {
                i = memchr::memchr(b'\n', &bytes[i..])
                    .map(|off| i + off + 1)
                    .unwrap_or(len);
                continue;
            }
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                // Python 3.12+ allows nested f-strings reusing the outer quote delimiter
                if is_fstring_quote_at(bytes, i) {
                    let (_, next_i) = scan_string_literal(source, i, true, false, false);
                    i = next_i.min(len);
                    continue;
                }
                let eq = bytes[i];
                let eq_triple = i + 2 < len && bytes[i + 1] == eq && bytes[i + 2] == eq;
                expr_quote = Some((eq, eq_triple));
                i += if eq_triple { 3 } else { 1 };
                continue;
            }
            if matches!(bytes[i], b'(' | b'[') {
                top.0 += 1;
                i += 1;
                continue;
            }
            if matches!(bytes[i], b')' | b']') {
                top.0 = top.0.saturating_sub(1);
                i += 1;
                continue;
            }
            if top.0 == 0 && top.1 == 0 && bytes[i] == b':' {
                top.2 = true;
                i += 1;
                continue;
            }
            if bytes[i] == b'{' {
                top.1 += 1;
                i += 1;
                continue;
            }
            if bytes[i] == b'}' {
                if top.1 > 0 {
                    top.1 -= 1;
                } else {
                    fstring_fields.pop();
                }
                i += 1;
                continue;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'\\' {
            // In Python f-strings (`f"\{x}"` / `rf"\{x}"`), `\` does NOT escape `{` or `}`.
            if is_fstring && i + 1 < len && matches!(bytes[i + 1], b'{' | b'}') {
                i += 1;
                continue;
            }
            i += 1;
            if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
            continue;
        }
        if is_fstring {
            if bytes[i] == b'{' {
                if i + 1 < len && bytes[i + 1] == b'{' {
                    i += 2;
                    continue;
                }
                fstring_fields.push((0, 0, false));
                i += 1;
                continue;
            } else if bytes[i] == b'}' {
                if i + 1 < len && bytes[i + 1] == b'}' {
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
        }
        let brace_depth = fstring_fields.len();
        let decode_slice = |raw_slice: &str| -> String {
            if is_raw || (is_fstring && raw_slice.contains('{')) {
                raw_slice.to_string()
            } else if is_bytes {
                decode_python_bytes_literal(raw_slice)
            } else {
                decode_python_string_escapes(raw_slice)
            }
        };
        if is_triple {
            if brace_depth == 0
                && i + 2 < len
                && bytes[i] == quote
                && bytes[i + 1] == quote
                && bytes[i + 2] == quote
            {
                return (decode_slice(&source[content_start..i]), i + 3);
            }
            i += 1;
        } else {
            // Python 3.12+ allows newlines inside single-quoted f-string `{...}` expressions;
            // only terminate on quote or newline when outside an interpolation (`brace_depth == 0`).
            if brace_depth == 0 && (bytes[i] == quote || bytes[i] == b'\n') {
                return (decode_slice(&source[content_start..i]), (i + 1).min(len));
            }
            i += 1;
        }
    }
    let raw_slice = &source[content_start..len];
    let lit = if is_raw || (is_fstring && raw_slice.contains('{')) {
        raw_slice.to_string()
    } else if is_bytes {
        decode_python_bytes_literal(raw_slice)
    } else {
        decode_python_string_escapes(raw_slice)
    };
    (lit, len)
}

pub(super) fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

pub(super) fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}


pub(super) fn normalize_data_ref(lit: &str) -> Option<String> {
    if lit != lit.trim() {
        return None;
    }
    let trimmed = lit.trim_start_matches("./").replace('\\', "/");
    if trimmed.len() < 3
        || trimmed.len() > 160
        || trimmed.starts_with('/')
        || trimmed.contains(['\n', '\r', '\0', '{', '}', '*', '?'])
        || trimmed.matches(' ').count() > 4
    {
        return None;
    }
    // Allow relative parent traversals (`../fixtures/data.json`) and bounded interior spaces
    // (`fixtures/expected output.json`) because callers resolve them lexically against the
    // referencing file's directory via `normalize_lexical_rel`.
    if DATA_FILE_EXTENSIONS
        .iter()
        .any(|ext| trimmed.ends_with(ext))
    {
        Some(trimmed)
    } else {
        None
    }
}

pub(super) fn is_valid_dotted_module(s: &str) -> bool {
    !s.is_empty()
        && s.split('.')
            .all(|part| !part.is_empty() && part.bytes().all(is_ident_continue))
}

pub(super) fn parse_import_statement(tokens: &[PyToken<'_>], mut i: usize) -> (Vec<ImportSpec>, usize) {
    let n = tokens.len();
    let mut specs = Vec::new();
    while i < n {
        if matches!(tokens[i], PyToken::Newline) {
            i += 1;
            break;
        }
        let (dotted, next_i) = read_dotted_name(tokens, i);
        i = next_i;
        if !dotted.is_empty() {
            specs.push(ImportSpec {
                level: 0,
                module: dotted,
                symbols: Vec::new(),
            });
        }
        // Optional `as alias`
        if matches!(tokens.get(i), Some(PyToken::Ident("as"))) {
            i += 1;
            if matches!(tokens.get(i), Some(PyToken::Ident(_))) {
                i += 1;
            }
        }
        if matches!(tokens.get(i), Some(PyToken::Comma)) {
            i += 1;
        } else {
            while i < n && !matches!(tokens[i], PyToken::Newline) {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            break;
        }
    }
    (specs, i)
}

pub(super) fn parse_from_import_statement(tokens: &[PyToken<'_>], mut i: usize) -> Option<(ImportSpec, usize)> {
    let n = tokens.len();
    let mut level = 0usize;
    while i < n && matches!(tokens[i], PyToken::Dot) {
        level += 1;
        i += 1;
    }
    let (module, next_i) =
        if matches!(tokens.get(i), Some(PyToken::Ident(kw)) if *kw != "import") {
            read_dotted_name(tokens, i)
        } else {
            (String::new(), i)
        };
    i = next_i;
    if !matches!(tokens.get(i), Some(PyToken::Ident("import"))) {
        return None;
    }
    i += 1;
    let mut in_parens = false;
    if matches!(tokens.get(i), Some(PyToken::LParen)) {
        in_parens = true;
        i += 1;
    }
    let mut symbols = Vec::new();
    while i < n {
        match &tokens[i] {
            PyToken::Newline if !in_parens => {
                i += 1;
                break;
            }
            PyToken::Newline | PyToken::Comma => {
                i += 1;
            }
            PyToken::RParen if in_parens => {
                i += 1;
                break;
            }
            PyToken::Star => {
                symbols.push("*".to_string());
                i += 1;
            }
            PyToken::Ident(sym) => {
                symbols.push((*sym).to_string());
                i += 1;
                if matches!(tokens.get(i), Some(PyToken::Ident("as"))) {
                    i += 1;
                    if matches!(tokens.get(i), Some(PyToken::Ident(_))) {
                        i += 1;
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    Some((
        ImportSpec {
            level,
            module,
            symbols,
        },
        i,
    ))
}

pub(super) fn read_dotted_name(tokens: &[PyToken<'_>], mut i: usize) -> (String, usize) {
    let n = tokens.len();
    let mut parts = Vec::new();
    while i < n {
        if let PyToken::Ident(ident) = tokens[i] {
            if ident == "import" || ident == "as" {
                break;
            }
            parts.push(ident);
            i += 1;
            if matches!(tokens.get(i), Some(PyToken::Dot)) {
                i += 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    (parts.join("."), i)
}

pub(super) fn parse_function_params(tokens: &[PyToken<'_>], mut i: usize) -> (Vec<String>, usize) {
    let n = tokens.len();
    let mut params = Vec::new();
    let mut depth = 1usize;
    let mut expect_param_name = true;
    let mut skip_vararg_ident = false;

    while i < n && depth > 0 {
        match &tokens[i] {
            PyToken::LParen | PyToken::LBracket => {
                depth += 1;
                i += 1;
            }
            PyToken::RParen | PyToken::RBracket => {
                depth -= 1;
                i += 1;
            }
            PyToken::Star if depth == 1 && expect_param_name => {
                skip_vararg_ident = true;
                i += 1;
            }
            PyToken::Comma if depth == 1 => {
                expect_param_name = true;
                skip_vararg_ident = false;
                i += 1;
            }
            PyToken::Colon | PyToken::Assign if depth == 1 => {
                expect_param_name = false;
                skip_vararg_ident = false;
                i += 1;
            }
            PyToken::Ident(name) if depth == 1 && expect_param_name => {
                if !skip_vararg_ident {
                    params.push((*name).to_string());
                }
                expect_param_name = false;
                skip_vararg_ident = false;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    (params, i)
}


