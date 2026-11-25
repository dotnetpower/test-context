use super::*;

/// Parses polyglot source files: Go (`.go`), C# (`.cs`), Java/Kotlin (`.java`, `.kt`),
/// C/C++ (`.c`, `.cpp`, `.cc`, `.h`, `.hpp`), Protobuf (`.proto`), and GraphQL (`.graphql`, `.gql`).
pub fn parse_polyglot_source(rel_path: &str, source: &str) -> ParsedPythonFile {
    let mut imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut env_vars = BTreeSet::new();

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

    if rel_path.ends_with(".go") {
        // Lexically scan Go imports (handling `import /* comment */ ( ... )`, comments, aliases,
        // escaped double quotes, and raw backtick literals), env-var calls (`Getenv`,
        // `LookupEnv`, `ExpandEnv` with constant `+` folding and dynamic-key sentinel), and
        // runtime filesystem reads (`os.ReadFile`, `os.Open`, `os.OpenFile`, `os.ReadDir`,
        // `os.DirFS`, `ioutil.ReadFile`, `ioutil.ReadDir`, including import aliases & dot-imports).
        if extract_go_imports_and_env_lexically(
            source,
            rel_path.ends_with("_test.go"),
            &mut imports,
            &mut env_vars,
            &mut data_refs,
            &mut defined_symbols,
            &mut test_symbols,
        ) {
            defined_symbols.insert("go_dynamic_env".to_string());
        }
        if go_source_has_caller_inspection(source) {
            defined_symbols.insert("go_caller_sensitive".to_string());
        }
        let is_cgo_file = source.contains("\"C\"") || source.contains("`C`");
        if is_cgo_file && go_cgo_body_has_runtime_io_or_env(source) {
            defined_symbols.insert("cgo_dynamic_io".to_string());
            defined_symbols.insert("go_dynamic_env".to_string());
        }
        // For cgo Go files, extract ONLY the C preamble comment immediately preceding `import "C"`
        // so commented examples elsewhere in Go code (`/* Example: #include HEADER */`) never
        // trigger false macro-include sentinels.
        if is_cgo_file
            && let Some(preamble) = extract_go_cgo_preamble(source)
        {
            if c_source_has_runtime_io(&preamble) {
                defined_symbols.insert("cgo_dynamic_io".to_string());
            }
            for (hdr, is_opt) in extract_c_includes_from_source(&preamble) {
                defined_symbols.insert(if is_opt {
                    format!("c_include_opt:{hdr}")
                } else {
                    format!("c_include:{hdr}")
                });
                imports.insert(ImportSpec {
                    level: 0,
                    module: hdr,
                    symbols: Vec::new(),
                });
            }
            for raw_line in preamble.lines() {
                let trimmed = raw_line.trim();
                if let Some(cgo_pos) = trimmed.find("#cgo") {
                    let after_cgo = &trimmed[cgo_pos + "#cgo".len()..];
                    // Parse directive verb before `:` with Go's whitespace rules so
                    // `#cgo pkg-config : libfoo` and `#cgo LDFLAGS : ${SRCDIR}/libfoo.a` are matched.
                    let (verb_part, flags_part) = match after_cgo.split_once(':') {
                        Some((lhs, rhs)) => (lhs.trim(), rhs.trim()),
                        None => ("", after_cgo.trim()),
                    };
                    let verb = verb_part.split_whitespace().last().unwrap_or("");
                    if verb == "pkg-config" {
                        defined_symbols.insert(format!("c_include:{DYNAMIC_MACRO_INCLUDE_SENTINEL}"));
                        continue;
                    }
                    let tokens = split_cgo_flag_tokens(flags_part);
                    if verb == "LDFLAGS" {
                        // Track workspace-local static/shared linker inputs (`${SRCDIR}/libfoo.a`)
                        // and fail closed on external `-L` / `-l` / `-Wl,...` / `-Xlinker` / `-T`
                        // / `-framework` linker options that can reference external/untracked files.
                        for tok in &tokens {
                            if tok.starts_with("-L")
                                || tok.starts_with("-l")
                                || tok.starts_with("-Wl,")
                                || tok == "-Xlinker"
                                || tok == "-framework"
                                || tok.starts_with("-T")
                                || tok.contains("${SRCDIR}") && tok.starts_with('-')
                            {
                                defined_symbols
                                    .insert(format!("c_include:{DYNAMIC_MACRO_INCLUDE_SENTINEL}"));
                            } else if !tok.starts_with('-')
                                && (tok.contains("${SRCDIR}")
                                    || tok.ends_with(".a")
                                    || tok.ends_with(".o")
                                    || tok.ends_with(".so")
                                    || tok.ends_with(".dylib")
                                    || tok.ends_with(".lib"))
                            {
                                let clean = tok
                                    .trim_start_matches("${SRCDIR}/")
                                    .trim_start_matches("${SRCDIR}")
                                    .trim_start_matches("./");
                                if !clean.is_empty() {
                                    defined_symbols.insert(format!("c_include:{clean}"));
                                    imports.insert(ImportSpec {
                                        level: 0,
                                        module: clean.to_string(),
                                        symbols: Vec::new(),
                                    });
                                }
                            }
                        }
                        continue;
                    }
                    let mut ti = 0usize;
                    while ti < tokens.len() {
                        let tok = tokens[ti].as_str();
                        // `--sysroot` / `-isysroot` overrides even standard system headers like
                        // `<stdio.h>`; fail closed immediately via the dynamic-include sentinel.
                        if tok == "--sysroot" || tok == "-isysroot" {
                            if ti + 1 < tokens.len() {
                                ti += 1;
                            }
                            defined_symbols
                                .insert(format!("c_include:{DYNAMIC_MACRO_INCLUDE_SENTINEL}"));
                            ti += 1;
                            continue;
                        }
                        if tok.starts_with("--sysroot=") || tok.starts_with("-isysroot=") {
                            defined_symbols
                                .insert(format!("c_include:{DYNAMIC_MACRO_INCLUDE_SENTINEL}"));
                            ti += 1;
                            continue;
                        }
                        let (is_forced_inc, raw_arg): (bool, Option<String>) =
                            if let Some(rest) = tok.strip_prefix("-I") {
                                if !rest.is_empty() {
                                    (false, Some(rest.to_string()))
                                } else if ti + 1 < tokens.len() {
                                    ti += 1;
                                    (false, Some(tokens[ti].clone()))
                                } else {
                                    (false, None)
                                }
                            } else if matches!(tok, "-isystem" | "-iquote" | "-idirafter")
                                && ti + 1 < tokens.len()
                            {
                                ti += 1;
                                (false, Some(tokens[ti].clone()))
                            } else if let Some(rest) = tok
                                .strip_prefix("-isystem")
                                .or_else(|| tok.strip_prefix("-iquote"))
                                .or_else(|| tok.strip_prefix("-idirafter"))
                                && !rest.is_empty()
                            {
                                (false, Some(rest.to_string()))
                            } else if matches!(tok, "-include" | "-imacros")
                                && ti + 1 < tokens.len()
                            {
                                ti += 1;
                                (true, Some(tokens[ti].clone()))
                            } else if let Some(rest) = tok
                                .strip_prefix("-include")
                                .or_else(|| tok.strip_prefix("-imacros"))
                                && !rest.is_empty()
                            {
                                (true, Some(rest.to_string()))
                            } else {
                                (false, None)
                            };
                        if let Some(arg) = raw_arg {
                            let clean = arg
                                .trim_start_matches("${SRCDIR}/")
                                .trim_start_matches("${SRCDIR}")
                                .trim_start_matches("./")
                                .trim_end_matches('/');
                            if !clean.is_empty() {
                                if is_forced_inc {
                                    defined_symbols.insert(format!("c_include:{clean}"));
                                    imports.insert(ImportSpec {
                                        level: 0,
                                        module: clean.to_string(),
                                        symbols: Vec::new(),
                                    });
                                } else {
                                    defined_symbols.insert(format!("cgo_inc:{clean}"));
                                }
                            }
                        }
                        ti += 1;
                    }
                }
            }
        }
        for raw_line in source.lines() {
            let trimmed = raw_line.trim();
            if let Some(embed) = trimmed.strip_prefix("//go:embed")
                && embed.as_bytes().first().is_some_and(u8::is_ascii_whitespace)
            {
                // Parse space/tab-separated, double-quoted (unescaped per Go string rules), or
                // backtick-quoted `//go:embed` patterns (including `all:` prefix and globs).
                let eb = embed.as_bytes();
                let mut pos = 0usize;
                while pos < eb.len() {
                    while pos < eb.len() && eb[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    if pos >= eb.len() {
                        break;
                    }
                    let decoded_pat: String = if eb[pos] == b'"' {
                        pos += 1;
                        let st = pos;
                        while pos < eb.len() && eb[pos] != b'"' {
                            if eb[pos] == b'\\' && pos + 1 < eb.len() {
                                pos += 2;
                                continue;
                            }
                            pos += 1;
                        }
                        let raw_s = &embed[st..pos];
                        if pos < eb.len() {
                            pos += 1;
                        }
                        decode_go_double_quoted(raw_s)
                    } else if eb[pos] == b'`' {
                        pos += 1;
                        let st = pos;
                        while pos < eb.len() && eb[pos] != b'`' {
                            pos += 1;
                        }
                        let raw_s = &embed[st..pos];
                        if pos < eb.len() {
                            pos += 1;
                        }
                        raw_s.to_string()
                    } else {
                        let st = pos;
                        while pos < eb.len() && !eb[pos].is_ascii_whitespace() {
                            pos += 1;
                        }
                        embed[st..pos].to_string()
                    };
                    let (has_all_prefix, stripped_all) =
                        if let Some(rest) = decoded_pat.strip_prefix("all:") {
                            (true, rest)
                        } else {
                            (false, decoded_pat.as_str())
                        };
                    let clean = stripped_all.trim_start_matches("./");
                    if !clean.is_empty() {
                        let embed_sym = if has_all_prefix {
                            format!("go_embed:all:{clean}")
                        } else {
                            format!("go_embed:{clean}")
                        };
                        defined_symbols.insert(embed_sym);
                        if !clean.contains(['*', '?', '[']) {
                            data_refs.insert(clean.to_string());
                        }
                    }
                }
            }
            let line = trimmed.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line
                .strip_prefix("func")
                .filter(|r| r.starts_with(|c: char| c.is_ascii_whitespace()) || r.starts_with("/*"))
            {
                let mut fn_part = rest.trim_start();
                while let Some(after_blk) = fn_part.strip_prefix("/*") {
                    if let Some(end_idx) = after_blk.find("*/") {
                        fn_part = after_blk[end_idx + 2..].trim_start();
                    } else {
                        break;
                    }
                }
                let fn_name = fn_part
                    .split(|c: char| c == '(' || c == '[' || c.is_ascii_whitespace())
                    .next()
                    .unwrap_or("")
                    .trim();
                if !fn_name.is_empty() && !fn_name.starts_with('(') {
                    defined_symbols.insert(fn_name.to_string());
                    if (fn_name.starts_with("Test")
                        || fn_name.starts_with("Benchmark")
                        || fn_name.starts_with("Example")
                        || fn_name.starts_with("Fuzz"))
                        && rel_path.ends_with("_test.go")
                    {
                        test_symbols.insert(fn_name.to_string());
                    }
                }
            }
        }
        return ParsedPythonFile {
            imports: imports.into_iter().collect(),
            test_symbols: test_symbols.into_iter().collect(),
            defined_symbols: defined_symbols.into_iter().collect(),
            data_refs: data_refs.into_iter().collect(),
            env_vars_referenced: env_vars.into_iter().collect(),
            ..ParsedPythonFile::default()
        };
    }

    // C/C++ (`.c`, `.cpp`, `.cc`, `.h`, `.hpp`), Fortran (`.f`, `.f90`, `.f95`, `.for`),
    // C# (`.cs`), Java/Kotlin (`.java`, `.kt`)
    if is_cgo_native_source_path(rel_path) {
        for (hdr, is_opt) in extract_c_includes_from_source(source) {
            defined_symbols.insert(if is_opt {
                format!("c_include_opt:{hdr}")
            } else {
                format!("c_include:{hdr}")
            });
            imports.insert(ImportSpec {
                level: 0,
                module: hdr,
                symbols: Vec::new(),
            });
        }
        let fortran_ext = Path::new(rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let is_fortran_file = matches!(
            fortran_ext.as_deref(),
            Some("f" | "for" | "f90" | "f95" | "f03" | "f08")
        );
        let is_fixed_form_fortran = matches!(fortran_ext.as_deref(), Some("f" | "for"));
        if is_fortran_file {
            let fortran_joined = join_fortran_continuations(source, is_fixed_form_fortran);
            scan_fortran_logical_source(&fortran_joined, &mut imports, &mut defined_symbols);
        } else {
            // Included C/Fortran header files (e.g. `config.h`, `defs.fh`) can contain continued
            // Fortran `INCLUDE`/`USE`/`OPEN` fragments as well as C runtime I/O or dynamic loading!
            let is_header_fragment = matches!(
                fortran_ext.as_deref(),
                Some("h" | "hh" | "hpp" | "hxx" | "fh" | "inc" | "finc")
            );
            if is_header_fragment {
                let free_joined = join_fortran_continuations(source, false);
                scan_fortran_logical_source(&free_joined, &mut imports, &mut defined_symbols);
                let fixed_joined = join_fortran_continuations(source, true);
                if fixed_joined != free_joined {
                    scan_fortran_logical_source(&fixed_joined, &mut imports, &mut defined_symbols);
                }
            } else if source
                .as_bytes()
                .windows(7)
                .any(|w| w.eq_ignore_ascii_case(b"include"))
            {
                for inc in extract_fortran_includes(source) {
                    defined_symbols.insert(format!("c_include:{inc}"));
                    imports.insert(ImportSpec {
                        level: 0,
                        module: inc,
                        symbols: Vec::new(),
                    });
                }
            }
            if c_source_has_runtime_io(source) {
                defined_symbols.insert("cgo_dynamic_io".to_string());
            }
        }
    }
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
        // C# `[Fact]`, `[Theory]`, `[Test]`, `[TestMethod]` and Java/Kotlin `@Test`, `@ParameterizedTest`
        if line.starts_with("[Fact")
            || line.starts_with("[Theory")
            || line.starts_with("[Test")
            || line.starts_with("@Test")
            || line.starts_with("@ParameterizedTest")
        {
            pending_test_attr = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("using ") {
            let ns = rest.trim_end_matches(';').trim();
            if !ns.is_empty() && !ns.contains('(') && !ns.starts_with("System") {
                imports.insert(ImportSpec {
                    level: 0,
                    module: ns.to_string(),
                    symbols: Vec::new(),
                });
            }
        } else if let Some(rest) = line.strip_prefix("import ") {
            let imp = rest.trim_end_matches(';').trim();
            if !imp.is_empty() && !imp.starts_with("java.") && !imp.starts_with("kotlin.") {
                imports.insert(ImportSpec {
                    level: 0,
                    module: imp.to_string(),
                    symbols: Vec::new(),
                });
            }
        }
        for kw in ["class ", "interface ", "struct ", "record "] {
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
        if pending_test_attr
            && let Some(paren_idx) = line.find('(')
        {
            let before_paren = line[..paren_idx].trim();
            if let Some(fn_name) = before_paren.split_whitespace().last() {
                let clean_name = fn_name.trim_start_matches("fun ").trim();
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
        env_vars_referenced: env_vars.into_iter().collect(),
        ..ParsedPythonFile::default()
    }
}

/// Lexically scans Go source to extract:
/// 1. All import specs — including `import /* comment */ ( ... )`, single-line `import "pkg"`,
///    aliased `import alias "pkg"`, escaped double-quoted strings, and raw backtick literals.
/// 2. All env-var lookups — `Getenv(...)`, `LookupEnv(...)`, and `ExpandEnv(...)` with any
///    package/alias qualifier, optional whitespace/comments, constant `"A" + "B"` folding, and
///    `"..."` or `` `...` `` literals.
///
/// Returns `true` if a `Getenv` / `LookupEnv` / `ExpandEnv` call receives a non-literal dynamic
/// expression so the caller can fail closed instead of caching against an untracked env var.
pub(super) fn extract_go_imports_and_env_lexically(
    source: &str,
    is_go_test_file: bool,
    imports: &mut BTreeSet<ImportSpec>,
    env_vars: &mut BTreeSet<String>,
    data_refs: &mut BTreeSet<String>,
    defined_symbols: &mut BTreeSet<String>,
    test_symbols: &mut BTreeSet<String>,
) -> bool {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut has_dynamic_env = false;

    let skip_ws_and_comments = |pos: &mut usize| {
        while *pos < len {
            if bytes[*pos].is_ascii_whitespace() {
                *pos += 1;
            } else if bytes[*pos] == b'/' && *pos + 1 < len && bytes[*pos + 1] == b'/' {
                *pos += 2;
                while *pos < len && bytes[*pos] != b'\n' {
                    *pos += 1;
                }
            } else if bytes[*pos] == b'/' && *pos + 1 < len && bytes[*pos + 1] == b'*' {
                *pos += 2;
                while *pos + 1 < len && !(bytes[*pos] == b'*' && bytes[*pos + 1] == b'/') {
                    *pos += 1;
                }
                if *pos + 1 < len {
                    *pos += 2;
                } else {
                    *pos = len;
                }
            } else {
                break;
            }
        }
    };

    let read_go_string_lit = |pos: &mut usize| -> Option<std::result::Result<String, ()>> {
        if *pos >= len {
            return None;
        }
        if bytes[*pos] == b'"' {
            *pos += 1;
            let start = *pos;
            while *pos < len && bytes[*pos] != b'"' {
                if bytes[*pos] == b'\\' && *pos + 1 < len {
                    *pos += 1;
                    let esc_ch = source[*pos..].chars().next().unwrap_or(' ');
                    *pos += esc_ch.len_utf8();
                    continue;
                }
                let ch = source[*pos..].chars().next().unwrap_or(' ');
                *pos += ch.len_utf8();
            }
            let raw = &source[start..*pos];
            if *pos < len {
                *pos += 1;
            }
            Some(decode_go_double_quoted_checked(raw))
        } else if bytes[*pos] == b'`' {
            *pos += 1;
            let start = *pos;
            while *pos < len && bytes[*pos] != b'`' {
                let ch = source[*pos..].chars().next().unwrap_or(' ');
                *pos += ch.len_utf8();
            }
            let raw = &source[start..*pos];
            if *pos < len {
                *pos += 1;
            }
            Some(Ok(raw.to_string()))
        } else {
            None
        }
    };

    let mut in_import_group = false;
    let mut expecting_single_import = false;
    let mut pending_import_alias: Option<String> = None;
    let mut os_aliases: BTreeSet<String> = BTreeSet::from(["os".to_string()]);
    let mut ioutil_aliases: BTreeSet<String> = BTreeSet::from(["ioutil".to_string()]);
    let mut filepath_aliases: BTreeSet<String> = BTreeSet::from(["filepath".to_string()]);
    let mut fs_aliases: BTreeSet<String> = BTreeSet::from(["fs".to_string()]);
    let mut syscall_aliases: BTreeSet<String> =
        BTreeSet::from(["syscall".to_string(), "unix".to_string()]);
    let mut user_aliases: BTreeSet<String> = BTreeSet::from(["user".to_string()]);
    let mut exec_aliases: BTreeSet<String> = BTreeSet::from(["exec".to_string()]);
    let mut os_dot_imported = false;
    let mut ioutil_dot_imported = false;
    let mut filepath_dot_imported = false;
    let mut fs_dot_imported = false;
    let mut syscall_dot_imported = false;
    let mut user_dot_imported = false;
    let mut exec_dot_imported = false;
    let mut prev_was_dot = false;
    let mut prev_was_func_kw = false;
    let mut has_dot_import = false;
    let mut prev_ident: Option<String> = None;
    let mut prev_dot_ident: Option<String> = None;

    while i < len {
        skip_ws_and_comments(&mut i);
        if i >= len {
            break;
        }
        let b = bytes[i];
        let after_dot = prev_was_dot;
        let after_func_kw = prev_was_func_kw;
        let cur_dot_ident = prev_dot_ident.take();
        prev_was_dot = false;
        prev_was_func_kw = false;
        // Rune literal `'x'`
        if b == b'\'' {
            prev_ident = None;
            i += 1;
            while i < len && bytes[i] != b'\'' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1;
                    let esc_ch = source[i..].chars().next().unwrap_or(' ');
                    i += esc_ch.len_utf8();
                    continue;
                }
                let ch = source[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
            }
            if i < len {
                i += 1;
            }
            continue;
        }
        // String literal `"..."` or `` `...` ``
        if b == b'"' || b == b'`' {
            prev_ident = None;
            if let Some(lit_res) = read_go_string_lit(&mut i)
                && (in_import_group || expecting_single_import)
            {
                let alias = pending_import_alias.take();
                if let Ok(lit) = lit_res
                    && !lit.is_empty()
                {
                    if lit == "os" {
                        match alias.as_deref() {
                            Some(".") => os_dot_imported = true,
                            Some("_") => {}
                            Some(a) => {
                                os_aliases.insert(a.to_string());
                            }
                            None => {
                                os_aliases.insert("os".to_string());
                            }
                        }
                    } else if lit == "io/ioutil" {
                        match alias.as_deref() {
                            Some(".") => ioutil_dot_imported = true,
                            Some("_") => {}
                            Some(a) => {
                                ioutil_aliases.insert(a.to_string());
                            }
                            None => {
                                ioutil_aliases.insert("ioutil".to_string());
                            }
                        }
                    } else if lit == "path/filepath" {
                        match alias.as_deref() {
                            Some(".") => filepath_dot_imported = true,
                            Some("_") => {}
                            Some(a) => {
                                filepath_aliases.insert(a.to_string());
                            }
                            None => {
                                filepath_aliases.insert("filepath".to_string());
                            }
                        }
                    } else if lit == "io/fs" {
                        match alias.as_deref() {
                            Some(".") => fs_dot_imported = true,
                            Some("_") => {}
                            Some(a) => {
                                fs_aliases.insert(a.to_string());
                            }
                            None => {
                                fs_aliases.insert("fs".to_string());
                            }
                        }
                    } else if matches!(
                        lit.as_str(),
                        "syscall" | "golang.org/x/sys/unix" | "golang.org/x/sys/windows"
                    ) {
                        let def_name = lit.rsplit('/').next().unwrap_or("syscall");
                        match alias.as_deref() {
                            Some(".") => syscall_dot_imported = true,
                            Some("_") => {}
                            Some(a) => {
                                syscall_aliases.insert(a.to_string());
                            }
                            None => {
                                syscall_aliases.insert(def_name.to_string());
                            }
                        }
                    } else if lit == "os/user" {
                        match alias.as_deref() {
                            Some(".") => user_dot_imported = true,
                            Some("_") => {}
                            Some(a) => {
                                user_aliases.insert(a.to_string());
                            }
                            None => {
                                user_aliases.insert("user".to_string());
                            }
                        }
                    } else if lit == "os/exec" {
                        match alias.as_deref() {
                            Some(".") => exec_dot_imported = true,
                            Some("_") => {}
                            Some(a) => {
                                exec_aliases.insert(a.to_string());
                            }
                            None => {
                                exec_aliases.insert("exec".to_string());
                            }
                        }
                    }
                    imports.insert(ImportSpec {
                        level: 0,
                        module: lit,
                        symbols: Vec::new(),
                    });
                }
                if expecting_single_import {
                    expecting_single_import = false;
                }
            }
            continue;
        }
        if in_import_group {
            prev_ident = None;
            if b == b'.' {
                has_dot_import = true;
                pending_import_alias = Some(".".to_string());
                i += 1;
                continue;
            }
            if b == b')' {
                in_import_group = false;
                pending_import_alias = None;
                i += 1;
                continue;
            }
            if b == b';' {
                pending_import_alias = None;
                i += 1;
                continue;
            }
            if let Some(ch) = source[i..].chars().next()
                && (ch == '_' || ch.is_alphabetic())
            {
                let start = i;
                while let Some(c) = source[i..].chars().next() {
                    if c == '_' || c.is_alphanumeric() {
                        i += c.len_utf8();
                    } else {
                        break;
                    }
                }
                pending_import_alias = Some(source[start..i].to_string());
                continue;
            }
            let ch = source[i..].chars().next().unwrap_or(' ');
            i += ch.len_utf8();
            continue;
        }
        if expecting_single_import {
            prev_ident = None;
            if b == b'(' {
                in_import_group = true;
                expecting_single_import = false;
                pending_import_alias = None;
                i += 1;
                continue;
            }
            // Optional package alias (`.` / `_` / Unicode identifier) before single import path
            if b == b'.' {
                has_dot_import = true;
                pending_import_alias = Some(".".to_string());
                i += 1;
                continue;
            }
            if let Some(ch) = source[i..].chars().next()
                && (ch == '_' || ch.is_alphabetic())
            {
                let start = i;
                while let Some(c) = source[i..].chars().next() {
                    if c == '_' || c.is_alphanumeric() {
                        i += c.len_utf8();
                    } else {
                        break;
                    }
                }
                pending_import_alias = Some(source[start..i].to_string());
                continue;
            }
            expecting_single_import = false;
            pending_import_alias = None;
        }
        // Identifiers (including Unicode Go identifiers like `import ф "os"; ф.ReadFile(...)`):
        if let Some(first_ch) = source[i..].chars().next()
            && (first_ch == '_' || first_ch.is_alphabetic())
        {
            let start = i;
            while let Some(c) = source[i..].chars().next() {
                if c == '_' || c.is_alphanumeric() {
                    i += c.len_utf8();
                } else {
                    break;
                }
            }
            let ident = &source[start..i];
            if ident == "func" {
                prev_was_func_kw = true;
                prev_ident = None;
                continue;
            }
            if ident == "import" {
                expecting_single_import = true;
                pending_import_alias = None;
                prev_ident = None;
                continue;
            }
            prev_ident = Some(ident.to_string());
            if after_func_kw {
                // Capture Go function declarations lexically across newlines and multiline
                // `/* ... */` comments (`func /* comment\n */ TestFail(...)`)!
                defined_symbols.insert(ident.to_string());
                if is_go_test_file
                    && (ident.starts_with("Test")
                        || ident.starts_with("Benchmark")
                        || ident.starts_with("Example")
                        || ident.starts_with("Fuzz"))
                {
                    test_symbols.insert(ident.to_string());
                }
                continue;
            }
            // Directory-capable handles (`os.Open`, `os.OpenFile`, `os.DirFS`), cwd mutations
            // (`os.Chdir`), and environment-dependent OS helpers (`os.UserHomeDir`, `UserCacheDir`,
            // `UserConfigDir`, `TempDir`, `Executable`) cannot be captured by static file hashes:
            // fail closed!
            let is_os_meta = matches!(
                ident,
                "Stat"
                    | "Lstat"
                    | "ReadDir"
                    | "Readlink"
                    | "Getwd"
                    | "Chdir"
                    | "UserHomeDir"
                    | "UserCacheDir"
                    | "UserConfigDir"
                    | "TempDir"
                    | "Executable"
                    | "Open"
                    | "OpenFile"
                    | "DirFS"
                    | "OpenRoot"
                    | "OpenInRoot"
            ) && (cur_dot_ident
                .as_deref()
                .is_some_and(|p| os_aliases.contains(p))
                || (!after_dot && os_dot_imported));
            let is_syscall_io = matches!(
                ident,
                "Open"
                    | "Openat"
                    | "OpenFile"
                    | "Stat"
                    | "Lstat"
                    | "Fstatat"
                    | "Statfs"
                    | "Fstatfs"
                    | "Statvfs"
                    | "Fstatvfs"
                    | "Statx"
                    | "Getdents"
                    | "Getdents64"
                    | "OpenByHandleAt"
                    | "Listxattr"
                    | "Getxattr"
                    | "Lgetxattr"
                    | "Access"
                    | "Faccessat"
                    | "Chdir"
                    | "Fchdir"
                    | "Getwd"
                    | "Getcwd"
                    | "Readlink"
                    | "Readlinkat"
                    | "Getenv"
                    | "Environ"
                    | "Syscall"
                    | "Syscall6"
                    | "Syscall9"
                    | "Syscall12"
                    | "Syscall15"
                    | "Syscall18"
                    | "SyscallN"
                    | "RawSyscall"
                    | "RawSyscall6"
                    | "RawSyscallN"
                    | "CreateFile"
                    | "FindFirstFile"
                    | "GetDiskFreeSpaceEx"
            ) && (cur_dot_ident
                .as_deref()
                .is_some_and(|p| syscall_aliases.contains(p))
                || (!after_dot && syscall_dot_imported));
            let is_user_env = matches!(ident, "Current" | "Lookup" | "LookupId")
                && (cur_dot_ident
                    .as_deref()
                    .is_some_and(|p| user_aliases.contains(p))
                    || (!after_dot && user_dot_imported));
            let is_exec_io = matches!(ident, "LookPath" | "Command" | "CommandContext")
                && (cur_dot_ident
                    .as_deref()
                    .is_some_and(|p| exec_aliases.contains(p))
                    || (!after_dot && exec_dot_imported));
            let is_ioutil_readdir = ident == "ReadDir"
                && (cur_dot_ident
                    .as_deref()
                    .is_some_and(|p| ioutil_aliases.contains(p))
                    || (!after_dot && ioutil_dot_imported));
            let is_filepath_fs_enum = matches!(
                ident,
                "Glob" | "Walk" | "WalkDir" | "EvalSymlinks" | "Abs"
            ) && (cur_dot_ident
                .as_deref()
                .is_some_and(|p| filepath_aliases.contains(p))
                || (!after_dot && filepath_dot_imported));
            let is_iofs_enum = matches!(
                ident,
                "Glob" | "WalkDir" | "ReadDir" | "ReadFile" | "Stat" | "Sub"
            ) && (cur_dot_ident
                .as_deref()
                .is_some_and(|p| fs_aliases.contains(p))
                || (!after_dot && fs_dot_imported));
            if is_os_meta
                || is_syscall_io
                || is_user_env
                || is_exec_io
                || is_ioutil_readdir
                || is_filepath_fs_enum
                || is_iofs_enum
            {
                // Filesystem enumeration, cwd mutation, syscall I/O, subprocess, or env-dependent
                // OS helpers observe state beyond static file hashes: fail closed.
                defined_symbols.insert("go_dynamic_io".to_string());
                continue;
            }
            let is_os_io = ident == "ReadFile"
                && (cur_dot_ident
                    .as_deref()
                    .is_some_and(|p| os_aliases.contains(p))
                    || (!after_dot && os_dot_imported));
            let is_ioutil_io = ident == "ReadFile"
                && (cur_dot_ident
                    .as_deref()
                    .is_some_and(|p| ioutil_aliases.contains(p))
                    || (!after_dot && ioutil_dot_imported));
            if is_os_io || is_ioutil_io {
                // Relative runtime reads inside non-test library `.go` files resolve against the
                // CONSUMING test package's working directory (which may be a different package!),
                // so only `_test.go` files have a statically known cwd (`parent_str`); library
                // `.go` files using `os.ReadFile` must fail closed!
                if !is_go_test_file {
                    defined_symbols.insert("go_dynamic_io".to_string());
                    continue;
                }
                let mut probe = i;
                skip_ws_and_comments(&mut probe);
                if probe < len && bytes[probe] == b'(' {
                    probe += 1;
                    skip_ws_and_comments(&mut probe);
                    let mut folded = String::new();
                    let mut parsed_literal = false;
                    let mut all_literal = true;
                    loop {
                        match read_go_string_lit(&mut probe) {
                            Some(Ok(part)) => {
                                folded.push_str(&part);
                                parsed_literal = true;
                                skip_ws_and_comments(&mut probe);
                                if probe < len && bytes[probe] == b'+' {
                                    probe += 1;
                                    skip_ws_and_comments(&mut probe);
                                    continue;
                                }
                                if probe < len && matches!(bytes[probe], b')' | b',') {
                                    break;
                                }
                                all_literal = false;
                                break;
                            }
                            Some(Err(())) | None => {
                                all_literal = false;
                                break;
                            }
                        }
                    }
                    if parsed_literal && all_literal {
                        // On POSIX, backslashes inside filenames (`config\prod.json`) are literal
                        // filename characters, NOT directory separators: fail closed before
                        // normalizing if a backslash is present on non-Windows!
                        let has_posix_bs = !cfg!(windows) && folded.contains('\\');
                        let clean = if cfg!(windows) {
                            folded
                                .trim_start_matches("./")
                                .trim_end_matches('/')
                                .replace('\\', "/")
                        } else {
                            folded
                                .trim_start_matches("./")
                                .trim_end_matches('/')
                                .to_string()
                        };
                        if !has_posix_bs
                            && !clean.is_empty()
                            && clean != "."
                            && !clean.starts_with('/')
                            && !clean.starts_with("..")
                            && !clean.contains("/..")
                            && !clean.contains(['*', '?', '[', ']', '\\', '\n', '\r', '\0'])
                        {
                            data_refs.insert(clean.clone());
                            defined_symbols.insert(format!("go_embed:all:{clean}"));
                        } else {
                            defined_symbols.insert("go_dynamic_io".to_string());
                        }
                    } else {
                        defined_symbols.insert("go_dynamic_io".to_string());
                    }
                } else {
                    // Indirect function value (e.g. `fn := os.ReadFile`): fail closed.
                    defined_symbols.insert("go_dynamic_io".to_string());
                }
                continue;
            }
            if ident == "Environ" {
                let mut probe = i;
                skip_ws_and_comments(&mut probe);
                if (probe < len && bytes[probe] == b'(') || after_dot || has_dot_import {
                    // `os.Environ()` OR function value `fn := os.Environ` (or dot-imported
                    // `Environ`) enumerates the full environment; fail closed.
                    has_dynamic_env = true;
                }
                continue;
            }
            if matches!(ident, "Getenv" | "LookupEnv" | "ExpandEnv") {
                let mut probe = i;
                skip_ws_and_comments(&mut probe);
                if probe < len && bytes[probe] == b'(' {
                    probe += 1;
                    skip_ws_and_comments(&mut probe);
                    // Parse one or more `+`-concatenated string literals (`"APP_" + "MODE"`).
                    let mut folded = String::new();
                    let mut parsed_literal = false;
                    let mut all_literal = true;
                    loop {
                        match read_go_string_lit(&mut probe) {
                            Some(Ok(part)) => {
                                folded.push_str(&part);
                                parsed_literal = true;
                                skip_ws_and_comments(&mut probe);
                                if probe < len && bytes[probe] == b'+' {
                                    probe += 1;
                                    skip_ws_and_comments(&mut probe);
                                    continue;
                                }
                                if probe < len && bytes[probe] == b')' {
                                    break;
                                }
                                all_literal = false;
                                break;
                            }
                            Some(Err(())) | None => {
                                // Non-UTF-8 escape sequence or non-literal expression: fail closed.
                                all_literal = false;
                                break;
                            }
                        }
                    }
                    if parsed_literal && all_literal {
                        let arg = folded;
                        if ident == "ExpandEnv" {
                            // Follow Go's `os.Expand` rules:
                            // - `${...}` accepts any non-empty key without `=` or NUL (e.g. `${APP-MODE}`, `${9}`)
                            // - `$X` accepts ASCII identifier chars AND single-char special shell vars (`$0`..`$9`, `$*`, `$#`, `$$`, `$@`, `$!`, `$?`, `$-`).
                            let ab = arg.as_bytes();
                            let mut k = 0usize;
                            while k < ab.len() {
                                if ab[k] == b'$' {
                                    k += 1;
                                    if k < ab.len() && ab[k] == b'{' {
                                        k += 1;
                                        let vs = k;
                                        while k < ab.len() && ab[k] != b'}' {
                                            k += 1;
                                        }
                                        let var = &arg[vs..k];
                                        if !var.is_empty()
                                            && !var.contains('=')
                                            && !var.contains('\0')
                                        {
                                            env_vars.insert(var.to_string());
                                        }
                                        if k < ab.len() {
                                            k += 1;
                                        }
                                    } else if k < ab.len()
                                        && matches!(
                                            ab[k],
                                            b'*' | b'#' | b'$' | b'@' | b'!' | b'?' | b'-'
                                                | b'0'..=b'9'
                                        )
                                    {
                                        env_vars.insert((ab[k] as char).to_string());
                                        k += 1;
                                    } else {
                                        let vs = k;
                                        while k < ab.len()
                                            && (ab[k].is_ascii_alphanumeric() || ab[k] == b'_')
                                        {
                                            k += 1;
                                        }
                                        let var = &arg[vs..k];
                                        if !var.is_empty() {
                                            env_vars.insert(var.to_string());
                                        }
                                    }
                                } else {
                                    k += 1;
                                }
                            }
                        } else if !arg.is_empty() && !arg.contains('=') && !arg.contains('\0') {
                            // Go's `os.Getenv` / `os.LookupEnv` accepts any non-empty OS env key
                            // without `=` or NUL (including hyphens/dots such as `APP-MODE`).
                            env_vars.insert(arg);
                        }
                        i = probe;
                    } else {
                        // Non-literal key expression (e.g. `os.Getenv(key)` or non-UTF-8 byte escape).
                        has_dynamic_env = true;
                    }
                } else if after_dot || has_dot_import {
                    // Indirect function value (e.g. `get := os.Getenv` or dot-imported `get := Getenv`):
                    // call arguments cannot be statically tracked, so fail closed.
                    has_dynamic_env = true;
                }
            }
            continue;
        }
        if b == b'.' {
            prev_was_dot = true;
            prev_dot_ident = prev_ident.take();
        } else {
            prev_ident = None;
        }
        let ch = source[i..].chars().next().unwrap_or(' ');
        i += ch.len_utf8();
    }
    has_dynamic_env
}


