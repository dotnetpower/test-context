use super::*;

/// UTF-8-safe extractor for Fortran `INCLUDE 'file'` / `INCLUDE "file"` directives.
/// Parses the quoted filename before comment scanning so `!` inside `'constants!dev.inc'`
/// is preserved verbatim instead of truncating the include path.
pub(super) fn extract_fortran_includes(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in source.lines() {
        let t = raw_line.trim_start();
        let tb = t.as_bytes();
        if tb.len() >= 8
            && tb[..7].eq_ignore_ascii_case(b"include")
            && tb[7].is_ascii_whitespace()
        {
            let rest = t[7..].trim_start();
            if let Some(&qbyte) = rest.as_bytes().first()
                && matches!(qbyte, b'\'' | b'"')
            {
                let q = qbyte as char;
                if let Some(end_q) = rest[1..].find(q) {
                    let inc = rest[1..1 + end_q].trim();
                    if !inc.is_empty() {
                        out.push(inc.to_string());
                    }
                }
            }
        }
    }
    out
}

pub(super) fn splice_c_continuation_lines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("\\\n") && !text.contains("\\\r\n") {
        return std::borrow::Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' {
            if i + 1 < len && bytes[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 < len && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                i += 3;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    std::borrow::Cow::Owned(out)
}

pub(super) const DYNAMIC_MACRO_INCLUDE_SENTINEL: &str = "__TCTX_DYNAMIC_MACRO_INCLUDE__";

pub(super) fn normalize_lexical_rel_allow_root(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::Normal(os) => {
                parts.push(os.to_string_lossy().into_owned());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

pub(super) fn split_cgo_flag_tokens(s: &str) -> Vec<String> {
    // Go-compatible flag tokenizer supporting single quotes, double quotes, and `\` escapes
    // inside BOTH quote types (matching Go's `internal/quoted.Split`).
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == '\\' {
                if let Some(next_ch) = chars.next() {
                    cur.push(next_ch);
                }
            } else if ch == q {
                quote = None;
            } else {
                cur.push(ch);
            }
            continue;
        }
        if ch.is_ascii_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == '\\' {
            if let Some(next_ch) = chars.next() {
                cur.push(next_ch);
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Lexically scans Go source to extract the COMPLETE contiguous comment group (combining
/// adjacent `// ...` and `/* ... */` comments with no blank line between them) immediately
/// preceding the real `import "C"` OR grouped `import ( ... "C" ... )` declaration, ignoring
/// any `import "C"` text inside comments or strings.
pub(super) fn join_fortran_continuations(source: &str, fixed_form: bool) -> String {
    if fixed_form {
        let mut joined = String::with_capacity(source.len());
        let mut has_prev = false;
        let mut in_sq = false;
        let mut in_dq = false;
        for raw_fline in source.lines() {
            let first_ch = raw_fline.chars().next();
            if matches!(first_ch, Some('c' | 'C' | '*' | '!')) || raw_fline.trim().is_empty() {
                continue;
            }
            let chars: Vec<char> = raw_fline.chars().collect();
            let is_cont = chars.len() >= 6
                && chars[..5].iter().all(|c| *c == ' ')
                && chars[5] != ' '
                && chars[5] != '0';
            if !is_cont {
                in_sq = false;
                in_dq = false;
            }
            let body: String = if chars.len() > 6 {
                chars[6..chars.len().min(72)].iter().collect()
            } else {
                String::new()
            };
            let mut code_end = body.len();
            let mut bchars = body.char_indices().peekable();
            let started_in_quote = in_sq || in_dq;
            while let Some((bi, ch)) = bchars.next() {
                match ch {
                    '\'' if !in_dq => {
                        if in_sq && bchars.peek().is_some_and(|&(_, nc)| nc == '\'') {
                            bchars.next();
                        } else {
                            in_sq = !in_sq;
                        }
                    }
                    '"' if !in_sq => {
                        if in_dq && bchars.peek().is_some_and(|&(_, nc)| nc == '"') {
                            bchars.next();
                        } else {
                            in_dq = !in_dq;
                        }
                    }
                    '!' if !in_sq && !in_dq => {
                        code_end = bi;
                        break;
                    }
                    _ => {}
                }
            }
            let raw_code = &body[..code_end];
            let code_part = if in_sq || in_dq {
                raw_code
            } else {
                raw_code.trim_end()
            };
            if is_cont && has_prev {
                if started_in_quote {
                    joined.push_str(code_part);
                } else {
                    joined.push_str(code_part.trim_start());
                }
            } else {
                if has_prev {
                    joined.push('\n');
                }
                joined.push_str(code_part);
                has_prev = true;
            }
        }
        if has_prev {
            joined.push('\n');
        }
        return joined;
    }
    let mut joined = String::with_capacity(source.len());
    let mut continuing = false;
    let mut in_sq = false;
    let mut in_dq = false;
    for raw_fline in source.lines() {
        if continuing {
            let trimmed = raw_fline.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('!') {
                continue;
            }
        }
        let mut seg = raw_fline;
        if continuing {
            let trimmed = seg.trim_start();
            if let Some(after_amp) = trimmed.strip_prefix('&') {
                seg = after_amp;
            } else if !in_sq && !in_dq {
                joined.push(' ');
                seg = trimmed;
            }
        }
        let mut code_end = seg.len();
        let mut chars = seg.char_indices().peekable();
        while let Some((bi, ch)) = chars.next() {
            match ch {
                '\'' if !in_dq => {
                    if in_sq && chars.peek().is_some_and(|&(_, nc)| nc == '\'') {
                        chars.next();
                    } else {
                        in_sq = !in_sq;
                    }
                }
                '"' if !in_sq => {
                    if in_dq && chars.peek().is_some_and(|&(_, nc)| nc == '"') {
                        chars.next();
                    } else {
                        in_dq = !in_dq;
                    }
                }
                '!' if !in_sq && !in_dq => {
                    code_end = bi;
                    break;
                }
                _ => {}
            }
        }
        let code_part = seg[..code_end].trim_end();
        if let Some(before_amp) = code_part.strip_suffix('&') {
            joined.push_str(before_amp);
            continuing = true;
        } else {
            joined.push_str(code_part);
            joined.push('\n');
            continuing = false;
            in_sq = false;
            in_dq = false;
        }
    }
    joined
}

pub(super) fn scan_fortran_logical_source(
    joined: &str,
    imports: &mut BTreeSet<ImportSpec>,
    defined_symbols: &mut BTreeSet<String>,
) {
    if joined
        .as_bytes()
        .windows(7)
        .any(|w| w.eq_ignore_ascii_case(b"include"))
    {
        for inc in extract_fortran_includes(joined) {
            defined_symbols.insert(format!("c_include:{inc}"));
            imports.insert(ImportSpec {
                level: 0,
                module: inc,
                symbols: Vec::new(),
            });
        }
    }
    for raw_fline in joined.lines() {
        let mut stmts: Vec<&str> = Vec::new();
        let mut start = 0usize;
        let mut in_sq = false;
        let mut in_dq = false;
        for (bi, ch) in raw_fline.char_indices() {
            match ch {
                '\'' if !in_dq => in_sq = !in_sq,
                '"' if !in_sq => in_dq = !in_dq,
                ';' if !in_sq && !in_dq => {
                    stmts.push(&raw_fline[start..bi]);
                    start = bi + 1;
                }
                _ => {}
            }
        }
        stmts.push(&raw_fline[start..]);
        let mut found_dyn_mod = false;
        for stmt in stmts {
            let code = stmt.trim();
            if code.len() < 4 {
                continue;
            }
            let lower = code.to_ascii_lowercase();
            let after_use = if let Some(rest) = lower.strip_prefix("use") {
                if rest.starts_with(|c: char| c.is_ascii_whitespace() || c == ',')
                    || rest.starts_with("::")
                {
                    rest.trim_start()
                } else {
                    continue;
                }
            } else {
                continue;
            };
            let is_intrinsic_nature = if let Some((attrs, _mod_part)) = after_use.split_once("::") {
                attrs.trim().trim_start_matches(',').trim() == "intrinsic"
            } else {
                false
            };
            if !is_intrinsic_nature {
                defined_symbols.insert("fortran_dynamic_mod".to_string());
                found_dyn_mod = true;
                break;
            }
        }
        if found_dyn_mod {
            break;
        }
    }
    let code = joined.to_ascii_lowercase();
    let bytes = code.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        if bytes[pos] == b'\'' || bytes[pos] == b'"' {
            let q = bytes[pos];
            pos += 1;
            while pos < bytes.len() {
                if bytes[pos] == q {
                    if pos + 1 < bytes.len() && bytes[pos + 1] == q {
                        pos += 2;
                        continue;
                    }
                    pos += 1;
                    break;
                }
                pos += 1;
            }
            continue;
        }
        if bytes[pos].is_ascii_alphabetic() || bytes[pos] == b'_' {
            let s = pos;
            while pos < bytes.len()
                && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_')
            {
                pos += 1;
            }
            let tok = &code[s..pos];
            let rest = code[pos..].trim_start();
            if rest.starts_with('(')
                && matches!(
                    tok,
                    "open"
                        | "inquire"
                        | "get_environment_variable"
                        | "getenv"
                        | "get_command_argument"
                        | "getarg"
                        | "iargc"
                        | "execute_command_line"
                        | "system"
                        | "systemqq"
                        | "runqq"
                        | "getcwd"
                        | "chdir"
                )
            {
                defined_symbols.insert("cgo_dynamic_io".to_string());
                break;
            }
        } else {
            pos += 1;
        }
    }
}

/// Detects direct cgo runtime env/I/O/process/dlopen references in Go code outside comments and
/// string literals (e.g. `C.getenv(...)`, `C.fopen(...)`, `C.open(...)`, `C.system(...)`).
pub(super) fn go_cgo_body_has_runtime_io_or_env(source: &str) -> bool {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    while pos < len {
        if bytes[pos] == b'/' && pos + 1 < len && bytes[pos + 1] == b'/' {
            while pos < len && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        if bytes[pos] == b'/' && pos + 1 < len && bytes[pos + 1] == b'*' {
            pos += 2;
            while pos + 1 < len && !(bytes[pos] == b'*' && bytes[pos + 1] == b'/') {
                pos += 1;
            }
            pos = (pos + 2).min(len);
            continue;
        }
        if bytes[pos] == b'`' {
            pos += 1;
            while pos < len && bytes[pos] != b'`' {
                pos += 1;
            }
            pos = (pos + 1).min(len);
            continue;
        }
        if bytes[pos] == b'"' || bytes[pos] == b'\'' {
            let q = bytes[pos];
            pos += 1;
            while pos < len && bytes[pos] != q {
                if bytes[pos] == b'\\' && pos + 1 < len {
                    pos += 2;
                    continue;
                }
                pos += 1;
            }
            pos = (pos + 1).min(len);
            continue;
        }
        if bytes[pos].is_ascii_alphabetic() || bytes[pos] == b'_' {
            let start = pos;
            while pos < len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
            }
            if &source[start..pos] == "C"
                && !source[..start].trim_end().ends_with('.')
            {
                let mut look = pos;
                while look < len {
                    if bytes[look].is_ascii_whitespace() {
                        look += 1;
                    } else if bytes[look] == b'/' && look + 1 < len && bytes[look + 1] == b'/' {
                        while look < len && bytes[look] != b'\n' {
                            look += 1;
                        }
                    } else if bytes[look] == b'/' && look + 1 < len && bytes[look + 1] == b'*' {
                        look += 2;
                        while look + 1 < len && !(bytes[look] == b'*' && bytes[look + 1] == b'/') {
                            look += 1;
                        }
                        look = (look + 2).min(len);
                    } else {
                        break;
                    }
                }
                if look < len && bytes[look] == b'.' {
                    look += 1;
                    while look < len {
                        if bytes[look].is_ascii_whitespace() {
                            look += 1;
                        } else if bytes[look] == b'/' && look + 1 < len && bytes[look + 1] == b'/' {
                            while look < len && bytes[look] != b'\n' {
                                look += 1;
                            }
                        } else if bytes[look] == b'/' && look + 1 < len && bytes[look + 1] == b'*' {
                            look += 2;
                            while look + 1 < len && !(bytes[look] == b'*' && bytes[look + 1] == b'/')
                            {
                                look += 1;
                            }
                            look = (look + 2).min(len);
                        } else {
                            break;
                        }
                    }
                    let id_start = look;
                    while look < len
                        && (bytes[look].is_ascii_alphanumeric() || bytes[look] == b'_')
                    {
                        look += 1;
                    }
                    let member = &source[id_start..look];
                    if matches!(
                        member,
                        "getenv"
                            | "secure_getenv"
                            | "getenv_s"
                            | "_dupenv_s"
                            | "environ"
                            | "fopen"
                            | "fopen64"
                            | "freopen"
                            | "open"
                            | "open64"
                            | "openat"
                            | "open_by_handle_at"
                            | "popen"
                            | "_popen"
                            | "_wpopen"
                            | "system"
                            | "syscall"
                            | "__syscall"
                            | "dlopen"
                            | "dlmopen"
                            | "dlsym"
                            | "dlvsym"
                            | "LoadLibrary"
                            | "LoadLibraryA"
                            | "LoadLibraryW"
                            | "LoadLibraryEx"
                            | "LoadLibraryExA"
                            | "LoadLibraryExW"
                            | "execl"
                            | "execlp"
                            | "execle"
                            | "execv"
                            | "execvp"
                            | "execvpe"
                            | "execve"
                            | "fexecve"
                            | "posix_spawn"
                            | "posix_spawnp"
                            | "stat"
                            | "stat64"
                            | "lstat"
                            | "lstat64"
                            | "fstatat"
                            | "statx"
                            | "statfs"
                            | "statfs64"
                            | "statvfs"
                            | "statvfs64"
                            | "access"
                            | "faccessat"
                            | "opendir"
                            | "readdir"
                            | "readdir64"
                            | "scandir"
                            | "scandir64"
                            | "scandirat"
                            | "glob"
                            | "glob64"
                            | "ftw"
                            | "ftw64"
                            | "nftw"
                            | "nftw64"
                            | "fts_open"
                            | "wordexp"
                            | "pathconf"
                            | "fpathconf"
                            | "confstr"
                            | "gethostname"
                            | "uname"
                            | "realpath"
                            | "readlink"
                            | "readlinkat"
                            | "getcwd"
                            | "chdir"
                            | "fchdir"
                            | "CreateFile"
                            | "CreateFileA"
                            | "CreateFileW"
                            | "GetEnvironmentVariable"
                            | "GetEnvironmentVariableA"
                            | "GetEnvironmentVariableW"
                            | "FindFirstFile"
                            | "FindFirstFileA"
                            | "FindFirstFileW"
                    ) {
                        return true;
                    }
                }
            }
            continue;
        }
        pos += 1;
    }
    false
}

pub(super) fn c_source_has_runtime_io(raw_source: &str) -> bool {
    // Splice C preprocessor `\\\n` / `\\\r\n` physical line continuations BEFORE token scanning
    // so `fop\\\nen(...)` is recognized as `fopen(...)`!
    let spliced = splice_c_continuation_lines(raw_source);
    let source: &str = spliced.as_ref();
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    while pos < len {
        if bytes[pos] == b'/' && pos + 1 < len && bytes[pos + 1] == b'/' {
            while pos < len && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        if bytes[pos] == b'/' && pos + 1 < len && bytes[pos + 1] == b'*' {
            pos += 2;
            while pos + 1 < len && !(bytes[pos] == b'*' && bytes[pos + 1] == b'/') {
                pos += 1;
            }
            pos = (pos + 2).min(len);
            continue;
        }
        // Skip C++ raw string literals (`R"delim(...)delim"`, `u8R"..."`, `uR"..."`, `UR"..."`,
        // `LR"..."`) BEFORE ordinary string/comment scanning so `R"xyz(" /*)xyz"` cannot confuse
        // the comment scanner and hide later runtime I/O calls!
        if bytes[pos] == b'R' && pos + 1 < len && bytes[pos + 1] == b'"' {
            let valid_raw_prefix = pos == 0
                || !(bytes[pos - 1].is_ascii_alphanumeric() || bytes[pos - 1] == b'_')
                || (pos >= 1
                    && matches!(bytes[pos - 1], b'L' | b'u' | b'U')
                    && (pos == 1
                        || !(bytes[pos - 2].is_ascii_alphanumeric() || bytes[pos - 2] == b'_')))
                || (pos >= 2
                    && &bytes[pos - 2..pos] == b"u8"
                    && (pos == 2
                        || !(bytes[pos - 3].is_ascii_alphanumeric() || bytes[pos - 3] == b'_')));
            if valid_raw_prefix {
                let delim_start = pos + 2;
                let mut dpos = delim_start;
                while dpos < len
                    && bytes[dpos] != b'('
                    && !bytes[dpos].is_ascii_whitespace()
                    && bytes[dpos] != b'\\'
                    && bytes[dpos] != b')'
                    && dpos - delim_start <= 16
                {
                    dpos += 1;
                }
                if dpos < len && bytes[dpos] == b'(' {
                    let delim = &bytes[delim_start..dpos];
                    let mut scan = dpos + 1;
                    let mut closed = false;
                    while scan < len {
                        if bytes[scan] == b')'
                            && scan + 1 + delim.len() < len
                            && &bytes[scan + 1..scan + 1 + delim.len()] == delim
                            && bytes[scan + 1 + delim.len()] == b'"'
                        {
                            pos = scan + 2 + delim.len();
                            closed = true;
                            break;
                        }
                        scan += 1;
                    }
                    if closed {
                        continue;
                    }
                }
            }
        }
        if bytes[pos] == b'"' || bytes[pos] == b'\'' {
            let q = bytes[pos];
            pos += 1;
            while pos < len && bytes[pos] != q {
                if bytes[pos] == b'\\' && pos + 1 < len {
                    pos += 1;
                    let ch = source[pos..].chars().next().unwrap_or(' ');
                    pos += ch.len_utf8();
                    continue;
                }
                let ch = source[pos..].chars().next().unwrap_or(' ');
                pos += ch.len_utf8();
            }
            if pos < len {
                pos += 1;
            }
            continue;
        }
        if bytes[pos].is_ascii_alphabetic() || bytes[pos] == b'_' {
            let s = pos;
            while pos < len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
            }
            let tok = &source[s..pos];
            let prev_non_ws = source[..s].trim_end();
            let line_start = source[..s].rfind('\n').map_or(0, |nl| nl + 1);
            let line_prefix = source[line_start..s].trim_start();
            // Detect `#define MY_OPEN open` with arbitrary preprocessor whitespace (`#  define`,
            // `#\tdefine`, and `\`-continued macro bodies) as well as escaped function-pointer
            // values (`p = open`, `cond ? open : fallback`, `return open`)!
            let in_define_directive = line_prefix
                .strip_prefix('#')
                .map(str::trim_start)
                .is_some_and(|r| {
                    r.strip_prefix("define")
                        .is_some_and(|tail| tail.is_empty() || tail.starts_with(|c: char| c.is_ascii_whitespace()))
                })
                || (line_start >= 2
                    && source[..line_start].trim_end().ends_with('\\'));
            let is_member_access = prev_non_ws.ends_with('.') || prev_non_ws.ends_with("->");
            let is_type_tag = prev_non_ws.ends_with("struct")
                || prev_non_ws.ends_with("union")
                || prev_non_ws.ends_with("enum");
            // Skip ASCII whitespace AND C/C++ comments (`open /* comment */ ("f", O_RDONLY)`)
            // when checking if an identifier is immediately invoked with `(`!
            let mut look = pos;
            while look < len {
                if bytes[look].is_ascii_whitespace() {
                    look += 1;
                } else if bytes[look] == b'/' && look + 1 < len && bytes[look + 1] == b'/' {
                    while look < len && bytes[look] != b'\n' {
                        look += 1;
                    }
                } else if bytes[look] == b'/' && look + 1 < len && bytes[look + 1] == b'*' {
                    look += 2;
                    while look + 1 < len && !(bytes[look] == b'*' && bytes[look + 1] == b'/') {
                        look += 1;
                    }
                    look = (look + 2).min(len);
                } else {
                    break;
                }
            }
            let next_is_lparen = look < len && bytes[look] == b'(';
            let next_is_rparen = look < len && bytes[look] == b')';
            if !is_member_access
                && !is_type_tag
                && (matches!(
                    tok,
                    "fopen"
                        | "fopen64"
                        | "freopen"
                        | "popen"
                        | "pclose"
                        | "_popen"
                        | "_wpopen"
                        | "system"
                        | "syscall"
                        | "__syscall"
                        | "dlopen"
                        | "dlmopen"
                        | "dlsym"
                        | "dlvsym"
                        | "LoadLibrary"
                        | "LoadLibraryA"
                        | "LoadLibraryW"
                        | "LoadLibraryEx"
                        | "LoadLibraryExA"
                        | "LoadLibraryExW"
                        | "execl"
                        | "execlp"
                        | "execle"
                        | "execv"
                        | "execvp"
                        | "execvpe"
                        | "execve"
                        | "fexecve"
                        | "posix_spawn"
                        | "posix_spawnp"
                        | "openat"
                        | "open_by_handle_at"
                        | "name_to_handle_at"
                        | "lstat"
                        | "lstat64"
                        | "fstatat"
                        | "statx"
                        | "statfs"
                        | "statfs64"
                        | "fstatfs"
                        | "fstatfs64"
                        | "statvfs"
                        | "statvfs64"
                        | "fstatvfs"
                        | "fstatvfs64"
                        | "getxattr"
                        | "lgetxattr"
                        | "fgetxattr"
                        | "listxattr"
                        | "llistxattr"
                        | "flistxattr"
                        | "getdents"
                        | "getdents64"
                        | "faccessat"
                        | "opendir"
                        | "readdir"
                        | "readdir64"
                        | "scandir"
                        | "scandir64"
                        | "scandirat"
                        | "glob"
                        | "glob64"
                        | "ftw"
                        | "ftw64"
                        | "nftw"
                        | "nftw64"
                        | "fts_open"
                        | "wordexp"
                        | "pathconf"
                        | "fpathconf"
                        | "confstr"
                        | "gethostname"
                        | "uname"
                        | "getenv"
                        | "secure_getenv"
                        | "environ"
                        | "_environ"
                        | "__environ"
                        | "readlink"
                        | "readlinkat"
                        | "realpath"
                        | "getcwd"
                        | "chdir"
                        | "fchdir"
                        | "get_current_dir_name"
                        | "getwd"
                        | "ifstream"
                        | "fstream"
                        | "filesystem"
                ) || ((in_define_directive
                    || next_is_lparen
                    || next_is_rparen
                    || prev_non_ws.ends_with('*')
                    || prev_non_ws.ends_with('=')
                    || prev_non_ws.ends_with('&')
                    || prev_non_ws.ends_with(',')
                    || prev_non_ws.ends_with('(')
                    || prev_non_ws.ends_with('?')
                    || prev_non_ws.ends_with(':')
                    || prev_non_ws.ends_with("return"))
                    && matches!(tok, "open" | "open64" | "stat" | "stat64" | "access" | "glob" | "ftw")))
            {
                return true;
            }
            continue;
        }
        let ch = source[pos..].chars().next().unwrap_or(' ');
        pos += ch.len_utf8();
    }
    false
}

pub(super) fn extract_go_cgo_preamble(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut current_group: Vec<String> = Vec::new();
    let mut blank_line_since_comment = false;
    let mut in_import_group = false;
    let mut saw_import_kw = false;
    let mut group_before_import_kw: Vec<String> = Vec::new();

    while i < len {
        // Track whitespace & blank lines between tokens/comments.
        if bytes[i].is_ascii_whitespace() {
            let mut newlines = 0usize;
            while i < len && bytes[i].is_ascii_whitespace() {
                if bytes[i] == b'\n' {
                    newlines += 1;
                }
                i += 1;
            }
            if newlines >= 2 {
                current_group.clear();
                blank_line_since_comment = true;
            }
            continue;
        }
        // Line comment `// ...`
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            let start = i + 2;
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            if blank_line_since_comment {
                current_group.clear();
                blank_line_since_comment = false;
            }
            current_group.push(source[start..i].to_string());
            continue;
        }
        // Block comment `/* ... */`
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            let start = i + 2;
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            // On an unterminated block comment (`/* "C" café` at EOF), advance `i` to `len` so
            // slicing `&source[start..end]` NEVER lands in the middle of a multi-byte UTF-8 char!
            let end = if i + 1 < len {
                let e = i;
                i += 2;
                e
            } else {
                i = len;
                len
            };
            if blank_line_since_comment {
                current_group.clear();
                blank_line_since_comment = false;
            }
            current_group.push(source[start..end].to_string());
            continue;
        }
        // Raw string `` `...` ``
        if bytes[i] == b'`' {
            let start = i + 1;
            i += 1;
            while i < len && bytes[i] != b'`' {
                i += 1;
            }
            let val = &source[start..i];
            if i < len {
                i += 1;
            }
            if (saw_import_kw || in_import_group) && val == "C" {
                let chosen = if saw_import_kw {
                    if !group_before_import_kw.is_empty() {
                        &group_before_import_kw
                    } else {
                        &current_group
                    }
                } else if !current_group.is_empty() {
                    &current_group
                } else {
                    &group_before_import_kw
                };
                if !chosen.is_empty() {
                    return Some(chosen.join("\n"));
                }
                return None;
            }
            saw_import_kw = false;
            current_group.clear();
            continue;
        }
        // Interpreted string `"..."`
        if bytes[i] == b'"' {
            let start = i + 1;
            i += 1;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    let ch = source[i..].chars().next().unwrap_or(' ');
                    i += ch.len_utf8();
                    continue;
                }
                let ch = source[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
            }
            let val = &source[start..i];
            if i < len {
                i += 1;
            }
            if (saw_import_kw || in_import_group) && val == "C" {
                let chosen = if saw_import_kw {
                    if !group_before_import_kw.is_empty() {
                        &group_before_import_kw
                    } else {
                        &current_group
                    }
                } else if !current_group.is_empty() {
                    &current_group
                } else {
                    &group_before_import_kw
                };
                if !chosen.is_empty() {
                    return Some(chosen.join("\n"));
                }
                return None;
            }
            saw_import_kw = false;
            current_group.clear();
            continue;
        }
        // Rune literal `'...'`
        if bytes[i] == b'\'' {
            i += 1;
            while i < len && bytes[i] != b'\'' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    let ch = source[i..].chars().next().unwrap_or(' ');
                    i += ch.len_utf8();
                    continue;
                }
                let ch = source[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
            }
            if i < len {
                i += 1;
            }
            saw_import_kw = false;
            current_group.clear();
            continue;
        }
        if bytes[i] == b'(' && saw_import_kw {
            in_import_group = true;
            saw_import_kw = false;
            current_group.clear();
            i += 1;
            continue;
        }
        if bytes[i] == b')' && in_import_group {
            in_import_group = false;
            group_before_import_kw.clear();
            current_group.clear();
            i += 1;
            continue;
        }
        // Identifiers / keywords
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &source[start..i];
            if !in_import_group && ident == "import" {
                saw_import_kw = true;
                group_before_import_kw = std::mem::take(&mut current_group);
            } else {
                // Allow optional package alias (e.g. `import _ "C"` or alias before `"C"`)
                // without dropping `group_before_import_kw` or `current_group` inside import!
                if !saw_import_kw && !in_import_group {
                    current_group.clear();
                    group_before_import_kw.clear();
                }
            }
            continue;
        }
        if bytes[i] == b';' {
            saw_import_kw = false;
        }
        if !in_import_group && !saw_import_kw {
            current_group.clear();
            group_before_import_kw.clear();
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        i += ch.len_utf8();
    }
    None
}

/// Replaces C `/* ... */` and `// ...` comments with spaces (preserving `\n`) while respecting
/// string/char literals, so `#include /* comment */ HEADER` parses cleanly AND commented examples
/// like `/* Example: #include HEADER */` never trigger false directives.
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

/// Extracts `#include`, `#include_next`, `__has_include`, and `__has_include_next` targets from
/// C/C++ or cgo preamble source after splicing `\` continuations and stripping C comments.
/// Returns `(target, is_optional_probe)` (`true` for `__has_include`, `false` for mandatory `#include`).
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
            let (is_inc_next, rest_opt) = if let Some(rest) = dir.strip_prefix("include_next") {
                (true, Some(rest))
            } else if let Some(rest) = dir.strip_prefix("include") {
                (false, Some(rest))
            } else {
                (false, None)
            };
            if let Some(after_inc) = rest_opt
                && (after_inc.starts_with('"')
                    || after_inc.starts_with('<')
                    || after_inc
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_whitespace))
            {
                let operand = after_inc.trim_start();
                if is_inc_next {
                    out.push((DYNAMIC_MACRO_INCLUDE_SENTINEL.to_string(), false));
                }
                if let Some(after_q) = operand.strip_prefix('"')
                    && let Some(end_q) = after_q.find('"')
                {
                    // Preserve exact quoted C header filename (including leading/trailing spaces).
                    let hdr = &after_q[..end_q];
                    if !hdr.is_empty() {
                        out.push((hdr.to_string(), false));
                    }
                } else if let Some(after_lt) = operand.strip_prefix('<')
                    && let Some(end_gt) = after_lt.find('>')
                {
                    let hdr = &after_lt[..end_gt];
                    if !hdr.is_empty() {
                        out.push((hdr.to_string(), false));
                    }
                } else if operand
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                {
                    out.push((DYNAMIC_MACRO_INCLUDE_SENTINEL.to_string(), false));
                }
            }
        }
        // Scan for `__has_include(...)` and `__has_include_next(...)` safely on UTF-8 boundaries.
        for (i, ch) in trimmed.char_indices() {
            if ch != '_' {
                continue;
            }
            let tail = &trimmed[i..];
            let (is_next, after_kw) = if let Some(r) = tail.strip_prefix("__has_include_next") {
                (true, Some(r))
            } else if let Some(r) = tail.strip_prefix("__has_include") {
                (false, Some(r))
            } else {
                (false, None)
            };
            if let Some(after) = after_kw
                && let Some(after_p) = after.trim_start().strip_prefix('(')
            {
                if is_next {
                    out.push((DYNAMIC_MACRO_INCLUDE_SENTINEL.to_string(), false));
                }
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
                        out.push((hdr.to_string(), true));
                    }
                } else if inner
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                {
                    out.push((DYNAMIC_MACRO_INCLUDE_SENTINEL.to_string(), false));
                }
            }
        }
    }
    out
}


