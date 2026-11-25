use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum JsToken {
    Ident(String),
    StringLit(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Colon,
    Semi,
    Star,
    Arrow,
    At,
    Other,
}

pub(super) fn push_js_other(tokens: &mut Vec<JsToken>) {
    if !matches!(tokens.last(), Some(JsToken::Other)) {
        tokens.push(JsToken::Other);
    }
}

pub(super) fn tokenize_frontend(source: &str) -> Vec<JsToken> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::with_capacity((len / 6).clamp(16, 65_536));

    let mut last_allows_regex = true;
    let mut last_ident_is_ctrl = false;
    let mut last_was_dot = false;
    let mut paren_stack: Vec<bool> = Vec::new();
    while i < len {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            0xE2 if i + 2 < len
                && bytes[i + 1] == 0x80
                && matches!(bytes[i + 2], 0xA8 | 0xA9) =>
            {
                i += 3;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                i = find_js_line_terminator(bytes, i + 2);
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                i += 2;
                loop {
                    if let Some(off) = memchr::memchr(b'*', &bytes[i..]) {
                        i += off;
                        if i + 1 < len && bytes[i + 1] == b'/' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    } else {
                        i = len;
                        break;
                    }
                }
            }
            b'/' if last_allows_regex => {
                // Skip JS regex literal (e.g. `/"/` or `/[//]/g`) so quotes inside regexes never
                // open a string literal and swallow following `import(...)` statements.
                let mut r = i + 1;
                let mut in_class = false;
                let mut closed = false;
                while r < len && bytes[r] != b'\n' && bytes[r] != b'\r' {
                    if bytes[r] == 0xE2
                        && r + 2 < len
                        && bytes[r + 1] == 0x80
                        && matches!(bytes[r + 2], 0xA8 | 0xA9)
                    {
                        break;
                    }
                    if bytes[r] == b'\\' && r + 1 < len {
                        r += 2;
                        continue;
                    }
                    if bytes[r] == b'[' {
                        in_class = true;
                    } else if bytes[r] == b']' {
                        in_class = false;
                    } else if bytes[r] == b'/' && !in_class {
                        r += 1;
                        while r < len && bytes[r].is_ascii_alphabetic() {
                            r += 1;
                        }
                        closed = true;
                        break;
                    }
                    r += 1;
                }
                if closed {
                    push_js_other(&mut tokens);
                    i = r;
                    last_allows_regex = false;
                } else {
                    push_js_other(&mut tokens);
                    i += 1;
                    last_allows_regex = true;
                }
            }
            b'\'' | b'"' => {
                let (lit, next_i) = scan_js_string(source, i, b);
                tokens.push(JsToken::StringLit(lit));
                i = next_i;
                last_allows_regex = false;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b'`' => {
                let (lit, next_i) = scan_js_template_string(source, i);
                if !lit.is_empty() {
                    tokens.push(JsToken::StringLit(lit));
                } else {
                    // Emit `Other` placeholder for interpolated template literals (`./views/${x}.js`)
                    // so `import(\`./views/${'foo'}.js\`)` is NEVER mistaken for `import('foo')`!
                    push_js_other(&mut tokens);
                }
                append_js_template_expr_tokens(source, i, next_i, &mut tokens);
                i = next_i;
                last_allows_regex = false;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b'{' => {
                tokens.push(JsToken::LBrace);
                i += 1;
                last_allows_regex = true;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b'}' => {
                tokens.push(JsToken::RBrace);
                i += 1;
                last_allows_regex = true;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b'(' => {
                tokens.push(JsToken::LParen);
                paren_stack.push(last_ident_is_ctrl);
                last_ident_is_ctrl = false;
                last_was_dot = false;
                i += 1;
                last_allows_regex = true;
            }
            b')' => {
                tokens.push(JsToken::RParen);
                let is_ctrl = paren_stack.pop().unwrap_or(false);
                last_ident_is_ctrl = false;
                last_was_dot = false;
                i += 1;
                // Control conditions (`if (ok) /regex/`, `while (ok) /regex/`) allow a regex after `)`.
                last_allows_regex = is_ctrl;
            }
            b'[' => {
                tokens.push(JsToken::LBracket);
                i += 1;
                last_allows_regex = true;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b']' => {
                tokens.push(JsToken::RBracket);
                i += 1;
                last_allows_regex = false;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b',' => {
                tokens.push(JsToken::Comma);
                i += 1;
                last_allows_regex = true;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b'.' => {
                tokens.push(JsToken::Dot);
                i += 1;
                last_allows_regex = false;
                last_ident_is_ctrl = false;
                last_was_dot = true;
            }
            b';' => {
                tokens.push(JsToken::Semi);
                i += 1;
                last_allows_regex = true;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            b'*' => {
                tokens.push(JsToken::Star);
                i += 1;
                last_allows_regex = true;
                last_ident_is_ctrl = false;
                last_was_dot = false;
            }
            _ if is_ident_start(b) || b == b'$' => {
                let start = i;
                i += 1;
                while i < len && (is_ident_continue(bytes[i]) || bytes[i] == b'$') {
                    i += 1;
                }
                let ident = &source[start..i];
                // Property accesses (`obj.if(true) / x`, `obj.return / 2`) are never control
                // keywords or regex-preceding unary operators.
                last_ident_is_ctrl =
                    !last_was_dot && matches!(ident, "if" | "while" | "for" | "with" | "catch");
                last_allows_regex = !last_was_dot
                    && matches!(
                        ident,
                        "return"
                            | "typeof"
                            | "void"
                            | "delete"
                            | "throw"
                            | "case"
                            | "in"
                            | "of"
                            | "instanceof"
                            | "new"
                            | "yield"
                            | "await"
                            | "else"
                            | "do"
                    );
                last_was_dot = false;
                tokens.push(JsToken::Ident(ident.to_string()));
            }
            b':' => {
                tokens.push(JsToken::Colon);
                last_ident_is_ctrl = false;
                last_was_dot = false;
                last_allows_regex = true;
                i += 1;
            }
            b'@' => {
                tokens.push(JsToken::At);
                last_ident_is_ctrl = false;
                last_was_dot = false;
                last_allows_regex = false;
                i += 1;
            }
            b'=' if i + 1 < len && bytes[i + 1] == b'>' => {
                tokens.push(JsToken::Arrow);
                last_ident_is_ctrl = false;
                last_was_dot = false;
                last_allows_regex = true;
                i += 2;
            }
            _ => {
                push_js_other(&mut tokens);
                last_ident_is_ctrl = false;
                last_was_dot = false;
                last_allows_regex = !b.is_ascii_digit();
                i += 1;
            }
        }
    }
    tokens
}

pub(super) fn decode_js_string_escapes(raw: &str) -> String {
    if !raw.as_bytes().contains(&b'\\') {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' && i + 1 < len {
            let next = bytes[i + 1];
            match next {
                b'x' if i + 3 < len => {
                    if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 4])
                        && let Ok(code) = u32::from_str_radix(hex, 16)
                        && let Some(ch) = char::from_u32(code)
                    {
                        out.push(ch);
                        i += 4;
                        continue;
                    }
                }
                b'u' if i + 2 < len && bytes[i + 2] == b'{' => {
                    if let Some(close_rel) = bytes[i + 3..].iter().position(|&b| b == b'}') {
                        let hex_slice = &bytes[i + 3..i + 3 + close_rel];
                        if let Ok(hex) = std::str::from_utf8(hex_slice)
                            && let Ok(code) = u32::from_str_radix(hex, 16)
                            && let Some(ch) = char::from_u32(code)
                        {
                            out.push(ch);
                            i = i + 3 + close_rel + 1;
                            continue;
                        }
                    }
                }
                b'u' if i + 5 < len => {
                    if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 6])
                        && let Ok(code) = u32::from_str_radix(hex, 16)
                    {
                        if (0xD800..=0xDBFF).contains(&code)
                            && i + 11 < len
                            && bytes[i + 6] == b'\\'
                            && bytes[i + 7] == b'u'
                            && let Ok(low_hex) = std::str::from_utf8(&bytes[i + 8..i + 12])
                            && let Ok(low_code) = u32::from_str_radix(low_hex, 16)
                            && (0xDC00..=0xDFFF).contains(&low_code)
                        {
                            let combined =
                                0x10000 + (((code - 0xD800) << 10) | (low_code - 0xDC00));
                            if let Some(ch) = char::from_u32(combined) {
                                out.push(ch);
                                i += 12;
                                continue;
                            }
                        } else if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                            i += 6;
                            continue;
                        }
                    }
                }
                b'0'..=b'7' => {
                    // ECMAScript Annex B legacy octal escape (e.g. `\145` -> `'e'` in CommonJS
                    // `process['\145nv']`): up to 3 octal digits representing a byte <= 0o377.
                    let first = bytes[i + 1];
                    let mut code = u32::from(first - b'0');
                    let mut consumed = 2usize;
                    if i + 2 < len && matches!(bytes[i + 2], b'0'..=b'7') {
                        code = (code << 3) | u32::from(bytes[i + 2] - b'0');
                        consumed = 3;
                        if first <= b'3' && i + 3 < len && matches!(bytes[i + 3], b'0'..=b'7') {
                            code = (code << 3) | u32::from(bytes[i + 3] - b'0');
                            consumed = 4;
                        }
                    }
                    if let Some(ch) = char::from_u32(code) {
                        out.push(ch);
                    }
                    i += consumed;
                    continue;
                }
                b'b' => {
                    out.push('\x08');
                    i += 2;
                    continue;
                }
                b'f' => {
                    out.push('\x0C');
                    i += 2;
                    continue;
                }
                b'v' => {
                    out.push('\x0B');
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
                b'\n' => {
                    i += 2;
                    continue;
                }
                b'\r' => {
                    i += if i + 2 < len && bytes[i + 2] == b'\n' { 3 } else { 2 };
                    continue;
                }
                _ => {
                    if let Some(ch) = raw[i + 1..].chars().next() {
                        // ECMAScript LineContinuation also removes `\` followed by U+2028 (LS) or U+2029 (PS).
                        if matches!(ch, '\u{2028}' | '\u{2029}') {
                            i += 1 + ch.len_utf8();
                            continue;
                        }
                        out.push(ch);
                        i += 1 + ch.len_utf8();
                        continue;
                    }
                }
            }
        }
        let ch = raw[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub(super) fn scan_js_string(source: &str, quote_idx: usize, quote: u8) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let start = quote_idx + 1;
    let mut i = start;
    while i < len {
        let Some(off) = memchr::memchr3(quote, b'\\', b'\n', &bytes[i..]) else {
            break;
        };
        i += off;
        if bytes[i] == b'\\' {
            i += 1;
            if i + 1 < len && bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
                i += 2;
            } else if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
            continue;
        }
        return (decode_js_string_escapes(&source[start..i]), (i + 1).min(len));
    }
    (decode_js_string_escapes(&source[start..len]), len)
}

/// Advances from the start of a JS/TS `${...}` template expression to the byte index just past its
/// matching `}`, properly skipping nested strings, nested `` `...` `` templates, comments, and
/// token-aware regex literals (e.g. `typeof /}/`).
pub(super) fn skip_js_template_expr(source: &str, expr_start: usize, limit: usize) -> usize {
    let bytes = source.as_bytes();
    let end = limit.min(bytes.len());
    let mut q = expr_start;
    let mut depth = 1usize;
    let mut in_q: Option<u8> = None;
    let mut last_allows_regex = true;
    let mut last_ident_is_ctrl = false;
    let mut last_ident_is_block_kw = false;
    let mut last_ident_is_stmt_async = false;
    let mut in_fn_decl_header = false;
    let mut in_class_decl_header = false;
    let mut last_was_ctrl_paren = false;
    let mut last_was_stmt_block = false;
    let mut last_sig_byte: u8 = b'{';
    let mut paren_stack: Vec<bool> = Vec::new();
    let mut brace_stack: Vec<bool> = Vec::new();
    while q < end && depth > 0 {
        if let Some(iq) = in_q {
            if bytes[q] == b'\\' && q + 1 < end {
                q += 2;
                continue;
            }
            if bytes[q] == iq {
                in_q = None;
                last_allows_regex = false;
                last_ident_is_ctrl = false;
                last_ident_is_block_kw = false;
                last_was_ctrl_paren = false;
                last_was_stmt_block = false;
                last_sig_byte = b'"';
            }
            q += 1;
            continue;
        }
        if bytes[q] == b'`' {
            let (_, next_q) = scan_js_template_string(source, q);
            q = next_q.min(end);
            last_allows_regex = false;
            last_ident_is_ctrl = false;
            last_ident_is_block_kw = false;
            last_was_ctrl_paren = false;
            last_was_stmt_block = false;
            last_sig_byte = b'`';
            continue;
        }
        if matches!(bytes[q], b'"' | b'\'') {
            in_q = Some(bytes[q]);
            q += 1;
            continue;
        }
        if bytes[q] == b'/' && q + 1 < end && bytes[q + 1] == b'/' {
            let term = find_js_line_terminator(&bytes[..end], q + 2);
            if term >= end {
                q = end;
            } else if bytes[term] == b'\r' && term + 1 < end && bytes[term + 1] == b'\n' {
                q = term + 2;
            } else if bytes[term] == 0xE2 && term + 2 < end {
                q = term + 3;
            } else {
                q = term + 1;
            }
            continue;
        }
        if bytes[q] == b'/' && q + 1 < end && bytes[q + 1] == b'*' {
            q += 2;
            while q + 1 < end && !(bytes[q] == b'*' && bytes[q + 1] == b'/') {
                q += 1;
            }
            q = (q + 2).min(end);
            continue;
        }
        if bytes[q] == b'/' && last_allows_regex {
            let mut r = q + 1;
            let mut in_class = false;
            let mut closed = false;
            while r < end && bytes[r] != b'\n' && bytes[r] != b'\r' {
                if bytes[r] == b'\\' && r + 1 < end {
                    r += 2;
                    continue;
                }
                if bytes[r] == b'[' {
                    in_class = true;
                } else if bytes[r] == b']' {
                    in_class = false;
                } else if bytes[r] == b'/' && !in_class {
                    r += 1;
                    while r < end && bytes[r].is_ascii_alphabetic() {
                        r += 1;
                    }
                    closed = true;
                    break;
                }
                r += 1;
            }
            if closed {
                q = r;
                last_allows_regex = false;
                last_ident_is_ctrl = false;
                last_ident_is_block_kw = false;
                last_ident_is_stmt_async = false;
                last_was_ctrl_paren = false;
                last_was_stmt_block = false;
                last_sig_byte = b')';
                continue;
            }
        }
        if bytes[q] == b'(' {
            paren_stack.push(last_ident_is_ctrl || in_fn_decl_header);
            last_ident_is_ctrl = false;
            last_ident_is_block_kw = false;
            last_ident_is_stmt_async = false;
            in_fn_decl_header = false;
            last_was_ctrl_paren = false;
            last_was_stmt_block = false;
            last_allows_regex = true;
            last_sig_byte = b'(';
            q += 1;
            continue;
        }
        if bytes[q] == b')' {
            let is_ctrl = paren_stack.pop().unwrap_or(false);
            last_was_ctrl_paren = is_ctrl;
            last_allows_regex = is_ctrl;
            last_ident_is_ctrl = false;
            last_ident_is_block_kw = false;
            last_ident_is_stmt_async = false;
            last_was_stmt_block = false;
            last_sig_byte = b')';
            q += 1;
            continue;
        }
        if bytes[q] == b'{' {
            let is_stmt_block = last_was_ctrl_paren
                || last_was_stmt_block
                || last_ident_is_block_kw
                || in_class_decl_header
                || matches!(last_sig_byte, b';' | b'{' | b'}' | b'>');
            brace_stack.push(is_stmt_block);
            depth += 1;
            last_allows_regex = true;
            last_ident_is_ctrl = false;
            last_ident_is_block_kw = false;
            last_ident_is_stmt_async = false;
            in_fn_decl_header = false;
            in_class_decl_header = false;
            last_was_ctrl_paren = false;
            last_was_stmt_block = false;
            last_sig_byte = b'{';
            q += 1;
            continue;
        }
        if bytes[q] == b'}' {
            depth -= 1;
            q += 1;
            if depth == 0 {
                return q;
            }
            let closed_stmt_block = brace_stack.pop().unwrap_or(true);
            last_was_stmt_block = closed_stmt_block;
            last_allows_regex = closed_stmt_block;
            last_ident_is_ctrl = false;
            last_ident_is_block_kw = false;
            last_ident_is_stmt_async = false;
            in_fn_decl_header = false;
            in_class_decl_header = false;
            last_was_ctrl_paren = false;
            last_sig_byte = b'}';
            continue;
        }
        if bytes[q].is_ascii_alphabetic() || bytes[q] == b'_' || bytes[q] == b'$' {
            let start = q;
            q += 1;
            while q < end
                && (bytes[q].is_ascii_alphanumeric() || bytes[q] == b'_' || bytes[q] == b'$')
            {
                q += 1;
            }
            let ident = &source[start..q];
            let at_stmt_start =
                last_was_stmt_block || matches!(last_sig_byte, b';' | b'{' | b'}');
            if ident == "function" && (at_stmt_start || last_ident_is_stmt_async) {
                // Statement-position `[async] function f() {}` closes a statement block, allowing a regex after `}`.
                in_fn_decl_header = true;
            } else if ident == "class" && at_stmt_start {
                // Statement-position `class C [extends Base] {}` closes a statement block, allowing a regex after `}`.
                in_class_decl_header = true;
            }
            last_ident_is_stmt_async = ident == "async" && at_stmt_start;
            last_ident_is_ctrl = matches!(ident, "if" | "while" | "for" | "with" | "catch");
            last_ident_is_block_kw = matches!(ident, "else" | "do" | "try" | "finally" | "catch");
            last_allows_regex = matches!(
                ident,
                "typeof"
                    | "void"
                    | "delete"
                    | "return"
                    | "case"
                    | "throw"
                    | "in"
                    | "of"
                    | "new"
                    | "yield"
                    | "await"
                    | "else"
                    | "do"
            );
            last_was_ctrl_paren = false;
            last_was_stmt_block = false;
            last_sig_byte = b'a';
            continue;
        }
        if !bytes[q].is_ascii_whitespace() {
            last_ident_is_ctrl = false;
            last_ident_is_block_kw = false;
            last_ident_is_stmt_async = false;
            if bytes[q] != b'*' {
                in_fn_decl_header = false;
            }
            if bytes[q] == b';' {
                in_class_decl_header = false;
            }
            last_was_ctrl_paren = false;
            last_was_stmt_block = false;
            last_sig_byte = bytes[q];
            // Include division `/` as an operator that permits a following regex literal (`10 / /}/`).
            last_allows_regex = matches!(
                bytes[q],
                b'(' | b'['
                    | b'{'
                    | b','
                    | b';'
                    | b':'
                    | b'='
                    | b'!'
                    | b'&'
                    | b'|'
                    | b'?'
                    | b'+'
                    | b'-'
                    | b'*'
                    | b'/'
                    | b'%'
                    | b'^'
                    | b'~'
                    | b'<'
                    | b'>'
            );
        }
        q += 1;
    }
    q
}

pub(super) fn append_js_template_expr_tokens(
    source: &str,
    start_tick_idx: usize,
    end_idx: usize,
    out: &mut Vec<JsToken>,
) {
    let bytes = source.as_bytes();
    let body_start = start_tick_idx + 1;
    let body_end = end_idx.saturating_sub(1).min(bytes.len());
    if body_start >= body_end {
        return;
    }
    let mut p = body_start;
    while p + 1 < body_end {
        if bytes[p] == b'\\' {
            p += 2;
            continue;
        }
        if bytes[p] == b'$' && bytes[p + 1] == b'{' {
            let expr_start = p + 2;
            let after_expr = skip_js_template_expr(source, expr_start, body_end);
            let expr_end = if after_expr > expr_start && bytes.get(after_expr - 1) == Some(&b'}') {
                after_expr - 1
            } else {
                after_expr
            };
            if expr_end > expr_start {
                let expr_slice = &source[expr_start..expr_end];
                if !expr_slice.trim().is_empty() && expr_slice.len() < source.len() {
                    out.extend(tokenize_frontend(expr_slice));
                }
            }
            p = after_expr;
            continue;
        }
        p += 1;
    }
}

pub(super) fn scan_js_template_string(source: &str, quote_idx: usize) -> (String, usize) {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let start = quote_idx + 1;
    let mut i = start;
    let mut has_interpolation = false;
    while i < len {
        let Some(off) = memchr::memchr3(b'`', b'\\', b'$', &bytes[i..]) else {
            break;
        };
        i += off;
        if bytes[i] == b'\\' {
            i += 1;
            if i + 1 < len && bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
                i += 2;
            } else if i < len {
                i += source[i..].chars().next().map_or(1, char::len_utf8);
            }
            continue;
        }
        if bytes[i] == b'$' && i + 1 < len && bytes[i + 1] == b'{' {
            has_interpolation = true;
            // Skip the entire balanced `${...}` expression (including nested templates) so a
            // nested backtick inside `${`x` + import('./dep.js')}` never closes the outer template.
            i = skip_js_template_expr(source, i + 2, len);
            continue;
        }
        if bytes[i] == b'`' {
            let lit = if has_interpolation {
                String::new()
            } else {
                decode_js_string_escapes(&source[start..i])
            };
            return (lit, i + 1);
        }
        i += 1;
    }
    (String::new(), len)
}


/// Conservatively classifies whether a frontend module is strictly a pure re-export barrel
/// (`export { ... } from '...'`, `export * [as X] from '...'`, `export type { ... } from '...'`,
/// `import type { ... } from '...'`). Any ambiguous syntax (including semicolon-free `export type T = ...`)
/// conservatively returns `false` so side effects can never be hidden by ASI.
pub(super) fn frontend_tokens_are_pure_reexports(tokens: &[JsToken]) -> bool {
    let n = tokens.len();
    let mut i = 0usize;
    while i < n {
        match &tokens[i] {
            JsToken::Semi => {
                i += 1;
            }
            JsToken::Ident(kw) if kw == "export" => {
                i += 1;
                if matches!(tokens.get(i), Some(JsToken::Ident(t)) if t == "type") {
                    i += 1;
                    // Only treat explicit `export type { ... } from '...'` as pure without a
                    // full JS ASI parser; bare `export type T = ...` falls back conservatively.
                    if !matches!(tokens.get(i), Some(JsToken::LBrace)) {
                        return false;
                    }
                    i += 1;
                    while i < n && !matches!(tokens[i], JsToken::RBrace) {
                        i += 1;
                    }
                    if i >= n {
                        return false;
                    }
                    i += 1;
                    if !matches!(tokens.get(i), Some(JsToken::Ident(f)) if f == "from") {
                        return false;
                    }
                    i += 1;
                    if !matches!(tokens.get(i), Some(JsToken::StringLit(_))) {
                        return false;
                    }
                    i += 1;
                } else if matches!(tokens.get(i), Some(JsToken::Star)) {
                    i += 1;
                    if matches!(tokens.get(i), Some(JsToken::Ident(a)) if a == "as") {
                        i += 2;
                    }
                    if !matches!(tokens.get(i), Some(JsToken::Ident(f)) if f == "from") {
                        return false;
                    }
                    i += 1;
                    if !matches!(tokens.get(i), Some(JsToken::StringLit(_))) {
                        return false;
                    }
                    i += 1;
                } else if matches!(tokens.get(i), Some(JsToken::LBrace)) {
                    i += 1;
                    while i < n && !matches!(tokens[i], JsToken::RBrace) {
                        i += 1;
                    }
                    if i >= n {
                        return false;
                    }
                    i += 1;
                    if !matches!(tokens.get(i), Some(JsToken::Ident(f)) if f == "from") {
                        return false;
                    }
                    i += 1;
                    if !matches!(tokens.get(i), Some(JsToken::StringLit(_))) {
                        return false;
                    }
                    i += 1;
                } else {
                    return false;
                }
            }
            JsToken::Ident(kw) if kw == "import" => {
                if !matches!(tokens.get(i + 1), Some(JsToken::Ident(t)) if t == "type") {
                    return false;
                }
                i += 2;
                if !matches!(tokens.get(i), Some(JsToken::LBrace)) {
                    return false;
                }
                i += 1;
                while i < n && !matches!(tokens[i], JsToken::RBrace) {
                    i += 1;
                }
                if i >= n {
                    return false;
                }
                i += 1;
                if !matches!(tokens.get(i), Some(JsToken::Ident(f)) if f == "from") {
                    return false;
                }
                i += 1;
                if !matches!(tokens.get(i), Some(JsToken::StringLit(_))) {
                    return false;
                }
                i += 1;
            }
            _ => return false,
        }
    }
    true
}

/// Returns `true` if a frontend module has top-level executable statements outside pure
/// `import` / `export` / `function` / `class` / `type` / `interface` / pure `const` function or
/// literal declarations (e.g. top-level `configure();` or `globalThis.x = 1;`).
pub(super) fn frontend_tokens_have_top_level_side_effects(tokens: &[JsToken]) -> bool {
    let n = tokens.len();
    let mut i = 0usize;
    let skip_balanced = |mut pos: usize| -> usize {
        let mut depth = 0usize;
        while pos < n {
            match &tokens[pos] {
                JsToken::LBrace | JsToken::LParen | JsToken::LBracket => {
                    depth += 1;
                    pos += 1;
                }
                JsToken::RBrace | JsToken::RParen | JsToken::RBracket => {
                    depth = depth.saturating_sub(1);
                    pos += 1;
                    if depth == 0 {
                        return pos;
                    }
                }
                _ => pos += 1,
            }
        }
        pos
    };
    while i < n {
        match &tokens[i] {
            JsToken::Semi => {
                i += 1;
            }
            JsToken::Ident(kw) if kw == "import" => {
                i += 1;
                // Side-effect-only `import './polyfill'`, `import.meta...`, and dynamic `import(...)`
                // are executable expressions/side effects, NOT pure static binding imports.
                if matches!(
                    tokens.get(i),
                    Some(JsToken::StringLit(_) | JsToken::Dot | JsToken::LParen)
                ) {
                    return true;
                }
                while i < n {
                    if matches!(tokens[i], JsToken::StringLit(_)) {
                        i += 1;
                        break;
                    }
                    if matches!(tokens[i], JsToken::Semi) {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            JsToken::Ident(kw) if kw == "export" => {
                i += 1;
                if matches!(tokens.get(i), Some(JsToken::Ident(d)) if d == "default") {
                    i += 1;
                }
                // `export { ... } [from '...']` or `export * [as X] from '...'`
                if matches!(tokens.get(i), Some(JsToken::LBrace)) {
                    i = skip_balanced(i);
                    if matches!(tokens.get(i), Some(JsToken::Ident(f)) if f == "from") {
                        i += 2;
                    }
                } else if matches!(tokens.get(i), Some(JsToken::Star)) {
                    i += 1;
                    if matches!(tokens.get(i), Some(JsToken::Ident(a)) if a == "as") {
                        i += 2;
                    }
                    if matches!(tokens.get(i), Some(JsToken::Ident(f)) if f == "from") {
                        i += 2;
                    }
                }
                // Otherwise let the next iteration inspect `function`/`class`/`const`/`type`/`interface`.
            }
            JsToken::Ident(kw)
                if matches!(kw.as_str(), "async" | "declare" | "abstract") =>
            {
                i += 1;
            }
            JsToken::Ident(kw) if matches!(kw.as_str(), "function" | "interface") => {
                i += 1;
                while i < n && !matches!(tokens[i], JsToken::LBrace | JsToken::Semi) {
                    i += 1;
                }
                if i < n && matches!(tokens[i], JsToken::LBrace) {
                    i = skip_balanced(i);
                } else if i < n {
                    i += 1;
                }
            }
            JsToken::Ident(kw) if kw == "class" => {
                // Conservatively classify classes with `extends` (evaluated on import) or `static`
                // fields/blocks (`static value = register();`) as side-effectful.
                i += 1;
                while i < n && !matches!(tokens[i], JsToken::LBrace | JsToken::Semi) {
                    if matches!(&tokens[i], JsToken::Ident(id) if id == "extends") {
                        return true;
                    }
                    i += 1;
                }
                if i < n && matches!(tokens[i], JsToken::LBrace) {
                    let body_start = i + 1;
                    let body_end = skip_balanced(i);
                    // Reject `static` members/blocks, computed keys (`[expr]() {}`), and ANY
                    // decorator (`@register() method() {}` OR parameter decorator
                    // `constructor(@Inject() dep: T) {}`) inside the class body.
                    let mut cdepth = 0usize;
                    for tok in &tokens[body_start..body_end.saturating_sub(1)] {
                        match tok {
                            JsToken::At => return true,
                            JsToken::Ident(id) if id == "static" && cdepth == 0 => return true,
                            JsToken::LBracket if cdepth == 0 => return true,
                            JsToken::LBrace | JsToken::LParen | JsToken::LBracket => cdepth += 1,
                            JsToken::RBrace | JsToken::RParen | JsToken::RBracket => {
                                cdepth = cdepth.saturating_sub(1);
                            }
                            _ => {}
                        }
                    }
                    i = body_end;
                } else {
                    return true;
                }
            }
            JsToken::Ident(kw) if kw == "type" => {
                // Only `type { ... } from '...'` has a guaranteed token boundary without ASI;
                // bare `type T = ...` (which may lack `;` before a side-effectful call on the
                // next line) conservatively returns `true`.
                if matches!(tokens.get(i + 1), Some(JsToken::LBrace)) {
                    i = skip_balanced(i + 1);
                    if matches!(tokens.get(i), Some(JsToken::Ident(f)) if f == "from") {
                        i += 2;
                    } else {
                        return true;
                    }
                } else {
                    return true;
                }
            }
            JsToken::Ident(kw) if kw == "const" => {
                // Pure `const X = (...) => { ... }` or `const X = function ...` or literal value;
                // a top-level call like `const X = init()` conservatively returns `true`.
                i += 1;
                if !matches!(tokens.get(i), Some(JsToken::Ident(_))) {
                    return true;
                }
                i += 1;
                // Skip optional TS type annotation `: ...` until `=` (Other)
                while i < n && !matches!(tokens[i], JsToken::Other | JsToken::Semi) {
                    if matches!(tokens[i], JsToken::LParen | JsToken::LBrace | JsToken::LBracket) {
                        i = skip_balanced(i);
                    } else {
                        i += 1;
                    }
                }
                if !matches!(tokens.get(i), Some(JsToken::Other)) {
                    return true;
                }
                i += 1;
                // Check RHS: allow `async (...) => ...`, `(...) => ...`, `function ...`, string/object/array literal
                if matches!(tokens.get(i), Some(JsToken::Ident(a)) if a == "async") {
                    i += 1;
                }
                match tokens.get(i) {
                    Some(JsToken::Ident(f)) if f == "function" => {
                        while i < n && !matches!(tokens[i], JsToken::LBrace) {
                            i += 1;
                        }
                        if i < n {
                            i = skip_balanced(i);
                        }
                    }
                    Some(JsToken::LParen) => {
                        let after_params = skip_balanced(i);
                        // Require an actual `=>` (`JsToken::Arrow`) followed by a `{ ... }` block
                        // body so `(register()) + 1` is never mistaken for an arrow and ASI after
                        // a single-expression arrow (`const f = () => 1\nregister()`) cannot hide calls.
                        if matches!(tokens.get(after_params), Some(JsToken::Arrow))
                            && matches!(tokens.get(after_params + 1), Some(JsToken::LBrace))
                        {
                            i = skip_balanced(after_params + 1);
                        } else {
                            return true;
                        }
                    }
                    Some(JsToken::StringLit(_)) => {
                        i += 1;
                    }
                    _ => return true,
                }
            }
            _ => return true,
        }
    }
    false
}


pub(super) fn parse_js_import_clause(
    tokens: &[JsToken],
    mut i: usize,
) -> (Option<ImportSpec>, bool, usize) {
    let n = tokens.len();
    // Side-effect import: `import './styles.css'`
    if let Some(JsToken::StringLit(spec)) = tokens.get(i) {
        let parsed = clean_frontend_import_specifier(spec).map(|module| ImportSpec {
            level: 0,
            module,
            symbols: Vec::new(),
        });
        return (parsed, false, i + 1);
    }

    let mut top_type_only = false;
    // Distinguish `import type Foo from './x'` / `import type { Foo } from './x'` from
    // default runtime binding `import type, { x } from './x'` and `import type from './x'`.
    if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type")
        && !matches!(
            tokens.get(i + 1),
            Some(JsToken::Ident(f)) if f == "from"
        )
        && !matches!(tokens.get(i + 1), Some(JsToken::Comma))
    {
        top_type_only = true;
        i += 1;
    }

    let mut value_symbols = BTreeSet::new();
    let mut type_symbols = BTreeSet::new();

    while i < n {
        match &tokens[i] {
            JsToken::Ident(kw) if kw == "from" => {
                if let Some(JsToken::StringLit(spec)) = tokens.get(i + 1) {
                    // Only top-level `import type ...` is guaranteed erased across all TS configs;
                    // under `verbatimModuleSyntax`, `import { type T } from './effect'` compiles to
                    // `import {} from './effect'` which still evaluates `./effect` at runtime.
                    let is_type_only = top_type_only;
                    let syms = if is_type_only {
                        type_symbols.into_iter().collect()
                    } else {
                        value_symbols.into_iter().collect()
                    };
                    let parsed = clean_frontend_import_specifier(spec).map(|module| ImportSpec {
                        level: 0,
                        module,
                        symbols: syms,
                    });
                    return (parsed, is_type_only, i + 2);
                }
                return (None, false, i + 1);
            }
            JsToken::Star => {
                if top_type_only {
                    type_symbols.insert("*".to_string());
                } else {
                    value_symbols.insert("*".to_string());
                }
                i += 1;
                if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as") {
                    i += 2;
                }
            }
            JsToken::LBrace => {
                i += 1;
                while i < n && !matches!(tokens[i], JsToken::RBrace) {
                    let mut item_type_only = top_type_only;
                    // Disambiguate contextual `import { type as foo }` (runtime binding `type`
                    // renamed to `foo`) from inline type modifiers `import { type Foo }`,
                    // `import { type as }`, and `import { type as as foo }`.
                    let is_inline_type_mod = matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type")
                        && match tokens.get(i + 1) {
                            Some(JsToken::Ident(next)) if next != "as" => true,
                            Some(JsToken::Ident(next)) if next == "as" => {
                                // `type as }` or `type as ,` or `type as as foo` -> modifier!
                                // `type as foo` (where foo != "as") -> runtime binding named `type`!
                                match tokens.get(i + 2) {
                                    // `type as as foo` -> type modifier; `type as as }` -> runtime `type` aliased to `as`!
                                    Some(JsToken::Ident(third)) if third == "as" => {
                                        matches!(tokens.get(i + 3), Some(JsToken::Ident(_)))
                                    }
                                    Some(JsToken::Ident(_)) => false,
                                    _ => true,
                                }
                            }
                            _ => false,
                        };
                    if is_inline_type_mod {
                        item_type_only = true;
                        i += 1;
                    }
                    if let Some(JsToken::Ident(orig_sym)) = tokens.get(i) {
                        let sym_to_record = orig_sym.clone();
                        i += 1;
                        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as")
                            && matches!(tokens.get(i + 1), Some(JsToken::Ident(_)))
                        {
                            i += 2;
                        }
                        if item_type_only {
                            type_symbols.insert(sym_to_record);
                        } else {
                            value_symbols.insert(sym_to_record);
                        }
                    } else {
                        i += 1;
                    }
                    if matches!(tokens.get(i), Some(JsToken::Comma)) {
                        i += 1;
                    }
                }
                if i < n && matches!(tokens[i], JsToken::RBrace) {
                    i += 1;
                }
            }
            JsToken::Ident(default_ident) => {
                let _ = default_ident;
                if top_type_only {
                    type_symbols.insert("default".to_string());
                } else {
                    value_symbols.insert("default".to_string());
                }
                i += 1;
            }
            JsToken::Comma => {
                i += 1;
            }
            JsToken::Semi => {
                return (None, false, i + 1);
            }
            _ => {
                return (None, false, i + 1);
            }
        }
    }
    (None, false, i)
}

pub(super) fn parse_js_export_clause(
    tokens: &[JsToken],
    mut i: usize,
) -> (Option<ImportSpec>, bool, Vec<String>, Vec<String>, usize) {
    let n = tokens.len();
    let mut defined = Vec::new();
    let mut reexport_links = Vec::new();
    let mut top_type_only = false;

    if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type") {
        if matches!(tokens.get(i + 1), Some(JsToken::LBrace | JsToken::Star)) {
            top_type_only = true;
            i += 1;
        } else if let Some(JsToken::Ident(type_name)) = tokens.get(i + 1) {
            defined.push(type_name.clone());
            return (None, true, defined, reexport_links, i + 2);
        }
    }

    if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "default") {
        defined.push("default".to_string());
        i += 1;
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "function" || kw == "class")
            && let Some(JsToken::Ident(name)) = tokens.get(i + 1)
        {
            defined.push(name.clone());
            return (None, false, defined, reexport_links, i + 2);
        }
        return (None, false, defined, reexport_links, i);
    }

    if let Some(JsToken::Ident(kw)) = tokens.get(i) {
        if kw == "async"
            && matches!(tokens.get(i + 1), Some(JsToken::Ident(f)) if f == "function")
            && let Some(JsToken::Ident(name)) = tokens.get(i + 2)
        {
            defined.push(name.clone());
            return (None, false, defined, reexport_links, i + 3);
        }
        if matches!(
            kw.as_str(),
            "function" | "class" | "const" | "let" | "var" | "interface" | "enum"
        ) && let Some(JsToken::Ident(name)) = tokens.get(i + 1)
        {
            defined.push(name.clone());
            return (None, false, defined, reexport_links, i + 2);
        }
    }

    // `export * from './sub'` or `export * as ns from './sub'`
    if matches!(tokens.get(i), Some(JsToken::Star)) {
        i += 1;
        let mut alias_name: Option<String> = None;
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as")
            && let Some(JsToken::Ident(ns)) = tokens.get(i + 1)
        {
            alias_name = Some(ns.clone());
            defined.push(ns.clone());
            i += 2;
        }
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "from")
            && let Some(JsToken::StringLit(spec)) = tokens.get(i + 1)
        {
            if let Some(clean) = clean_frontend_import_specifier(spec) {
                if let Some(ns) = alias_name
                    && !top_type_only
                {
                    reexport_links.push(format!("reexport:{ns}->{clean}:*"));
                }
                return (
                    Some(ImportSpec {
                        level: 0,
                        module: clean,
                        symbols: vec!["*".to_string()],
                    }),
                    top_type_only,
                    defined,
                    reexport_links,
                    i + 2,
                );
            }
            return (None, top_type_only, defined, reexport_links, i + 2);
        }
        return (None, top_type_only, defined, reexport_links, i);
    }

    // `export { A, B as C, type D } from './sub'` or local `export { A, B }`
    if matches!(tokens.get(i), Some(JsToken::LBrace)) {
        i += 1;
        let mut pairs: Vec<(String, String, bool)> = Vec::new();
        while i < n && !matches!(tokens[i], JsToken::RBrace) {
            let mut item_type = top_type_only;
            // Disambiguate contextual `export { type as foo }` from `export { type Foo }`,
            // `export { type as }`, and `export { type as as foo }`.
            let is_inline_type_mod = matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "type")
                && match tokens.get(i + 1) {
                    Some(JsToken::Ident(next)) if next != "as" => true,
                    Some(JsToken::Ident(next)) if next == "as" => match tokens.get(i + 2) {
                        Some(JsToken::Ident(third)) if third == "as" => {
                            matches!(tokens.get(i + 3), Some(JsToken::Ident(_)))
                        }
                        Some(JsToken::Ident(_)) => false,
                        _ => true,
                    },
                    _ => false,
                };
            if is_inline_type_mod {
                item_type = true;
                i += 1;
            }
            if let Some(JsToken::Ident(orig)) = tokens.get(i) {
                let orig_name = orig.clone();
                let mut exported_name = orig_name.clone();
                i += 1;
                if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "as")
                    && let Some(JsToken::Ident(alias)) = tokens.get(i + 1)
                {
                    exported_name = alias.clone();
                    i += 2;
                }
                defined.push(exported_name.clone());
                pairs.push((orig_name, exported_name, item_type));
            } else {
                i += 1;
            }
            if matches!(tokens.get(i), Some(JsToken::Comma)) {
                i += 1;
            }
        }
        if i < n && matches!(tokens[i], JsToken::RBrace) {
            i += 1;
        }
        if matches!(tokens.get(i), Some(JsToken::Ident(kw)) if kw == "from")
            && let Some(JsToken::StringLit(spec)) = tokens.get(i + 1)
        {
            if let Some(clean) = clean_frontend_import_specifier(spec) {
                let value_pairs: Vec<_> = pairs.iter().filter(|(_, _, is_t)| !*is_t).collect();
                // Reserve type-only classification strictly for top-level `export type { ... }`;
                // under `verbatimModuleSyntax`, `export { type T } from './effect'` compiles to
                // `export {} from './effect'` which still evaluates `./effect` at runtime.
                let is_type_only = top_type_only;
                let syms: Vec<String> = if is_type_only {
                    pairs.iter().map(|(orig, _, _)| orig.clone()).collect()
                } else {
                    for (orig, exp, _) in &value_pairs {
                        reexport_links.push(format!("reexport:{exp}->{clean}:{orig}"));
                    }
                    value_pairs.iter().map(|(orig, _, _)| orig.clone()).collect()
                };
                return (
                    Some(ImportSpec {
                        level: 0,
                        module: clean,
                        symbols: syms,
                    }),
                    is_type_only,
                    defined,
                    reexport_links,
                    i + 2,
                );
            }
            return (None, top_type_only, defined, reexport_links, i + 2);
        }
        return (None, top_type_only, defined, reexport_links, i);
    }

    (None, false, defined, reexport_links, i)
}


