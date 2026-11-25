use super::*;

/// Parses a Python source file to extract imports, test symbols, pytest fixtures, defined symbols, and data file references.
pub fn parse_python_source(source: &str) -> ParsedPythonFile {
    let default_patterns = ["test_*".to_string()];
    parse_python_source_with_patterns(source, &default_patterns)
}

pub fn parse_python_source_with_patterns(
    source: &str,
    test_function_patterns: &[String],
) -> ParsedPythonFile {
    let (tokens, token_indents) = tokenize_python_with_indents(source);
    let mut imports = BTreeSet::new();
    let mut type_checking_imports = BTreeSet::new();
    let mut env_vars = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut fixtures_defined = BTreeSet::new();
    let mut fixtures_used = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut markers = BTreeSet::new();
    let mut module_markers = BTreeSet::new();

    let mut has_top_level_non_import = false;
    {
        let mut stmt_start = true;
        let mut in_tc = false;
        let mut k = 0usize;
        while k < tokens.len() {
            match &tokens[k] {
                PyToken::Newline => {
                    stmt_start = true;
                    k += 1;
                }
                PyToken::StartTypeChecking => {
                    in_tc = true;
                    k += 1;
                }
                PyToken::EndTypeChecking => {
                    in_tc = false;
                    stmt_start = true;
                    k += 1;
                }
                tok if stmt_start && !in_tc => {
                    let is_pure_barrel_stmt = match tok {
                        PyToken::Ident("import" | "from" | "pass") => true,
                        PyToken::Ident("__all__")
                            if matches!(tokens.get(k + 1), Some(PyToken::Assign)) =>
                        {
                            // Only treat `__all__ = [...]` / `(...)` as a pure barrel statement
                            // when EVERY token on its RHS is a literal/punctuation (never a call
                            // such as `__all__ = register()`!).
                            let mut rhs = k + 2;
                            let mut bdepth = 0usize;
                            let mut pure_rhs = rhs < tokens.len();
                            while rhs < tokens.len() {
                                match &tokens[rhs] {
                                    PyToken::Newline if bdepth == 0 => break,
                                    PyToken::LBracket | PyToken::LParen => {
                                        bdepth += 1;
                                    }
                                    PyToken::RBracket | PyToken::RParen => {
                                        bdepth = bdepth.saturating_sub(1);
                                    }
                                    PyToken::StringLit(_) | PyToken::Comma | PyToken::Newline => {}
                                    _ => {
                                        pure_rhs = false;
                                        break;
                                    }
                                }
                                rhs += 1;
                            }
                            if pure_rhs {
                                k = rhs;
                                true
                            } else {
                                false
                            }
                        }
                        PyToken::StringLit(_)
                            if matches!(tokens.get(k + 1), None | Some(PyToken::Newline)) =>
                        {
                            true
                        }
                        _ => false,
                    };
                    if !is_pure_barrel_stmt {
                        has_top_level_non_import = true;
                        break;
                    }
                    stmt_start = false;
                    k += 1;
                }
                _ => {
                    stmt_start = false;
                    k += 1;
                }
            }
        }
    }
    if has_top_level_non_import {
        defined_symbols.insert("py_has_top_level_code".to_string());
    }
    if python_source_has_location_inspection(source) {
        defined_symbols.insert("py_location_sensitive".to_string());
    }
    // Track local aliases for `os.getenv`/`os.getenvb`, `os.environ`/`os.environb`, dynamic
    // import helpers, AND `pathlib.Path`/`open` constructors (including parenthesized RHS like
    // `read_env = (os.getenv)` and type-annotated assignments `p: Path = Path(...)`).
    let mut os_mod_names: BTreeSet<&str> = BTreeSet::from(["os"]);
    let mut importlib_mod_names: BTreeSet<&str> = BTreeSet::from(["importlib"]);
    let mut getenv_names: BTreeSet<&str> = BTreeSet::from(["getenv", "getenvb"]);
    let mut environ_names: BTreeSet<&str> = BTreeSet::from(["environ", "environb"]);
    let mut import_fn_names: BTreeSet<&str> =
        BTreeSet::from(["import_module", "importorskip", "__import__"]);
    let mut dunder_import_fn_names: BTreeSet<&str> = BTreeSet::from(["__import__"]);
    let mut path_ctor_names: BTreeSet<&str> =
        BTreeSet::from(["Path", "PurePath", "PosixPath", "WindowsPath"]);
    let mut open_fn_names: BTreeSet<&str> = BTreeSet::from(["open", "open_code"]);
    let mut get_exec_path_names: BTreeSet<&str> = BTreeSet::from(["get_exec_path"]);
    let mut subprocess_mod_names: BTreeSet<&str> = BTreeSet::from(["subprocess"]);
    let mut shutil_mod_names: BTreeSet<&str> = BTreeSet::from(["shutil"]);
    let mut asyncio_mod_names: BTreeSet<&str> = BTreeSet::from(["asyncio"]);
    let mut os_path_mod_names: BTreeSet<&str> = BTreeSet::from(["path", "posixpath", "ntpath"]);
    let mut pathlib_mod_names: BTreeSet<&str> = BTreeSet::from(["pathlib"]);
    let mut io_mod_names: BTreeSet<&str> = BTreeSet::from(["io", "builtins"]);
    let mut subprocess_fn_names: BTreeSet<&str> = BTreeSet::new();
    let mut shutil_fn_names: BTreeSet<&str> = BTreeSet::new();
    let mut dynamic_io_fn_names: BTreeSet<&str> = BTreeSet::from([
        "chdir",
        "fchdir",
        "popen",
        "system",
        "check_output",
        "check_call",
        "Popen",
        "create_subprocess_exec",
        "create_subprocess_shell",
        "execv",
        "execve",
        "execvp",
        "execvpe",
        "execl",
        "execlp",
        "execle",
        "execlpe",
        "spawnv",
        "spawnve",
        "spawnvp",
        "spawnvpe",
        "spawnl",
        "spawnle",
        "spawnlp",
        "spawnlpe",
        "copytree",
        "copyfile",
        "copyfileobj",
        "copy2",
        "copystat",
        "copymode",
        "rmtree",
        "disk_usage",
        "get_terminal_size",
        "chown",
        "make_archive",
        "unpack_archive",
        "exists",
        "isfile",
        "isdir",
        "islink",
        "ismount",
        "lexists",
        "stat",
        "lstat",
        "listdir",
        "scandir",
        "realpath",
        "readlink",
        "getsize",
        "getmtime",
        "getctime",
        "getatime",
        "samefile",
        "abspath",
        "expanduser",
        "expandvars",
        "access",
        "getcwd",
        "getcwdb",
        "walk",
    ]);
    // Precompute bracket/paren nesting depth, statement-start import flags, true indentation-based
    // function-local scope (tracking BOTH `def` and `class` frames so class attributes inside a
    // factory function `def f(): class C: ENV = os.environ` are NOT treated as function-local, and
    // tracking `global` bindings), so keyword arguments, tuple commas, `yield from`, compound RHS
    // expressions, class attributes, and module-level/global aliases are never misclassified!
    let mut py_nesting_depth: Vec<usize> = vec![0; tokens.len()];
    let mut py_in_import_stmt: Vec<bool> = vec![false; tokens.len()];
    let mut py_is_def_local: Vec<bool> = vec![false; tokens.len()];
    let mut py_global_names: BTreeSet<&str> = BTreeSet::new();
    {
        let mut depth = 0usize;
        let mut in_imp = false;
        let mut at_stmt_start = true;
        // Each entry is `(indent, is_def)` (`true` = `def`, `false` = `class`).
        let mut scope_indent_stack: Vec<(usize, bool)> = Vec::new();
        for (ti, tok) in tokens.iter().enumerate() {
            py_nesting_depth[ti] = depth;
            let ind = token_indents.get(ti).copied().unwrap_or(0);
            if at_stmt_start
                && depth == 0
                && !matches!(
                    tok,
                    PyToken::Newline | PyToken::StartTypeChecking | PyToken::EndTypeChecking
                )
            {
                while scope_indent_stack.last().is_some_and(|&(d, _)| ind <= d) {
                    scope_indent_stack.pop();
                }
            }
            match tok {
                PyToken::Newline if depth == 0 => {
                    in_imp = false;
                    at_stmt_start = true;
                }
                PyToken::StartTypeChecking | PyToken::EndTypeChecking => {
                    at_stmt_start = true;
                }
                PyToken::Ident("import" | "from") if at_stmt_start && depth == 0 => {
                    in_imp = true;
                    at_stmt_start = false;
                }
                PyToken::Ident("async") if at_stmt_start && depth == 0 => {}
                PyToken::Ident("def") if at_stmt_start && depth == 0 => {
                    scope_indent_stack.push((ind, true));
                    at_stmt_start = false;
                }
                PyToken::Ident("class") if at_stmt_start && depth == 0 => {
                    scope_indent_stack.push((ind, false));
                    at_stmt_start = false;
                }
                PyToken::Ident("global") if at_stmt_start && depth == 0 => {
                    let mut g = ti + 1;
                    while g < tokens.len() && !matches!(tokens[g], PyToken::Newline) {
                        if let PyToken::Ident(gn) = tokens[g] {
                            py_global_names.insert(gn);
                        }
                        g += 1;
                    }
                    at_stmt_start = false;
                }
                PyToken::LParen | PyToken::LBracket => {
                    depth += 1;
                    at_stmt_start = false;
                }
                PyToken::RParen | PyToken::RBracket => {
                    depth = depth.saturating_sub(1);
                    at_stmt_start = false;
                }
                _ => {
                    at_stmt_start = false;
                }
            }
            py_in_import_stmt[ti] = in_imp;
            py_is_def_local[ti] = scope_indent_stack
                .last()
                .is_some_and(|&(d, is_def)| is_def && ind > d);
        }
    }
    // Finds `(first_non_lparen_rhs_idx, open_paren_count)` ONLY for true statement-level
    // assignments (`ident = ...` or annotated `ident: T = ...` at nesting depth 0, preceded by
    // start-of-file or `Newline`), NEVER for call keyword args (`subprocess.run(..., env=os.environ)`).
    let find_py_assign_rhs = |ident_idx: usize| -> Option<(usize, usize)> {
        if py_nesting_depth[ident_idx] != 0 {
            return None;
        }
        if ident_idx > 0
            && !matches!(
                tokens.get(ident_idx - 1),
                Some(PyToken::Newline | PyToken::StartTypeChecking | PyToken::EndTypeChecking)
            )
        {
            return None;
        }
        let assign_idx = if matches!(tokens.get(ident_idx + 1), Some(PyToken::Assign)) {
            ident_idx + 1
        } else if matches!(tokens.get(ident_idx + 1), Some(PyToken::Colon)) {
            let mut k = ident_idx + 2;
            let limit = (ident_idx + 24).min(tokens.len());
            let mut found = None;
            while k < limit && !matches!(tokens[k], PyToken::Newline) {
                if matches!(tokens[k], PyToken::Assign) {
                    found = Some(k);
                    break;
                }
                k += 1;
            }
            found?
        } else {
            return None;
        };
        let mut rhs = assign_idx + 1;
        let mut opens = 0usize;
        while matches!(tokens.get(rhs), Some(PyToken::LParen)) {
            rhs += 1;
            opens += 1;
        }
        Some((rhs, opens))
    };
    // Verifies that the RHS expression ends immediately after `end_idx` with exactly `opens`
    // matching `)` tokens followed by `Newline` or EOF (rejecting compound expressions like
    // `HAS_ENV = os.environ != {}`).
    let rhs_is_standalone_expr = |end_idx: usize, opens: usize| -> bool {
        let mut k = end_idx;
        let mut rem = opens;
        while rem > 0 {
            if !matches!(tokens.get(k), Some(PyToken::RParen)) {
                return false;
            }
            k += 1;
            rem -= 1;
        }
        matches!(
            tokens.get(k),
            None | Some(PyToken::Newline | PyToken::EndTypeChecking)
        )
    };
    let mut environ_alias_rhs_indices: BTreeSet<usize> = BTreeSet::new();
    let mut environ_alias_lhs_indices: BTreeSet<usize> = BTreeSet::new();
    let mut getenv_alias_rhs_indices: BTreeSet<usize> = BTreeSet::new();
    let mut getenv_alias_lhs_indices: BTreeSet<usize> = BTreeSet::new();
    for _pass in 0..8 {
        let mut changed = false;
        for (idx, tok) in tokens.iter().enumerate() {
            if let PyToken::Ident(ident) = *tok {
                if py_in_import_stmt[idx]
                    && matches!(
                        ident,
                        "os"
                            | "importlib"
                            | "subprocess"
                            | "shutil"
                            | "asyncio"
                            | "copy"
                            | "move"
                            | "which"
                            | "copytree"
                            | "copyfile"
                            | "copyfileobj"
                            | "copy2"
                            | "copystat"
                            | "copymode"
                            | "rmtree"
                            | "disk_usage"
                            | "get_terminal_size"
                            | "chown"
                            | "make_archive"
                            | "unpack_archive"
                            | "getenv"
                            | "getenvb"
                            | "get_exec_path"
                            | "environ"
                            | "environb"
                            | "import_module"
                            | "importorskip"
                            | "__import__"
                            | "Path"
                            | "PurePath"
                            | "PosixPath"
                            | "WindowsPath"
                            | "open"
                            | "open_code"
                            | "chdir"
                            | "fchdir"
                            | "popen"
                            | "system"
                            | "run"
                            | "call"
                            | "check_call"
                            | "check_output"
                            | "Popen"
                            | "create_subprocess_exec"
                            | "create_subprocess_shell"
                            | "getoutput"
                            | "getstatusoutput"
                            | "execv"
                            | "execve"
                            | "execvp"
                            | "execvpe"
                            | "execl"
                            | "execlp"
                            | "execle"
                            | "execlpe"
                            | "spawnv"
                            | "spawnve"
                            | "spawnvp"
                            | "spawnvpe"
                            | "spawnl"
                            | "spawnle"
                            | "spawnlp"
                            | "spawnlpe"
                            | "path"
                            | "posixpath"
                            | "ntpath"
                            | "pathlib"
                            | "io"
                            | "builtins"
                            | "exists"
                            | "isfile"
                            | "isdir"
                            | "islink"
                            | "ismount"
                            | "lexists"
                            | "stat"
                            | "lstat"
                            | "listdir"
                            | "scandir"
                            | "realpath"
                            | "readlink"
                            | "getsize"
                            | "getmtime"
                            | "getctime"
                            | "getatime"
                            | "samefile"
                            | "abspath"
                            | "expanduser"
                            | "expandvars"
                            | "access"
                            | "getcwd"
                            | "getcwdb"
                            | "walk"
                    )
                    && matches!(tokens.get(idx + 1), Some(PyToken::Ident("as")))
                    && let Some(PyToken::Ident(alias)) = tokens.get(idx + 2)
                {
                    if ident == "os" {
                        changed |= os_mod_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_env".to_string());
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(ident, "path" | "posixpath" | "ntpath") {
                        changed |= os_path_mod_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if ident == "pathlib" {
                        changed |= pathlib_mod_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(ident, "io" | "builtins") {
                        changed |= io_mod_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if ident == "importlib" {
                        changed |= importlib_mod_names.insert(*alias);
                    } else if ident == "subprocess" {
                        changed |= subprocess_mod_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if ident == "shutil" {
                        changed |= shutil_mod_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(
                        ident,
                        "copy"
                            | "copy2"
                            | "copyfile"
                            | "copyfileobj"
                            | "copytree"
                            | "copystat"
                            | "copymode"
                            | "rmtree"
                            | "move"
                            | "disk_usage"
                            | "get_terminal_size"
                            | "chown"
                            | "which"
                            | "make_archive"
                            | "unpack_archive"
                    ) {
                        let mut k = idx;
                        let mut from_shutil = false;
                        while k > 0 && py_in_import_stmt[k - 1] {
                            k -= 1;
                            if matches!(tokens.get(k), Some(PyToken::Ident("from")))
                                && matches!(tokens.get(k + 1), Some(PyToken::Ident("shutil")))
                            {
                                from_shutil = true;
                                break;
                            }
                        }
                        if from_shutil {
                            changed |= shutil_fn_names.insert(*alias);
                            if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                                defined_symbols.insert("py_dynamic_io".to_string());
                            }
                        } else if dynamic_io_fn_names.contains(ident) {
                            changed |= dynamic_io_fn_names.insert(*alias);
                            if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                                defined_symbols.insert("py_dynamic_io".to_string());
                            }
                        }
                    } else if ident == "asyncio" {
                        changed |= asyncio_mod_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(ident, "getenv" | "getenvb") {
                        changed |= getenv_names.insert(*alias);
                        // Module-level OR `global`-declared `from os import getenv as G` exports
                        // a renamed env reader that importing tests can call under `G` -> fail closed!
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_env".to_string());
                        }
                    } else if ident == "get_exec_path" {
                        changed |= get_exec_path_names.insert(*alias);
                        env_vars.insert("PATH".to_string());
                    } else if matches!(ident, "environ" | "environb") {
                        changed |= environ_names.insert(*alias);
                        // Module-level OR `global`-declared `from os import environ as ENV` exports
                        // a renamed env map that importing tests can read via `ENV["KEY"]` -> fail closed!
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_env".to_string());
                        }
                    } else if matches!(ident, "Path" | "PurePath" | "PosixPath" | "WindowsPath") {
                        changed |= path_ctor_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(ident, "open" | "open_code") {
                        changed |= open_fn_names.insert(*alias);
                        // Module-level OR `global`-declared `from builtins import open as load` or
                        // `from io import open_code as load` exports a renamed file reader -> fail closed!
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(ident, "run" | "call" | "getoutput" | "getstatusoutput") {
                        // Track `from subprocess import run as r` only when the import statement is from subprocess.
                        let mut k = idx;
                        let mut from_subprocess = false;
                        while k > 0 && py_in_import_stmt[k - 1] {
                            k -= 1;
                            if matches!(tokens.get(k), Some(PyToken::Ident("from")))
                                && matches!(tokens.get(k + 1), Some(PyToken::Ident("subprocess")))
                            {
                                from_subprocess = true;
                                break;
                            }
                        }
                        if from_subprocess {
                            changed |= subprocess_fn_names.insert(*alias);
                            if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                                defined_symbols.insert("py_dynamic_io".to_string());
                            }
                        }
                    } else if dynamic_io_fn_names.contains(ident) {
                        changed |= dynamic_io_fn_names.insert(*alias);
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else {
                        changed |= import_fn_names.insert(*alias);
                        if ident == "__import__" {
                            changed |= dunder_import_fn_names.insert(*alias);
                        }
                        if !py_is_def_local[idx] || py_global_names.contains(*alias) {
                            defined_symbols.insert("py_dynamic_import".to_string());
                        }
                    }
                } else if py_in_import_stmt[idx]
                    && matches!(
                        ident,
                        "copy"
                            | "copy2"
                            | "copyfile"
                            | "copyfileobj"
                            | "copytree"
                            | "copystat"
                            | "copymode"
                            | "rmtree"
                            | "move"
                            | "disk_usage"
                            | "get_terminal_size"
                            | "chown"
                            | "which"
                            | "make_archive"
                            | "unpack_archive"
                    )
                    && !matches!(tokens.get(idx + 1), Some(PyToken::Ident("as")))
                {
                    let mut k = idx;
                    let mut from_shutil = false;
                    while k > 0 && py_in_import_stmt[k - 1] {
                        k -= 1;
                        if matches!(tokens.get(k), Some(PyToken::Ident("from")))
                            && matches!(tokens.get(k + 1), Some(PyToken::Ident("shutil")))
                        {
                            from_shutil = true;
                            break;
                        }
                    }
                    if from_shutil {
                        changed |= shutil_fn_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if dynamic_io_fn_names.contains(ident)
                        && (!py_is_def_local[idx] || py_global_names.contains(ident))
                    {
                        defined_symbols.insert("py_dynamic_io".to_string());
                    }
                } else if py_in_import_stmt[idx]
                    && matches!(ident, "run" | "call" | "getoutput" | "getstatusoutput")
                    && !matches!(tokens.get(idx + 1), Some(PyToken::Ident("as")))
                {
                    let mut k = idx;
                    let mut from_subprocess = false;
                    while k > 0 && py_in_import_stmt[k - 1] {
                        k -= 1;
                        if matches!(tokens.get(k), Some(PyToken::Ident("from")))
                            && matches!(tokens.get(k + 1), Some(PyToken::Ident("subprocess")))
                        {
                            from_subprocess = true;
                            break;
                        }
                    }
                    if from_subprocess {
                        changed |= subprocess_fn_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    }
                } else if py_in_import_stmt[idx]
                    && dynamic_io_fn_names.contains(ident)
                    && (!py_is_def_local[idx] || py_global_names.contains(ident))
                    && !matches!(tokens.get(idx + 1), Some(PyToken::Ident("as")))
                {
                    // Module-level or `global` `from os import chdir` / `create_subprocess_exec`
                    // re-exports a cwd/subprocess helper to importing tests -> fail closed!
                    defined_symbols.insert("py_dynamic_io".to_string());
                } else if let Some((rhs, opens)) = find_py_assign_rhs(idx) {
                    if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if subprocess_mod_names.contains(m)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= subprocess_mod_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if shutil_mod_names.contains(m)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= shutil_mod_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if (matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if shutil_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(
                            tokens.get(rhs + 2),
                            Some(PyToken::Ident(
                                "copy"
                                    | "copy2"
                                    | "copyfile"
                                    | "copyfileobj"
                                    | "copytree"
                                    | "copystat"
                                    | "copymode"
                                    | "rmtree"
                                    | "move"
                                    | "disk_usage"
                                    | "get_terminal_size"
                                    | "chown"
                                    | "which"
                                    | "make_archive"
                                    | "unpack_archive"
                            ))
                        )
                        && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(r)) if shutil_fn_names.contains(r)
                        ) && rhs_is_standalone_expr(rhs + 1, opens))
                    {
                        changed |= shutil_fn_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if asyncio_mod_names.contains(m)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= asyncio_mod_names.insert(ident);
                    } else if (matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if subprocess_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(
                            tokens.get(rhs + 2),
                            Some(PyToken::Ident(
                                "run" | "call" | "check_call" | "check_output" | "Popen" | "getoutput" | "getstatusoutput"
                            ))
                        )
                        && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(r)) if subprocess_fn_names.contains(r)
                        ) && rhs_is_standalone_expr(rhs + 1, opens))
                    {
                        changed |= subprocess_fn_names.insert(ident);
                        // Module-level/exported subprocess function alias (e.g. `run_cmd = subprocess.run`
                        // in `tools.py`) can be imported and invoked by tests -> fail closed!
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if (matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(tokens.get(rhs + 2), Some(PyToken::Ident("path")))
                        && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(m)) if os_path_mod_names.contains(m)
                        ) && rhs_is_standalone_expr(rhs + 1, opens))
                    {
                        changed |= os_path_mod_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if (matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m))
                            if os_mod_names.contains(m)
                                || asyncio_mod_names.contains(m)
                                || os_path_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(
                            tokens.get(rhs + 2),
                            Some(PyToken::Ident(f)) if dynamic_io_fn_names.contains(f)
                        )
                        && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                        ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                            && matches!(tokens.get(rhs + 2), Some(PyToken::Ident("path")))
                            && matches!(tokens.get(rhs + 3), Some(PyToken::Dot))
                            && matches!(
                                tokens.get(rhs + 4),
                                Some(PyToken::Ident(f)) if dynamic_io_fn_names.contains(f)
                            )
                            && rhs_is_standalone_expr(rhs + 5, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(f)) if dynamic_io_fn_names.contains(f)
                        ) && rhs_is_standalone_expr(rhs + 1, opens))
                    {
                        changed |= dynamic_io_fn_names.insert(ident);
                        // Module-level/exported cwd or subprocess alias (e.g. `spawn = asyncio.create_subprocess_exec`,
                        // `cd = os.chdir` in `tools.py`) can be imported by tests -> fail closed!
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if pathlib_mod_names.contains(m)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= pathlib_mod_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if io_mod_names.contains(m)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= io_mod_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if (matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if pathlib_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(
                            tokens.get(rhs + 2),
                            Some(PyToken::Ident(r)) if path_ctor_names.contains(r)
                        )
                        && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(r)) if path_ctor_names.contains(r)
                        ) && rhs_is_standalone_expr(rhs + 1, opens))
                    {
                        changed |= path_ctor_names.insert(ident);
                        // Module-level/global `P = pathlib.Path` or `P = Path` exports a renamed
                        // Path constructor to importing tests -> fail closed!
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if (matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if io_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(tokens.get(rhs + 2), Some(PyToken::Ident("open" | "open_code")))
                        && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(r)) if open_fn_names.contains(r)
                        ) && rhs_is_standalone_expr(rhs + 1, opens))
                    {
                        changed |= open_fn_names.insert(ident);
                        // Module-level/global `load = open` or `load = io.open_code` exports a renamed
                        // file reader to importing tests -> fail closed!
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= os_mod_names.insert(ident);
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_env".to_string());
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                    {
                        if matches!(tokens.get(rhs + 2), Some(PyToken::Ident("getenv" | "getenvb")))
                            && rhs_is_standalone_expr(rhs + 3, opens)
                        {
                            changed |= getenv_names.insert(ident);
                            if py_is_def_local[idx] && !py_global_names.contains(ident) {
                                getenv_alias_lhs_indices.insert(idx);
                                getenv_alias_rhs_indices.insert(rhs + 2);
                            }
                        } else if matches!(
                            tokens.get(rhs + 2),
                            Some(PyToken::Ident("get_exec_path"))
                        ) && rhs_is_standalone_expr(rhs + 3, opens)
                        {
                            changed |= get_exec_path_names.insert(ident);
                            env_vars.insert("PATH".to_string());
                        } else if matches!(
                            tokens.get(rhs + 2),
                            Some(PyToken::Ident("environ" | "environb"))
                        ) && rhs_is_standalone_expr(rhs + 3, opens)
                        {
                            changed |= environ_names.insert(ident);
                            // Exempt pure `env = os.environ` ONLY inside function-local scope
                            // (`py_is_def_local`) so module-level exported `ENV = os.environ` in
                            // `settings.py` still fails closed when another module imports `ENV`!
                            if py_is_def_local[idx] && !py_global_names.contains(ident) {
                                environ_alias_lhs_indices.insert(idx);
                                environ_alias_rhs_indices.insert(rhs + 2);
                            }
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(r)) if getenv_names.contains(r)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= getenv_names.insert(ident);
                        if py_is_def_local[idx] && !py_global_names.contains(ident) {
                            getenv_alias_lhs_indices.insert(idx);
                            getenv_alias_rhs_indices.insert(rhs);
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(r)) if environ_names.contains(r)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= environ_names.insert(ident);
                        if py_is_def_local[idx] && !py_global_names.contains(ident) {
                            environ_alias_lhs_indices.insert(idx);
                            environ_alias_rhs_indices.insert(rhs);
                        }
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if importlib_mod_names.contains(m)
                    ) && rhs_is_standalone_expr(rhs + 1, opens)
                    {
                        changed |= importlib_mod_names.insert(ident);
                    } else if let Some((mod_str, call_end)) = (if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if importlib_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(tokens.get(rhs + 2), Some(PyToken::Ident("import_module")))
                        && matches!(tokens.get(rhs + 3), Some(PyToken::LParen))
                        && let Some(PyToken::StringLit(m)) = tokens.get(rhs + 4)
                        && matches!(tokens.get(rhs + 5), Some(PyToken::RParen | PyToken::Comma))
                    {
                        let mut k = rhs + 5;
                        let mut p = 1usize;
                        while k < tokens.len() && p > 0 {
                            match tokens[k] {
                                PyToken::LParen => p += 1,
                                PyToken::RParen => p -= 1,
                                _ => {}
                            }
                            k += 1;
                        }
                        Some((m.as_str(), k))
                    } else if matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(r)) if import_fn_names.contains(r)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::LParen))
                        && let Some(PyToken::StringLit(m)) = tokens.get(rhs + 2)
                        && matches!(tokens.get(rhs + 3), Some(PyToken::RParen | PyToken::Comma))
                    {
                        let mut k = rhs + 3;
                        let mut p = 1usize;
                        while k < tokens.len() && p > 0 {
                            match tokens[k] {
                                PyToken::LParen => p += 1,
                                PyToken::RParen => p -= 1,
                                _ => {}
                            }
                            k += 1;
                        }
                        Some((m.as_str(), k))
                    } else {
                        None
                    }) && matches!(
                        mod_str,
                        "os"
                            | "pathlib"
                            | "io"
                            | "builtins"
                            | "subprocess"
                            | "shutil"
                            | "asyncio"
                            | "os.path"
                            | "posixpath"
                            | "ntpath"
                    ) {
                        if rhs_is_standalone_expr(call_end, opens) {
                            match mod_str {
                                "os" => {
                                    changed |= os_mod_names.insert(ident);
                                    if !py_is_def_local[idx] || py_global_names.contains(ident) {
                                        defined_symbols.insert("py_dynamic_env".to_string());
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                "pathlib" => {
                                    changed |= pathlib_mod_names.insert(ident);
                                    if !py_is_def_local[idx] || py_global_names.contains(ident) {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                "io" | "builtins" => {
                                    changed |= io_mod_names.insert(ident);
                                    if !py_is_def_local[idx] || py_global_names.contains(ident) {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                "subprocess" => {
                                    changed |= subprocess_mod_names.insert(ident);
                                    if !py_is_def_local[idx] || py_global_names.contains(ident) {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                "shutil" => {
                                    changed |= shutil_mod_names.insert(ident);
                                    if !py_is_def_local[idx] || py_global_names.contains(ident) {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                "asyncio" => {
                                    changed |= asyncio_mod_names.insert(ident);
                                    if !py_is_def_local[idx] || py_global_names.contains(ident) {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                "os.path" | "posixpath" | "ntpath" => {
                                    changed |= os_path_mod_names.insert(ident);
                                    if !py_is_def_local[idx] || py_global_names.contains(ident) {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            // Member access or chained expression on dynamic import
                            // (e.g. `load = __import__('builtins').open`, `run = __import__('subprocess').run`,
                            // `env = __import__('os').environ`): fail closed so aliased I/O/env
                            // functions never escape tracking!
                            defined_symbols.insert("py_dynamic_io".to_string());
                            if mod_str == "os" {
                                defined_symbols.insert("py_dynamic_env".to_string());
                            }
                        }
                    } else if (matches!(
                        tokens.get(rhs),
                        Some(PyToken::Ident(m)) if importlib_mod_names.contains(m)
                    ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                        && matches!(tokens.get(rhs + 2), Some(PyToken::Ident("import_module")))
                        && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(m)) if io_mod_names.contains(m)
                        ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                            && matches!(tokens.get(rhs + 2), Some(PyToken::Ident("__import__")))
                            && rhs_is_standalone_expr(rhs + 3, opens))
                        || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(r)) if import_fn_names.contains(r)
                        ) && rhs_is_standalone_expr(rhs + 1, opens))
                    {
                        changed |= import_fn_names.insert(ident);
                        if matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(r)) if dunder_import_fn_names.contains(r)
                        ) || (matches!(
                            tokens.get(rhs),
                            Some(PyToken::Ident(m)) if io_mod_names.contains(m)
                        ) && matches!(tokens.get(rhs + 2), Some(PyToken::Ident("__import__"))))
                        {
                            changed |= dunder_import_fn_names.insert(ident);
                        }
                        if !py_is_def_local[idx] || py_global_names.contains(ident) {
                            defined_symbols.insert("py_dynamic_import".to_string());
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Track variables assigned from `Path(...)` calls (including type-annotated `p: Path = Path(...)`)
    // and transitively propagate aliases (`q = p`) so later transformations or joins fail closed!
    let mut path_var_names: BTreeSet<&str> = BTreeSet::new();
    for _pass in 0..8 {
        let mut changed = false;
        for idx in 0..tokens.len() {
            if let PyToken::Ident(var_name) = tokens[idx]
                && let Some((mut rhs, _)) = find_py_assign_rhs(idx)
            {
                if matches!(
                    tokens.get(rhs),
                    Some(PyToken::Ident(m)) if pathlib_mod_names.contains(m)
                ) && matches!(tokens.get(rhs + 1), Some(PyToken::Dot))
                {
                    rhs += 2;
                }
                if let Some(PyToken::Ident(ctor)) = tokens.get(rhs)
                    && path_ctor_names.contains(ctor)
                    && matches!(tokens.get(rhs + 1), Some(PyToken::LParen))
                {
                    changed |= path_var_names.insert(var_name);
                } else if let Some(PyToken::Ident(src_var)) = tokens.get(rhs)
                    && path_var_names.contains(src_var)
                    && !matches!(tokens.get(rhs + 1), Some(PyToken::LParen))
                {
                    changed |= path_var_names.insert(var_name);
                }
            }
        }
        if !changed {
            break;
        }
    }
    let strip_py_named_escapes = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(pos) = rest.find("\\N{") {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + 3..];
            if let Some(close) = after.find('}') {
                out.push('x');
                rest = &after[close + 1..];
            } else {
                out.push_str(&rest[pos..]);
                return out;
            }
        }
        out.push_str(rest);
        out
    };
    // Concatenates implicitly adjacent Python string literals (`"payload" ".bin"`, including across
    // newlines when inside parentheses), returning `(combined, next_idx)`.
    let collect_py_str_lit = |start: usize, allow_newline: bool| -> Option<(String, usize)> {
        let PyToken::StringLit(first) = tokens.get(start)? else {
            return None;
        };
        let mut combined = first.clone();
        let mut cur = start + 1;
        while cur < tokens.len() {
            if let PyToken::StringLit(next_s) = &tokens[cur] {
                combined.push_str(next_s);
                cur += 1;
                continue;
            }
            if allow_newline
                && matches!(tokens[cur], PyToken::Newline)
                && matches!(tokens.get(cur + 1), Some(PyToken::StringLit(_)))
            {
                cur += 1;
                continue;
            }
            break;
        }
        Some((combined, cur))
    };
    let clean_py_static_path_seg = |seg_lit: &str| -> Option<String> {
        let has_edge_ws = seg_lit != seg_lit.trim();
        let has_posix_backslash = cfg!(unix) && seg_lit.contains('\\');
        let has_named_esc = seg_lit.contains("\\N{");
        let has_interp = seg_lit.contains('{') || seg_lit.contains('}');
        let seg_clean = seg_lit
            .trim_start_matches("./")
            .trim_end_matches('/')
            .replace('\\', "/");
        if has_edge_ws
            || has_posix_backslash
            || has_named_esc
            || has_interp
            || seg_clean.is_empty()
            || seg_clean == "."
            || seg_clean.starts_with('/')
            || seg_clean.contains(['\n', '\r', '\0'])
        {
            None
        } else {
            Some(seg_clean)
        }
    };
    for (idx, tok) in tokens.iter().enumerate() {
        if let PyToken::StringLit(lit) = tok {
            if lit.contains("\\N{") {
                let stripped = strip_py_named_escapes(lit);
                if normalize_data_ref(stripped.trim()).is_some() {
                    // Named `\N{...}` Unicode escape in a fixture path cannot be resolved statically:
                    // fail closed instead of dropping the dependency.
                    defined_symbols.insert("py_dynamic_io".to_string());
                }
            } else if let Some(clean_ref) = normalize_data_ref(lit) {
                data_refs.insert(clean_ref);
            } else if lit != lit.trim() && normalize_data_ref(lit.trim()).is_some() {
                // Leading/trailing whitespace in a fixture path cannot be safely normalized
                // without tracking the wrong file: fail closed!
                defined_symbols.insert("py_dynamic_io".to_string());
            }
        }
        if let PyToken::Ident(ident) = *tok {
            let is_def = idx > 0
                && matches!(
                    tokens.get(idx - 1),
                    Some(PyToken::Ident("def" | "class"))
                );
            // Fail closed on stored `Path` variable transformations, metadata queries, or joins
            // (`p.with_suffix(...)`, `p.is_dir()`, `p.exists()`, `p.stat()`, `p.parent`, `p / "sub"`,
            // `p.joinpath(...)`), static `Path.cwd()` / `Path.home()`, AND uniquely `pathlib.Path`
            // transform/metadata methods anywhere in the file.
            if !is_def
                && idx > 0
                && matches!(tokens.get(idx - 1), Some(PyToken::Dot))
                && matches!(
                    ident,
                    "with_suffix"
                        | "with_name"
                        | "with_stem"
                        | "expanduser"
                        | "rglob"
                        | "iterdir"
                        | "relative_to"
                        | "is_dir"
                        | "is_file"
                        | "is_symlink"
                        | "is_mount"
                        | "is_junction"
                        | "is_socket"
                        | "is_fifo"
                        | "is_block_device"
                        | "is_char_device"
                        | "lstat"
                        | "samefile"
                        | "readlink"
                )
            {
                defined_symbols.insert("py_dynamic_io".to_string());
            }
            if !is_def
                && path_ctor_names.contains(ident)
                && matches!(tokens.get(idx + 1), Some(PyToken::Dot))
                && matches!(tokens.get(idx + 2), Some(PyToken::Ident("cwd" | "home")))
            {
                defined_symbols.insert("py_dynamic_io".to_string());
            }
            if !is_def
                && path_var_names.contains(ident)
                && !matches!(tokens.get(idx + 1), Some(PyToken::Assign))
                && (matches!(tokens.get(idx + 1), Some(PyToken::Slash))
                    || (matches!(tokens.get(idx + 1), Some(PyToken::Dot))
                        && matches!(
                            tokens.get(idx + 2),
                            Some(PyToken::Ident(
                                "joinpath"
                                    | "with_suffix"
                                    | "with_name"
                                    | "with_stem"
                                    | "parent"
                                    | "parents"
                                    | "resolve"
                                    | "expanduser"
                                    | "absolute"
                                    | "glob"
                                    | "rglob"
                                    | "iterdir"
                                    | "relative_to"
                                    | "exists"
                                    | "is_dir"
                                    | "is_file"
                                    | "is_symlink"
                                    | "is_mount"
                                    | "is_junction"
                                    | "is_socket"
                                    | "is_fifo"
                                    | "is_block_device"
                                    | "is_char_device"
                                    | "stat"
                                    | "lstat"
                                    | "samefile"
                                    | "owner"
                                    | "group"
                                    | "readlink"
                                    | "walk"
                            ))
                        )))
            {
                defined_symbols.insert("py_dynamic_io".to_string());
            }
            // Fail closed on escaped/higher-order Python `shutil` function references
            // (e.g. `map(shutil.which, tools)`, `funcs = [shutil.copy]`) even without direct `(`.
            if !is_def && !py_in_import_stmt[idx] {
                let is_shutil_ref = shutil_fn_names.contains(ident)
                    || (matches!(
                        ident,
                        "copy"
                            | "copy2"
                            | "copyfile"
                            | "copyfileobj"
                            | "copytree"
                            | "copystat"
                            | "copymode"
                            | "rmtree"
                            | "move"
                            | "disk_usage"
                            | "get_terminal_size"
                            | "chown"
                            | "which"
                            | "make_archive"
                            | "unpack_archive"
                    ) && idx >= 2
                        && matches!(tokens.get(idx - 1), Some(PyToken::Dot))
                        && matches!(tokens.get(idx - 2), Some(PyToken::Ident(m)) if shutil_mod_names.contains(m)));
                if is_shutil_ref {
                    defined_symbols.insert("py_dynamic_io".to_string());
                }
            }
            // Track explicit Python filesystem reads (`open("fixtures/input.bin", "rb")`,
            // `Path("config/settings.json")`) regardless of file extension, and fail closed on
            // interpolated/dynamic paths (`Path(f"fixtures/{name}.json")`, `open(var)`),
            // edge whitespace, named `\N{...}` escapes, or metadata/directory-existence APIs.
            if !is_def && matches!(tokens.get(idx + 1), Some(PyToken::LParen)) {
                // Detect direct calls on dynamically imported I/O/env modules such as
                // `__import__("subprocess").run(...)`, `importlib.import_module("os").getenv(...)`,
                // or `__import__("builtins").open("fixtures/input.bin")`.
                if (import_fn_names.contains(ident)
                    || (ident == "import_module"
                        && idx >= 2
                        && matches!(tokens.get(idx - 1), Some(PyToken::Dot))
                        && matches!(tokens.get(idx - 2), Some(PyToken::Ident(m)) if importlib_mod_names.contains(m))))
                    && let Some(PyToken::StringLit(mod_s)) = tokens.get(idx + 2)
                    && matches!(
                        mod_s.as_str(),
                        "os"
                            | "pathlib"
                            | "io"
                            | "builtins"
                            | "subprocess"
                            | "shutil"
                            | "asyncio"
                            | "os.path"
                            | "posixpath"
                            | "ntpath"
                    )
                {
                    let mut close_k = idx + 2;
                    let mut p_d = 1usize;
                    while close_k < tokens.len() && p_d > 0 {
                        match tokens[close_k] {
                            PyToken::LParen => p_d += 1,
                            PyToken::RParen => p_d -= 1,
                            _ => {}
                        }
                        close_k += 1;
                    }
                    if matches!(mod_s.as_str(), "subprocess" | "shutil")
                        || matches!(tokens.get(close_k), Some(PyToken::Dot))
                    {
                        defined_symbols.insert("py_dynamic_io".to_string());
                        if mod_s == "os" {
                            defined_symbols.insert("py_dynamic_env".to_string());
                        }
                    }
                }
                let is_subprocess_call = subprocess_fn_names.contains(ident)
                    || (matches!(ident, "run" | "call" | "getoutput" | "getstatusoutput")
                        && idx >= 2
                        && matches!(tokens.get(idx - 1), Some(PyToken::Dot))
                        && matches!(tokens.get(idx - 2), Some(PyToken::Ident(m)) if subprocess_mod_names.contains(m)));
                let is_shutil_call = shutil_fn_names.contains(ident)
                    || (matches!(
                        ident,
                        "copy"
                            | "copy2"
                            | "copyfile"
                            | "copyfileobj"
                            | "copytree"
                            | "copystat"
                            | "copymode"
                            | "rmtree"
                            | "move"
                            | "disk_usage"
                            | "get_terminal_size"
                            | "chown"
                            | "which"
                            | "make_archive"
                            | "unpack_archive"
                    ) && idx >= 2
                        && matches!(tokens.get(idx - 1), Some(PyToken::Dot))
                        && matches!(tokens.get(idx - 2), Some(PyToken::Ident(m)) if shutil_mod_names.contains(m)));
                if (is_subprocess_call
                    || is_shutil_call
                    || dynamic_io_fn_names.contains(ident)
                    || matches!(
                        ident,
                        "exists"
                            | "isfile"
                            | "isdir"
                            | "lexists"
                            | "stat"
                            | "lstat"
                            | "listdir"
                            | "scandir"
                            | "realpath"
                            | "readlink"
                    )) && !py_in_import_stmt[idx]
                {
                    defined_symbols.insert("py_dynamic_io".to_string());
                } else if open_fn_names.contains(ident) {
                    if let Some((path_lit, next_idx)) = collect_py_str_lit(idx + 2, true)
                        && matches!(tokens.get(next_idx), Some(PyToken::RParen | PyToken::Comma))
                    {
                        if let Some(clean) = clean_py_static_path_seg(&path_lit) {
                            data_refs.insert(clean.clone());
                            defined_symbols.insert(format!("py_raw_fs:{clean}"));
                        } else {
                            defined_symbols.insert("py_dynamic_io".to_string());
                        }
                    } else if !matches!(tokens.get(idx + 2), Some(PyToken::RParen)) {
                        defined_symbols.insert("py_dynamic_io".to_string());
                    }
                } else if path_ctor_names.contains(ident) {
                    // `Path("fixtures", "../outside/secret")` and empty `Path() / "a" / "b"` join
                    // positional arguments and chained `/` / `.joinpath(...)` segments; fail closed
                    // on dynamic segments, adjacent-literal failures, or path transformations
                    // (`.with_suffix`, `.with_name`, `.with_stem`, `.parent`, `.glob`, etc.)!
                    let mut p_idx = idx + 2;
                    let mut parts: Vec<String> = Vec::new();
                    let mut all_static_valid = true;
                    let mut is_empty_path_ctor = false;
                    if matches!(tokens.get(p_idx), Some(PyToken::RParen)) {
                        is_empty_path_ctor = true;
                    } else {
                        while p_idx < tokens.len() {
                            if let Some((part_lit, next_idx)) = collect_py_str_lit(p_idx, true) {
                                let Some(part_clean) = clean_py_static_path_seg(&part_lit) else {
                                    all_static_valid = false;
                                    break;
                                };
                                parts.push(part_clean);
                                p_idx = next_idx;
                                if matches!(tokens.get(p_idx), Some(PyToken::Comma)) {
                                    p_idx += 1;
                                    if matches!(tokens.get(p_idx), Some(PyToken::RParen)) {
                                        break;
                                    }
                                    continue;
                                }
                                if matches!(tokens.get(p_idx), Some(PyToken::RParen)) {
                                    break;
                                }
                                all_static_valid = false;
                                break;
                            }
                            all_static_valid = false;
                            break;
                        }
                    }
                    let mut lead_parens = 0usize;
                    let mut bp = idx;
                    while bp > 0 && matches!(tokens.get(bp - 1), Some(PyToken::LParen)) {
                        lead_parens += 1;
                        bp -= 1;
                    }
                    let mut chain_idx = p_idx + 1;
                    if all_static_valid {
                        // Also follow chained `Path("fixtures") / "../secret"`,
                        // `(Path("assets") / "subdir") / "../payload.bin"`, `Path() / "a" "b"`,
                        // and `.joinpath(...)` segments.
                        while chain_idx < tokens.len() {
                            if lead_parens > 0 && matches!(tokens.get(chain_idx), Some(PyToken::RParen)) {
                                lead_parens -= 1;
                                chain_idx += 1;
                                continue;
                            }
                            if matches!(tokens.get(chain_idx), Some(PyToken::Slash)) {
                                if let Some((seg_lit, next_idx)) =
                                    collect_py_str_lit(chain_idx + 1, false)
                                {
                                    // Require the string literal to be the COMPLETE operand of `/`
                                    // (never `Path("a") / "B".lower()`, `Path("a") / "b"[1:]`, or
                                    // `Path("a") / "b" + c`, where `.` / `[` / `+` bind tighter!).
                                    let operand_complete = matches!(
                                        tokens.get(next_idx),
                                        None | Some(
                                            PyToken::Slash
                                                | PyToken::RParen
                                                | PyToken::Comma
                                                | PyToken::Newline
                                                | PyToken::Colon
                                                | PyToken::RBracket
                                        )
                                    );
                                    if !operand_complete {
                                        all_static_valid = false;
                                        break;
                                    }
                                    let Some(seg_clean) = clean_py_static_path_seg(&seg_lit) else {
                                        all_static_valid = false;
                                        break;
                                    };
                                    parts.push(seg_clean);
                                    chain_idx = next_idx;
                                    continue;
                                }
                                all_static_valid = false;
                                break;
                            } else if matches!(tokens.get(chain_idx), Some(PyToken::Dot))
                                && matches!(
                                    tokens.get(chain_idx + 1),
                                    Some(PyToken::Ident("joinpath"))
                                )
                                && matches!(tokens.get(chain_idx + 2), Some(PyToken::LParen))
                            {
                                let mut j_idx = chain_idx + 3;
                                if matches!(tokens.get(j_idx), Some(PyToken::RParen)) {
                                    chain_idx = j_idx + 1;
                                    continue;
                                }
                                while j_idx < tokens.len() {
                                    if let Some((seg_lit, next_idx)) =
                                        collect_py_str_lit(j_idx, true)
                                    {
                                        let Some(seg_clean) = clean_py_static_path_seg(&seg_lit)
                                        else {
                                            all_static_valid = false;
                                            break;
                                        };
                                        parts.push(seg_clean);
                                        j_idx = next_idx;
                                        if matches!(tokens.get(j_idx), Some(PyToken::Comma)) {
                                            j_idx += 1;
                                            if matches!(tokens.get(j_idx), Some(PyToken::RParen)) {
                                                break;
                                            }
                                            continue;
                                        }
                                        if matches!(tokens.get(j_idx), Some(PyToken::RParen)) {
                                            break;
                                        }
                                    }
                                    all_static_valid = false;
                                    break;
                                }
                                if !all_static_valid {
                                    break;
                                }
                                chain_idx = j_idx + 1;
                                continue;
                            }
                            break;
                        }
                    }
                    // Skip any wrapping `)` around `(Path("a") / "b").with_suffix(...)` when checking
                    // for path-transforming attributes/methods (`.with_suffix`, `.with_name`,
                    // `.with_stem`, `.parent`, `.parents`, `.resolve`, `.expanduser`, `.glob`, etc.).
                    let mut after_chain = chain_idx;
                    while matches!(tokens.get(after_chain), Some(PyToken::RParen)) {
                        after_chain += 1;
                    }
                    let has_path_transform = matches!(tokens.get(after_chain), Some(PyToken::Dot))
                        && matches!(
                            tokens.get(after_chain + 1),
                            Some(PyToken::Ident(
                                "with_suffix"
                                    | "with_name"
                                    | "with_stem"
                                    | "parent"
                                    | "parents"
                                    | "resolve"
                                    | "expanduser"
                                    | "absolute"
                                    | "glob"
                                    | "rglob"
                                    | "iterdir"
                                    | "relative_to"
                                    | "exists"
                                    | "is_dir"
                                    | "is_file"
                                    | "is_symlink"
                                    | "is_mount"
                                    | "is_junction"
                                    | "is_socket"
                                    | "is_fifo"
                                    | "is_block_device"
                                    | "is_char_device"
                                    | "stat"
                                    | "lstat"
                                    | "samefile"
                                    | "owner"
                                    | "group"
                                    | "readlink"
                                    | "walk"
                            ))
                        );
                    if has_path_transform {
                        defined_symbols.insert("py_dynamic_io".to_string());
                    } else if all_static_valid && !parts.is_empty() {
                        let joined = parts.join("/");
                        data_refs.insert(joined.clone());
                        defined_symbols.insert(format!("py_raw_fs:{joined}"));
                    } else if !all_static_valid || !is_empty_path_ctor {
                        defined_symbols.insert("py_dynamic_io".to_string());
                    }
                }
            }
            // Detect reflective Python environment access on `os` or any `os` alias (including
            // parenthesized `(os)` and `__dict__` / `__getattribute__` via `getattr`):
            // `getattr(os, "getenv")`, `getattr((os), "environ")`, `getattr(os, "get" + "env")`,
            // `getattr(os, "__dict__")`, `(os).__dict__`, `vars((system))`.
            if !is_def
                && ident == "getattr"
                && matches!(tokens.get(idx + 1), Some(PyToken::LParen))
            {
                let mut obj_idx = idx + 2;
                let mut obj_parens = 0usize;
                while matches!(tokens.get(obj_idx), Some(PyToken::LParen)) {
                    obj_idx += 1;
                    obj_parens += 1;
                }
                let is_dyn_import_obj = (matches!(
                    tokens.get(obj_idx),
                    Some(PyToken::Ident(r)) if import_fn_names.contains(r)
                ) && matches!(tokens.get(obj_idx + 1), Some(PyToken::LParen))
                    && matches!(
                        tokens.get(obj_idx + 2),
                        Some(PyToken::StringLit(m))
                            if matches!(
                                m.as_str(),
                                "os"
                                    | "pathlib"
                                    | "io"
                                    | "builtins"
                                    | "subprocess"
                                    | "asyncio"
                                    | "os.path"
                                    | "posixpath"
                                    | "ntpath"
                            )
                    ))
                    || (matches!(
                        tokens.get(obj_idx),
                        Some(PyToken::Ident(m)) if importlib_mod_names.contains(m)
                    ) && matches!(tokens.get(obj_idx + 1), Some(PyToken::Dot))
                        && matches!(tokens.get(obj_idx + 2), Some(PyToken::Ident("import_module")))
                        && matches!(tokens.get(obj_idx + 3), Some(PyToken::LParen))
                        && matches!(
                            tokens.get(obj_idx + 4),
                            Some(PyToken::StringLit(m))
                                if matches!(
                                    m.as_str(),
                                    "os"
                                        | "pathlib"
                                        | "io"
                                        | "builtins"
                                        | "subprocess"
                                        | "asyncio"
                                        | "os.path"
                                        | "posixpath"
                                        | "ntpath"
                                )
                        ));
                if is_dyn_import_obj {
                    // Reflective access on a dynamically imported I/O/env module such as
                    // `getattr(__import__('builtins'), 'open')` or
                    // `getattr(importlib.import_module('os'), 'getenv')`: fail closed!
                    defined_symbols.insert("py_dynamic_io".to_string());
                    defined_symbols.insert("py_dynamic_env".to_string());
                }
                let mut is_os_obj = matches!(
                    tokens.get(obj_idx),
                    Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                );
                let is_fs_mod_obj = matches!(
                    tokens.get(obj_idx),
                    Some(PyToken::Ident(m))
                        if os_path_mod_names.contains(m)
                            || pathlib_mod_names.contains(m)
                            || io_mod_names.contains(m)
                            || subprocess_mod_names.contains(m)
                            || shutil_mod_names.contains(m)
                            || asyncio_mod_names.contains(m)
                );
                if is_os_obj || is_fs_mod_obj {
                    let mut after_obj = obj_idx + 1;
                    let mut rem_parens = obj_parens;
                    // Support `getattr(os.path, "exists")`, `getattr((os.path), "exists")`,
                    // and `getattr((os).path, "exists")`!
                    while rem_parens > 0 && matches!(tokens.get(after_obj), Some(PyToken::RParen)) {
                        after_obj += 1;
                        rem_parens -= 1;
                    }
                    if is_os_obj
                        && matches!(tokens.get(after_obj), Some(PyToken::Dot))
                        && matches!(tokens.get(after_obj + 1), Some(PyToken::Ident("path")))
                    {
                        after_obj += 2;
                        is_os_obj = false;
                        while rem_parens > 0
                            && matches!(tokens.get(after_obj), Some(PyToken::RParen))
                        {
                            after_obj += 1;
                            rem_parens -= 1;
                        }
                    }
                    let closed_ok = rem_parens == 0;
                    if closed_ok && matches!(tokens.get(after_obj), Some(PyToken::Comma)) {
                        let attr_start = after_obj + 1;
                        let single_lit_attr = if matches!(
                            tokens.get(attr_start + 1),
                            Some(PyToken::RParen | PyToken::Comma)
                        ) && let Some(PyToken::StringLit(s)) = tokens.get(attr_start)
                        {
                            Some(s.as_str())
                        } else {
                            None
                        };
                        if is_os_obj {
                            match single_lit_attr {
                                Some(s)
                                    if matches!(
                                        s,
                                        "getenv"
                                            | "getenvb"
                                            | "get_exec_path"
                                            | "environ"
                                            | "environb"
                                            | "__dict__"
                                            | "__getattribute__"
                                            | "__getattr__"
                                    ) =>
                                {
                                    defined_symbols.insert("py_dynamic_env".to_string());
                                    if matches!(s, "__dict__" | "__getattribute__" | "__getattr__") {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                }
                                Some(s)
                                    if dynamic_io_fn_names.contains(s)
                                        || matches!(s, "open" | "open_code" | "path") =>
                                {
                                    defined_symbols.insert("py_dynamic_io".to_string());
                                }
                                None => {
                                    defined_symbols.insert("py_dynamic_env".to_string());
                                    defined_symbols.insert("py_dynamic_io".to_string());
                                }
                                Some(_) => {}
                            }
                        } else {
                            // Reflective attribute lookup on `os.path`, `pathlib`, `io`, `builtins`,
                            // `subprocess`, or `asyncio` (e.g. `getattr(os_path, "exists")`):
                            // fail closed on dynamic or I/O/subprocess/Path attributes!
                            let is_safe_lit = single_lit_attr.is_some_and(|s| {
                                !dynamic_io_fn_names.contains(s)
                                    && !path_ctor_names.contains(s)
                                    && !matches!(
                                        s,
                                        "open"
                                            | "open_code"
                                            | "run"
                                            | "call"
                                            | "getoutput"
                                            | "getstatusoutput"
                                            | "copy"
                                            | "move"
                                            | "which"
                                            | "disk_usage"
                                            | "get_terminal_size"
                                            | "copyfileobj"
                                            | "rmtree"
                                            | "chown"
                                            | "__dict__"
                                            | "__getattribute__"
                                            | "__getattr__"
                                    )
                            });
                            if !is_safe_lit {
                                defined_symbols.insert("py_dynamic_io".to_string());
                            }
                        }
                    }
                }
            }
            if !is_def
                && matches!(ident, "__dict__" | "__getattribute__" | "__getattr__")
                && idx >= 2
                && matches!(tokens.get(idx - 1), Some(PyToken::Dot))
            {
                let mut lhs = idx - 2;
                while lhs > 0 && matches!(tokens.get(lhs), Some(PyToken::RParen)) {
                    lhs -= 1;
                }
                if matches!(
                    tokens.get(lhs),
                    Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                ) {
                    defined_symbols.insert("py_dynamic_env".to_string());
                    defined_symbols.insert("py_dynamic_io".to_string());
                } else if matches!(
                    tokens.get(lhs),
                    Some(PyToken::Ident(m))
                        if os_path_mod_names.contains(m)
                            || pathlib_mod_names.contains(m)
                            || io_mod_names.contains(m)
                            || subprocess_mod_names.contains(m)
                            || shutil_mod_names.contains(m)
                            || asyncio_mod_names.contains(m)
                            || importlib_mod_names.contains(m)
                ) {
                    defined_symbols.insert("py_dynamic_io".to_string());
                }
            }
            if !is_def
                && ident == "vars"
                && matches!(tokens.get(idx + 1), Some(PyToken::LParen))
            {
                let mut obj_idx = idx + 2;
                while matches!(tokens.get(obj_idx), Some(PyToken::LParen)) {
                    obj_idx += 1;
                }
                if matches!(
                    tokens.get(obj_idx),
                    Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                ) {
                    defined_symbols.insert("py_dynamic_env".to_string());
                    defined_symbols.insert("py_dynamic_io".to_string());
                } else if matches!(
                    tokens.get(obj_idx),
                    Some(PyToken::Ident(m))
                        if os_path_mod_names.contains(m)
                            || pathlib_mod_names.contains(m)
                            || io_mod_names.contains(m)
                            || subprocess_mod_names.contains(m)
                            || shutil_mod_names.contains(m)
                            || asyncio_mod_names.contains(m)
                            || importlib_mod_names.contains(m)
                ) {
                    defined_symbols.insert("py_dynamic_io".to_string());
                }
            }
            if !is_def && get_exec_path_names.contains(ident) {
                env_vars.insert("PATH".to_string());
            }
            if !is_def && getenv_names.contains(ident) {
                if matches!(tokens.get(idx + 1), Some(PyToken::LParen)) {
                    // Require the string literal to be the ENTIRE first argument (followed
                    // immediately by `)` or `,`) so expressions like `os.getenv("APP_" + "MODE")`
                    // never record only the partial prefix `"APP_"`.
                    if let Some(PyToken::StringLit(var_name)) = tokens.get(idx + 2)
                        && matches!(tokens.get(idx + 3), Some(PyToken::RParen | PyToken::Comma))
                        && is_valid_env_var_name(var_name)
                    {
                        env_vars.insert(var_name.trim().to_string());
                    } else if !matches!(tokens.get(idx + 2), Some(PyToken::RParen)) {
                        defined_symbols.insert("py_dynamic_env".to_string());
                    }
                } else {
                    // Escaping `os.getenv` / `getenv` function value (`map(os.getenv, keys)`,
                    // module-level `GET = os.getenv`, `return os.getenv`) cannot be tracked
                    // statically: fail closed unless it is an import or function-local alias!
                    let is_import_or_local_alias = (py_in_import_stmt[idx]
                        && idx > 0
                        && matches!(
                            tokens.get(idx - 1),
                            Some(
                                PyToken::Ident("import" | "as")
                                    | PyToken::Comma
                                    | PyToken::LParen
                            )
                        ))
                        || getenv_alias_lhs_indices.contains(&idx)
                        || getenv_alias_rhs_indices.contains(&idx);
                    if !is_import_or_local_alias {
                        defined_symbols.insert("py_dynamic_env".to_string());
                    }
                }
            } else if !is_def && environ_names.contains(ident) {
                // Skip pure import / alias-binding occurrences on BOTH sides (`from os import environ`,
                // `from os import environ as E`, `E = os.environ`, `E2 = E`).
                let is_import_or_alias_decl = (py_in_import_stmt[idx]
                    && idx > 0
                    && matches!(
                        tokens.get(idx - 1),
                        Some(PyToken::Ident("import" | "as") | PyToken::Comma | PyToken::LParen)
                    ))
                    || environ_alias_lhs_indices.contains(&idx)
                    || environ_alias_rhs_indices.contains(&idx);
                if is_import_or_alias_decl {
                    continue;
                }
                // Check membership tests: `"FEATURE" in os.environ`, `"FEATURE" not in os.environ`,
                // `key in os.environ`, `key in E`.
                let in_pos = if idx >= 1 && matches!(tokens.get(idx - 1), Some(PyToken::Ident("in"))) {
                    Some(idx - 1)
                } else if idx >= 3
                    && matches!(tokens.get(idx - 1), Some(PyToken::Dot))
                    && matches!(
                        tokens.get(idx - 2),
                        Some(PyToken::Ident(m)) if os_mod_names.contains(m)
                    )
                    && matches!(tokens.get(idx - 3), Some(PyToken::Ident("in")))
                {
                    Some(idx - 3)
                } else {
                    None
                };
                if let Some(ip) = in_pos {
                    let lhs_idx = if ip >= 2
                        && matches!(tokens.get(ip - 1), Some(PyToken::Ident("not")))
                    {
                        ip - 2
                    } else if ip >= 1 {
                        ip - 1
                    } else {
                        usize::MAX
                    };
                    if lhs_idx < tokens.len()
                        && matches!(tokens.get(lhs_idx), Some(PyToken::StringLit(_)))
                    {
                        // Fold adjacent implicit-concatenation string literals (`"APP_" "MODE" in os.environ`)!
                        let mut first_lit = lhs_idx;
                        while first_lit > 0
                            && matches!(tokens.get(first_lit - 1), Some(PyToken::StringLit(_)))
                        {
                            first_lit -= 1;
                        }
                        let prev_of_first = first_lit.checked_sub(1).and_then(|p| tokens.get(p));
                        let mut folded = String::new();
                        for tok_s in &tokens[first_lit..=lhs_idx] {
                            if let PyToken::StringLit(s) = tok_s {
                                folded.push_str(s);
                            }
                        }
                        if !matches!(prev_of_first, Some(PyToken::Other))
                            && is_valid_env_var_name(&folded)
                        {
                            env_vars.insert(folded);
                        } else {
                            defined_symbols.insert("py_dynamic_env".to_string());
                        }
                    } else {
                        defined_symbols.insert("py_dynamic_env".to_string());
                    }
                    continue;
                }
                if matches!(tokens.get(idx + 1), Some(PyToken::Dot))
                    && matches!(
                        tokens.get(idx + 2),
                        Some(PyToken::Ident("get" | "pop" | "setdefault"))
                    )
                    && matches!(tokens.get(idx + 3), Some(PyToken::LParen))
                {
                    if let Some(PyToken::StringLit(var_name)) = tokens.get(idx + 4)
                        && matches!(tokens.get(idx + 5), Some(PyToken::RParen | PyToken::Comma))
                        && is_valid_env_var_name(var_name)
                    {
                        env_vars.insert(var_name.trim().to_string());
                    } else if !matches!(tokens.get(idx + 4), Some(PyToken::RParen)) {
                        defined_symbols.insert("py_dynamic_env".to_string());
                    }
                } else if matches!(tokens.get(idx + 1), Some(PyToken::LBracket)) {
                    if let Some(PyToken::StringLit(var_name)) = tokens.get(idx + 2)
                        && matches!(tokens.get(idx + 3), Some(PyToken::RBracket))
                        && is_valid_env_var_name(var_name)
                    {
                        env_vars.insert(var_name.trim().to_string());
                    } else {
                        defined_symbols.insert("py_dynamic_env".to_string());
                    }
                } else {
                    // Any other whole-mapping use (`dict(os.environ)`, `env=os.environ`,
                    // `os.environ.items()`, `os.environ.copy()`, `list(os.environ)`, etc.)
                    // reads the entire environment -> fail closed!
                    defined_symbols.insert("py_dynamic_env".to_string());
                }
            }
        }
    }

    let n = tokens.len();
    let mut i = 0usize;
    let mut at_statement_start = true;
    let mut in_type_checking = false;
    let mut pending_fixture_decorator = false;
    let mut pending_fixture_custom_name: Option<String> = None;

    while i < n {
        match &tokens[i] {
            PyToken::StartTypeChecking => {
                in_type_checking = true;
                i += 1;
            }
            PyToken::EndTypeChecking => {
                in_type_checking = false;
                at_statement_start = true;
                i += 1;
            }
            PyToken::Newline => {
                at_statement_start = true;
                i += 1;
            }
            PyToken::At if at_statement_start => {
                // Decorator: check if `@pytest.fixture` / `@fixture` or `@pytest.mark.usefixtures(...)`
                i += 1;
                let mut deco_parts = Vec::new();
                while i < n {
                    match &tokens[i] {
                        PyToken::Ident(name) => {
                            deco_parts.push(*name);
                            i += 1;
                        }
                        PyToken::Dot => {
                            i += 1;
                        }
                        _ => break,
                    }
                }
                let is_fixture_deco = deco_parts.last().copied() == Some("fixture");
                let is_usefixtures_deco = deco_parts.last().copied() == Some("usefixtures");
                let is_http_route_deco = matches!(
                    deco_parts.last().copied(),
                    Some("get" | "post" | "put" | "delete" | "patch" | "route" | "api_route")
                );
                if deco_parts.len() >= 3
                    && deco_parts[deco_parts.len() - 2] == "mark"
                    && let Some(marker) = deco_parts.last()
                {
                    markers.insert((*marker).to_string());
                }
                if is_fixture_deco {
                    pending_fixture_decorator = true;
                }
                // Handle leading dynamic-import decorators like
                // `@__import__("pkg.decorators", fromlist=["decorator"]).decorator` or
                // `@importlib.import_module("pkg.decorators").decorator` by rewinding to the
                // import function token so the decorator loop's dynamic-import parser runs on it!
                if deco_parts
                    .last()
                    .is_some_and(|p| import_fn_names.contains(*p) || *p == "import_module")
                    && matches!(tokens.get(i), Some(PyToken::LParen))
                    && i > 0
                {
                    i -= 1;
                }
                // Parse decorator arguments (including balanced parens), also tracking nested
                // dynamic imports (`@pytest.mark.parametrize("x", __import__("cases").DATA)`),
                // non-literal `usefixtures(NAME)`, and non-literal `autouse=FLAG` / `name=VAR`!
                let mut paren_depth = 0usize;
                let mut saw_nonlit_usefixtures_deco = false;
                while i < n {
                    match &tokens[i] {
                        PyToken::LParen => {
                            paren_depth += 1;
                            i += 1;
                        }
                        PyToken::RParen => {
                            paren_depth = paren_depth.saturating_sub(1);
                            i += 1;
                        }
                        PyToken::Ident("name")
                            if is_fixture_deco
                                && paren_depth == 1
                                && matches!(tokens.get(i + 1), Some(PyToken::Assign)) =>
                        {
                            // Skip continuation newlines and join adjacent string literals
                            // (`name="db"\n "_fixture"`), validating the ENTIRE argument ends at
                            // `,` or `)` (after optional newlines)!
                            let mut k = i + 2;
                            while matches!(tokens.get(k), Some(PyToken::Newline)) {
                                k += 1;
                            }
                            let mut joined = String::new();
                            let mut saw_lit = false;
                            while let Some(PyToken::StringLit(part)) = tokens.get(k) {
                                joined.push_str(part);
                                saw_lit = true;
                                k += 1;
                                let mut peek = k;
                                while matches!(tokens.get(peek), Some(PyToken::Newline)) {
                                    peek += 1;
                                }
                                if matches!(tokens.get(peek), Some(PyToken::StringLit(_))) {
                                    k = peek;
                                } else {
                                    break;
                                }
                            }
                            while matches!(tokens.get(k), Some(PyToken::Newline)) {
                                k += 1;
                            }
                            if saw_lit
                                && !joined.is_empty()
                                && matches!(tokens.get(k), Some(PyToken::Comma | PyToken::RParen))
                            {
                                pending_fixture_custom_name = Some(joined);
                                i = k;
                            } else {
                                // Non-literal fixture `name=` (e.g. `@pytest.fixture(name=VAR)`):
                                // conservatively retain this conftest for all tests!
                                module_markers.insert("autouse".to_string());
                                defined_symbols.insert("py_dynamic_io".to_string());
                                i += 2;
                            }
                        }
                        PyToken::Ident("autouse")
                            if is_fixture_deco
                                && paren_depth == 1
                                && matches!(tokens.get(i + 1), Some(PyToken::Assign)) =>
                        {
                            // Skip continuation newlines and require `,` or `)` immediately after
                            // `False`/`None` so `autouse=False\n or True` is NOT treated as false!
                            let mut k = i + 2;
                            while matches!(tokens.get(k), Some(PyToken::Newline)) {
                                k += 1;
                            }
                            let is_lit_false = if matches!(
                                tokens.get(k),
                                Some(PyToken::Ident("False" | "None"))
                            ) {
                                let mut after = k + 1;
                                while matches!(tokens.get(after), Some(PyToken::Newline)) {
                                    after += 1;
                                }
                                matches!(tokens.get(after), Some(PyToken::Comma | PyToken::RParen))
                            } else {
                                false
                            };
                            if !is_lit_false {
                                // Both literal `autouse=True` AND non-literal `autouse=FLAG` /
                                // `autouse=False\n or True` conservatively mark as autouse!
                                module_markers.insert("autouse".to_string());
                            }
                            i += 2;
                        }
                        PyToken::Ident(fn_name)
                            if (import_fn_names.contains(fn_name) || *fn_name == "import_module")
                                && matches!(tokens.get(i + 1), Some(PyToken::LParen)) =>
                        {
                            // Join adjacent string literals (including across implicit-continuation
                            // newlines inside parens) for both the module name and `fromlist=[...]`!
                            let mut m_idx = i + 2;
                            while matches!(tokens.get(m_idx), Some(PyToken::Newline)) {
                                m_idx += 1;
                            }
                            let mut mod_joined = String::new();
                            let mut saw_mod_lit = false;
                            while let Some(PyToken::StringLit(part)) = tokens.get(m_idx) {
                                mod_joined.push_str(part);
                                saw_mod_lit = true;
                                m_idx += 1;
                                let mut peek = m_idx;
                                while matches!(tokens.get(peek), Some(PyToken::Newline)) {
                                    peek += 1;
                                }
                                if matches!(tokens.get(peek), Some(PyToken::StringLit(_))) {
                                    m_idx = peek;
                                } else {
                                    break;
                                }
                            }
                            while matches!(tokens.get(m_idx), Some(PyToken::Newline)) {
                                m_idx += 1;
                            }
                            if saw_mod_lit && is_valid_dotted_module(&mod_joined) {
                                imports.insert(ImportSpec {
                                    level: 0,
                                    module: mod_joined.clone(),
                                    symbols: Vec::new(),
                                });
                                if matches!(tokens.get(m_idx), Some(PyToken::RParen)) {
                                    // Simple single-arg `__import__("cases")` / `import_module("cases")`.
                                } else if matches!(tokens.get(m_idx), Some(PyToken::Comma)) {
                                    // Multi-arg `__import__("pkg", fromlist=["cases"])` or
                                    // `import_module(..., package=...)`: extract literal `fromlist`
                                    // submodules (joining adjacent/multiline literals!) AND fail
                                    // closed if non-literal/complex args appear!
                                    let mut k = m_idx + 1;
                                    let mut call_d = 1usize;
                                    let mut saw_fromlist_lits = false;
                                    let mut saw_unparsed_extra = false;
                                    while k < n && call_d > 0 {
                                        match &tokens[k] {
                                            PyToken::LParen => {
                                                call_d += 1;
                                                k += 1;
                                            }
                                            PyToken::RParen => {
                                                call_d -= 1;
                                                k += 1;
                                            }
                                            PyToken::Ident("fromlist")
                                                if call_d == 1
                                                    && matches!(
                                                        tokens.get(k + 1),
                                                        Some(PyToken::Assign)
                                                    )
                                                    && matches!(
                                                        tokens.get(k + 2),
                                                        Some(PyToken::LBracket | PyToken::LParen)
                                                    ) =>
                                            {
                                                let fl_is_paren =
                                                    matches!(tokens.get(k + 2), Some(PyToken::LParen));
                                                k += 3;
                                                let mut sub_d = 1usize;
                                                let mut fl_comma_at_d1 = false;
                                                let mut fl_items: Vec<String> = Vec::new();
                                                while k < n && sub_d > 0 {
                                                    match &tokens[k] {
                                                        PyToken::LBracket | PyToken::LParen => {
                                                            sub_d += 1;
                                                            k += 1;
                                                        }
                                                        PyToken::RBracket | PyToken::RParen => {
                                                            sub_d -= 1;
                                                            k += 1;
                                                        }
                                                        PyToken::StringLit(sub) if sub_d == 1 => {
                                                            let mut sub_joined = sub.clone();
                                                            k += 1;
                                                            loop {
                                                                let mut peek = k;
                                                                while matches!(
                                                                    tokens.get(peek),
                                                                    Some(PyToken::Newline)
                                                                ) {
                                                                    peek += 1;
                                                                }
                                                                if let Some(PyToken::StringLit(np)) =
                                                                    tokens.get(peek)
                                                                {
                                                                    sub_joined.push_str(np);
                                                                    k = peek + 1;
                                                                } else {
                                                                    break;
                                                                }
                                                            }
                                                            fl_items.push(sub_joined);
                                                        }
                                                        PyToken::Comma => {
                                                            if sub_d == 1 {
                                                                fl_comma_at_d1 = true;
                                                            }
                                                            k += 1;
                                                        }
                                                        PyToken::Newline => {
                                                            k += 1;
                                                        }
                                                        _ => {
                                                            saw_unparsed_extra = true;
                                                            k += 1;
                                                        }
                                                    }
                                                }
                                                // Grouping parens without comma `fromlist=("ab")`
                                                // evaluate to the string `"ab"` -> iterates `'a'`, `'b'`!
                                                let expanded_fl: Vec<String> =
                                                    if fl_is_paren && !fl_comma_at_d1 && fl_items.len() == 1 {
                                                        fl_items[0].chars().map(|c| c.to_string()).collect()
                                                    } else {
                                                        fl_items
                                                    };
                                                for sub_joined in expanded_fl {
                                                    if !sub_joined.is_empty() && sub_joined != "*" {
                                                        let full = format!("{mod_joined}.{sub_joined}");
                                                        if is_valid_dotted_module(&full) {
                                                            imports.insert(ImportSpec {
                                                                level: 0,
                                                                module: full,
                                                                symbols: Vec::new(),
                                                            });
                                                            saw_fromlist_lits = true;
                                                        } else {
                                                            saw_unparsed_extra = true;
                                                        }
                                                    } else {
                                                        saw_unparsed_extra = true;
                                                    }
                                                }
                                            }
                                            PyToken::Comma | PyToken::Newline => {
                                                k += 1;
                                            }
                                            _ => {
                                                saw_unparsed_extra = true;
                                                k += 1;
                                            }
                                        }
                                    }
                                    if !saw_fromlist_lits || saw_unparsed_extra {
                                        defined_symbols.insert("py_dynamic_io".to_string());
                                    }
                                } else {
                                    defined_symbols.insert("py_dynamic_io".to_string());
                                }
                            } else {
                                defined_symbols.insert("py_dynamic_io".to_string());
                            }
                            i += 1;
                        }
                        PyToken::StringLit(route_lit)
                            if is_http_route_deco && paren_depth == 1 && route_lit.starts_with('/') =>
                        {
                            defined_symbols.insert(format!("api_route:{}", route_lit.trim()));
                            i += 1;
                        }
                        PyToken::StringLit(fx_name)
                            if is_usefixtures_deco && paren_depth == 1 =>
                        {
                            // Concatenate adjacent Python string literals including across
                            // implicit-continuation newlines (`"db"\n "_fixture"` -> `"db_fixture"`).
                            let mut joined = fx_name.clone();
                            i += 1;
                            loop {
                                let mut peek = i;
                                while matches!(tokens.get(peek), Some(PyToken::Newline)) {
                                    peek += 1;
                                }
                                if let Some(PyToken::StringLit(next_part)) = tokens.get(peek) {
                                    joined.push_str(next_part);
                                    i = peek + 1;
                                } else {
                                    break;
                                }
                            }
                            if !joined.is_empty() {
                                fixtures_used.insert(joined);
                            }
                        }
                        PyToken::Comma | PyToken::Newline if is_usefixtures_deco && paren_depth >= 1 => {
                            i += 1;
                        }
                        PyToken::Newline if paren_depth == 0 => {
                            at_statement_start = true;
                            i += 1;
                            break;
                        }
                        _ => {
                            if is_usefixtures_deco && paren_depth >= 1 {
                                saw_nonlit_usefixtures_deco = true;
                            }
                            i += 1;
                        }
                    }
                }
                if saw_nonlit_usefixtures_deco {
                    module_markers.insert("py_dynamic_fixtures".to_string());
                    defined_symbols.insert("py_dynamic_io".to_string());
                }
            }
            PyToken::Ident("import") if at_statement_start => {
                i += 1;
                let (parsed_specs, next_i) = parse_import_statement(&tokens, i);
                for spec in parsed_specs {
                    if in_type_checking {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
                }
                i = next_i;
                at_statement_start = true;
            }
            PyToken::Ident("from") if at_statement_start => {
                i += 1;
                if let Some((spec, next_i)) = parse_from_import_statement(&tokens, i) {
                    if in_type_checking {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
                    i = next_i;
                }
                at_statement_start = true;
            }
            PyToken::Ident(kw)
                if at_statement_start && matches!(*kw, "def" | "async" | "class") =>
            {
                let is_class = *kw == "class";
                if *kw == "async" {
                    i += 1;
                    if !matches!(tokens.get(i), Some(PyToken::Ident("def"))) {
                        at_statement_start = false;
                        continue;
                    }
                }
                i += 1;
                if let Some(PyToken::Ident(name)) = tokens.get(i) {
                    let sym_name = (*name).to_string();
                    defined_symbols.insert(sym_name.clone());
                    i += 1;
                    if is_class {
                        if sym_name.starts_with("Test") {
                            test_symbols.insert(sym_name);
                        }
                        pending_fixture_decorator = false;
                        pending_fixture_custom_name = None;
                    } else {
                        let is_test_fn = sym_name.starts_with("test_")
                            || sym_name.ends_with("_test")
                            || matches_pytest_function_name(&sym_name, test_function_patterns);
                        if is_test_fn {
                            test_symbols.insert(sym_name.clone());
                        }
                        if pending_fixture_decorator {
                            let fx_name = pending_fixture_custom_name.take().unwrap_or(sym_name);
                            fixtures_defined.insert(fx_name);
                        }
                        if (is_test_fn || pending_fixture_decorator)
                            && matches!(tokens.get(i), Some(PyToken::LParen))
                        {
                            let (params, next_i) = parse_function_params(&tokens, i + 1);
                            for p in params {
                                if p != "self" && p != "cls" {
                                    fixtures_used.insert(p);
                                }
                            }
                            i = next_i;
                        }
                        pending_fixture_decorator = false;
                        pending_fixture_custom_name = None;
                    }
                }
                at_statement_start = false;
            }
            PyToken::Ident("usefixtures") => {
                i += 1;
                if matches!(tokens.get(i), Some(PyToken::LParen)) {
                    i += 1;
                    let mut depth = 1usize;
                    let mut saw_nonlit_usefixture = false;
                    while i < n && depth > 0 {
                        match &tokens[i] {
                            PyToken::LParen => {
                                depth += 1;
                                i += 1;
                            }
                            PyToken::RParen => {
                                depth -= 1;
                                i += 1;
                            }
                            PyToken::StringLit(fx) if depth == 1 => {
                                let mut joined = fx.clone();
                                i += 1;
                                loop {
                                    let mut peek = i;
                                    while matches!(tokens.get(peek), Some(PyToken::Newline)) {
                                        peek += 1;
                                    }
                                    if let Some(PyToken::StringLit(next_part)) = tokens.get(peek) {
                                        joined.push_str(next_part);
                                        i = peek + 1;
                                    } else {
                                        break;
                                    }
                                }
                                if !joined.is_empty() {
                                    fixtures_used.insert(joined);
                                }
                            }
                            PyToken::Comma | PyToken::Newline => {
                                i += 1;
                            }
                            _ => {
                                saw_nonlit_usefixture = true;
                                i += 1;
                            }
                        }
                    }
                    if saw_nonlit_usefixture {
                        module_markers.insert("py_dynamic_fixtures".to_string());
                        defined_symbols.insert("py_dynamic_io".to_string());
                    }
                }
                at_statement_start = false;
            }
            PyToken::Ident("pytestmark") if at_statement_start => {
                i += 1;
                let mut bracket_depth = 0usize;
                let mut usefixtures_active_depth: Option<usize> = None;
                let mut saw_unknown_marker_expr = false;
                if matches!(tokens.get(i), Some(PyToken::Assign)) {
                    i += 1;
                }
                while i < n {
                    match &tokens[i] {
                        PyToken::LBracket | PyToken::LParen => {
                            bracket_depth += 1;
                            i += 1;
                        }
                        PyToken::RBracket | PyToken::RParen => {
                            bracket_depth = bracket_depth.saturating_sub(1);
                            if usefixtures_active_depth.is_some_and(|d| bracket_depth < d) {
                                usefixtures_active_depth = None;
                            }
                            i += 1;
                        }
                        PyToken::Newline if bracket_depth == 0 => break,
                        PyToken::Newline | PyToken::Comma | PyToken::Dot => {
                            i += 1;
                        }
                        PyToken::Ident("pytest")
                            if matches!(tokens.get(i + 1), Some(PyToken::Dot))
                                && matches!(tokens.get(i + 2), Some(PyToken::Ident("mark"))) =>
                        {
                            i += 2;
                        }
                        PyToken::Ident("mark")
                            if matches!(tokens.get(i + 1), Some(PyToken::Dot))
                                && matches!(tokens.get(i + 2), Some(PyToken::Ident(_))) =>
                        {
                            if let Some(PyToken::Ident(marker)) = tokens.get(i + 2) {
                                module_markers.insert((*marker).to_string());
                                if *marker == "usefixtures"
                                    && matches!(tokens.get(i + 3), Some(PyToken::LParen))
                                {
                                    usefixtures_active_depth = Some(bracket_depth + 1);
                                }
                            }
                            i += 3;
                        }
                        PyToken::StringLit(fixture) => {
                            let mut joined = fixture.clone();
                            i += 1;
                            loop {
                                let mut peek = i;
                                while bracket_depth > 0
                                    && matches!(tokens.get(peek), Some(PyToken::Newline))
                                {
                                    peek += 1;
                                }
                                if let Some(PyToken::StringLit(next_part)) = tokens.get(peek) {
                                    joined.push_str(next_part);
                                    i = peek + 1;
                                } else {
                                    break;
                                }
                            }
                            if usefixtures_active_depth.is_some() && !joined.is_empty() {
                                fixtures_used.insert(joined);
                            }
                        }
                        PyToken::Ident(_) => {
                            // Unresolved variable/imported marker OR non-literal `usefixtures(VAR)`
                            // inside `pytestmark = ...` (e.g. `pytestmark = [EXTERNAL_MARK]` or
                            // `pytestmark = pytest.mark.usefixtures(FIXTURE_NAME)`): fail closed!
                            saw_unknown_marker_expr = true;
                            i += 1;
                        }
                        _ => {
                            if usefixtures_active_depth.is_some() {
                                saw_unknown_marker_expr = true;
                            }
                            i += 1;
                        }
                    }
                }
                if saw_unknown_marker_expr {
                    module_markers.insert("py_dynamic_fixtures".to_string());
                    defined_symbols.insert("py_dynamic_io".to_string());
                }
                at_statement_start = false;
            }
            PyToken::Ident("pytest_plugins") if at_statement_start => {
                // `pytest_plugins = [...]` (including annotated `pytest_plugins: list[str] = [...]`
                // and adjacent/multiline concatenated literals `["my"\n "_plugin"]`) in a `conftest.py`
                // loads plugins at collection time for ALL tests under this conftest -> mark `pytest_hook`!
                module_markers.insert("pytest_hook".to_string());
                i += 1;
                if matches!(tokens.get(i), Some(PyToken::Colon)) {
                    // Skip PEP 526 type annotation (`pytest_plugins: list[str] = [...]`).
                    i += 1;
                    let mut ann_d = 0usize;
                    while i < n {
                        match tokens[i] {
                            PyToken::LBracket | PyToken::LParen => {
                                ann_d += 1;
                                i += 1;
                            }
                            PyToken::RBracket | PyToken::RParen => {
                                ann_d = ann_d.saturating_sub(1);
                                i += 1;
                            }
                            PyToken::Assign if ann_d == 0 => break,
                            PyToken::Newline if ann_d == 0 => break,
                            _ => i += 1,
                        }
                    }
                }
                if matches!(tokens.get(i), Some(PyToken::Assign)) {
                    i += 1;
                    // Collect string literals on this assignment; if any computed expression
                    // (`+`, function call, variable identifier) appears in the RHS, fail closed!
                    let mut bracket_depth = 0usize;
                    let mut saw_computed_plugin = false;
                    while i < n {
                        match &tokens[i] {
                            PyToken::LBracket | PyToken::LParen => {
                                bracket_depth += 1;
                                i += 1;
                            }
                            PyToken::RBracket | PyToken::RParen => {
                                bracket_depth = bracket_depth.saturating_sub(1);
                                i += 1;
                            }
                            PyToken::StringLit(s) => {
                                let mut joined = s.clone();
                                i += 1;
                                loop {
                                    let mut peek = i;
                                    while bracket_depth > 0
                                        && matches!(tokens.get(peek), Some(PyToken::Newline))
                                    {
                                        peek += 1;
                                    }
                                    if let Some(PyToken::StringLit(next_part)) = tokens.get(peek) {
                                        joined.push_str(next_part);
                                        i = peek + 1;
                                    } else {
                                        break;
                                    }
                                }
                                if is_valid_dotted_module(&joined) {
                                    imports.insert(ImportSpec {
                                        level: 0,
                                        module: joined,
                                        symbols: Vec::new(),
                                    });
                                } else {
                                    saw_computed_plugin = true;
                                }
                            }
                            PyToken::Comma => i += 1,
                            PyToken::Newline if bracket_depth == 0 => break,
                            PyToken::Newline => i += 1,
                            _ => {
                                saw_computed_plugin = true;
                                i += 1;
                            }
                        }
                    }
                    if saw_computed_plugin {
                        defined_symbols.insert("py_dynamic_import".to_string());
                    }
                } else {
                    // Augmented/dynamic mutation (`pytest_plugins += [...]` or similar): fail closed!
                    defined_symbols.insert("py_dynamic_import".to_string());
                }
                at_statement_start = false;
            }
            PyToken::Ident(fn_name) if import_fn_names.contains(*fn_name) => {
                // Dynamic call: `importlib.import_module("pkg.mod")` / `pytest.importorskip("pkg.mod")`
                // (including aliased `from importlib import import_module as load`)
                // vs computed `load("plugins." + name)` (fail closed!).
                let is_def = i > 0
                    && matches!(tokens.get(i - 1), Some(PyToken::Ident("def" | "class")));
                if !is_def && matches!(tokens.get(i + 1), Some(PyToken::LParen)) {
                    if let Some(PyToken::StringLit(mod_name)) = tokens.get(i + 2)
                        && is_valid_dotted_module(mod_name)
                    {
                        if matches!(tokens.get(i + 3), Some(PyToken::RParen)) {
                            imports.insert(ImportSpec {
                                level: 0,
                                module: mod_name.clone(),
                                symbols: Vec::new(),
                            });
                        } else if matches!(tokens.get(i + 3), Some(PyToken::Comma)) {
                            // Handle `__import__('pkg', fromlist=['feature'])` / positional 4th arg:
                            // if it's `__import__` (or any tracked alias like `from builtins import
                            // __import__ as load` or `load = builtins.__import__`), parse static
                            // string-literal `fromlist` entries or fail closed if `fromlist` is computed!
                            let is_dunder_import = dunder_import_fn_names.contains(*fn_name);
                            if is_dunder_import {
                                let mut j = i + 4;
                                let mut pdepth = 1usize;
                                let mut bdepth = 0usize;
                                let mut fromlist_syms: Vec<String> = Vec::new();
                                let mut dynamic_fromlist = false;
                                let mut in_fromlist = false;
                                let mut fromlist_in_paren = false;
                                let mut fromlist_paren_saw_comma = false;
                                let mut fromlist_paren_lits: Vec<String> = Vec::new();
                                let mut arg_idx = 1usize; // 0=name, 1=globals, 2=locals, 3=fromlist, 4=level
                                while j < n && pdepth > 0 {
                                    // Inside `fromlist=(...)` or positional 4th arg `("feature",)`,
                                    // `pdepth == 2` is the tuple/grouping level; track `bdepth` too
                                    // so `['a', 'b']` commas NEVER increment `arg_idx`!
                                    let collecting_fromlist =
                                        (in_fromlist || arg_idx == 3) && pdepth >= 1;
                                    match &tokens[j] {
                                        PyToken::LParen => {
                                            pdepth += 1;
                                            if collecting_fromlist {
                                                if pdepth == 2 && bdepth == 0 {
                                                    fromlist_in_paren = true;
                                                    fromlist_paren_saw_comma = false;
                                                    fromlist_paren_lits.clear();
                                                } else if pdepth > 2 {
                                                    dynamic_fromlist = true;
                                                }
                                            }
                                        }
                                        PyToken::RParen => {
                                            if collecting_fromlist
                                                && fromlist_in_paren
                                                && pdepth == 2
                                                && bdepth == 0
                                            {
                                                if !fromlist_paren_saw_comma
                                                    && fromlist_paren_lits.len() == 1
                                                {
                                                    // Grouping parens without comma `fromlist=("ab")`
                                                    // evaluates to `"ab"` -> iterates `'a'`, `'b'`!
                                                    let s = &fromlist_paren_lits[0];
                                                    if s.is_empty() {
                                                        dynamic_fromlist = true;
                                                    } else {
                                                        for ch in s.chars() {
                                                            if ch == '*'
                                                                || (!ch.is_alphanumeric() && ch != '_')
                                                            {
                                                                dynamic_fromlist = true;
                                                            } else {
                                                                fromlist_syms.push(ch.to_string());
                                                            }
                                                        }
                                                    }
                                                } else {
                                                    for item in fromlist_paren_lits.drain(..) {
                                                        if !item.is_empty() && item != "*" {
                                                            fromlist_syms.push(item);
                                                        } else {
                                                            dynamic_fromlist = true;
                                                        }
                                                    }
                                                }
                                                fromlist_in_paren = false;
                                                fromlist_paren_lits.clear();
                                            }
                                            pdepth -= 1;
                                            if pdepth == 0 {
                                                break;
                                            }
                                        }
                                        PyToken::LBracket => {
                                            bdepth += 1;
                                            if collecting_fromlist && bdepth > 1 {
                                                dynamic_fromlist = true;
                                            }
                                        }
                                        PyToken::RBracket => {
                                            bdepth = bdepth.saturating_sub(1);
                                        }
                                        PyToken::Comma
                                            if pdepth == 1 && bdepth == 0 && !in_fromlist =>
                                        {
                                            arg_idx += 1;
                                            if arg_idx >= 4 {
                                                // 5th positional arg `level` (e.g. `__import__('helpers', globals(), locals(), ('VALUE',), 2)`):
                                                // relative level cannot be reliably resolved without integer token values -> fail closed!
                                                dynamic_fromlist = true;
                                            }
                                        }
                                        PyToken::Ident("level")
                                            if pdepth == 1
                                                && bdepth == 0
                                                && matches!(
                                                    tokens.get(j + 1),
                                                    Some(PyToken::Assign)
                                                ) =>
                                        {
                                            dynamic_fromlist = true;
                                        }
                                        PyToken::Ident("fromlist")
                                            if pdepth == 1
                                                && bdepth == 0
                                                && matches!(tokens.get(j + 1), Some(PyToken::Assign)) =>
                                        {
                                            in_fromlist = true;
                                            j += 2;
                                            continue;
                                        }
                                        PyToken::Star | PyToken::Other
                                            if pdepth == 1
                                                && bdepth == 0
                                                && !collecting_fromlist =>
                                        {
                                            // Unpacked `*args` / `**kwargs` in `__import__("pkg", *args)`
                                            // tokenize `*` as `PyToken::Star` and may supply `fromlist`
                                            // or relative `level` -> fail closed!
                                            dynamic_fromlist = true;
                                        }
                                        _ if collecting_fromlist => match &tokens[j] {
                                            PyToken::Comma | PyToken::Newline => {
                                                if matches!(tokens[j], PyToken::Comma)
                                                    && fromlist_in_paren
                                                    && pdepth == 2
                                                    && bdepth == 0
                                                {
                                                    fromlist_paren_saw_comma = true;
                                                }
                                                if in_fromlist && pdepth == 1 && bdepth == 0 {
                                                    in_fromlist = false;
                                                    arg_idx += 1;
                                                }
                                            }
                                            PyToken::Ident("None") => {}
                                            PyToken::StringLit(s) => {
                                                let mut joined = s.clone();
                                                loop {
                                                    let mut peek = j + 1;
                                                    while matches!(
                                                        tokens.get(peek),
                                                        Some(PyToken::Newline)
                                                    ) {
                                                        peek += 1;
                                                    }
                                                    if let Some(PyToken::StringLit(np)) =
                                                        tokens.get(peek)
                                                    {
                                                        joined.push_str(np);
                                                        j = peek;
                                                    } else {
                                                        break;
                                                    }
                                                }
                                                if fromlist_in_paren && pdepth == 2 && bdepth == 0 {
                                                    fromlist_paren_lits.push(joined);
                                                } else if pdepth == 1 && bdepth == 0 {
                                                    // Bare string `fromlist="ab"` iterates over
                                                    // individual characters (`pkg.a`, `pkg.b`) in Python!
                                                    if joined.is_empty() {
                                                        dynamic_fromlist = true;
                                                    } else {
                                                        for ch in joined.chars() {
                                                            if ch == '*' || (!ch.is_alphanumeric() && ch != '_') {
                                                                dynamic_fromlist = true;
                                                            } else {
                                                                fromlist_syms.push(ch.to_string());
                                                            }
                                                        }
                                                    }
                                                } else if !joined.is_empty() && joined != "*" {
                                                    fromlist_syms.push(joined);
                                                } else {
                                                    dynamic_fromlist = true;
                                                }
                                            }
                                            _ => {
                                                dynamic_fromlist = true;
                                            }
                                        },
                                        _ => {}
                                    }
                                    j += 1;
                                }
                                if dynamic_fromlist {
                                    defined_symbols.insert("py_dynamic_import".to_string());
                                }
                                imports.insert(ImportSpec {
                                    level: 0,
                                    module: mod_name.clone(),
                                    symbols: fromlist_syms,
                                });
                            } else {
                                imports.insert(ImportSpec {
                                    level: 0,
                                    module: mod_name.clone(),
                                    symbols: Vec::new(),
                                });
                            }
                        } else {
                            defined_symbols.insert("py_dynamic_import".to_string());
                        }
                    } else if !matches!(tokens.get(i + 2), Some(PyToken::RParen)) {
                        defined_symbols.insert("py_dynamic_import".to_string());
                    }
                }
                at_statement_start = false;
                i += 1;
            }
            PyToken::Ident(ident) if at_statement_start => {
                if matches!(tokens.get(i + 1), Some(PyToken::Assign | PyToken::Colon)) {
                    defined_symbols.insert((*ident).to_string());
                }
                at_statement_start = false;
                i += 1;
            }
            _ => {
                at_statement_start = false;
                i += 1;
            }
        }
    }

    for spec in &imports {
        if let Some(last_seg) = spec.module.rsplit('.').next()
            && let Some(stem) = last_seg
                .strip_suffix("_pb2_grpc")
                .or_else(|| last_seg.strip_suffix("_pb2"))
        {
            defined_symbols.insert(format!("proto_use:{stem}"));
        }
        for sym in &spec.symbols {
            if let Some(stem) = sym
                .strip_suffix("_pb2_grpc")
                .or_else(|| sym.strip_suffix("_pb2"))
            {
                defined_symbols.insert(format!("proto_use:{stem}"));
            }
        }
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        fixtures_defined: fixtures_defined.into_iter().collect(),
        fixtures_used: fixtures_used.into_iter().collect(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        markers: markers.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        rust_mods: Vec::new(),
        type_checking_imports: type_checking_imports.into_iter().collect(),
        env_vars_referenced: env_vars.into_iter().collect(),
    }
}


