use super::*;

/// Parses a frontend source file (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.vue`, `.svelte`)
/// to extract ES/CJS/dynamic imports, `import type` exclusions, Barrel re-exports, Vite asset queries,
/// `import.meta.env` / `process.env` keys, React component/hook definitions, and Vitest/Jest/Node test symbols.
pub fn parse_frontend_source(source: &str) -> ParsedPythonFile {
    let tokens = tokenize_frontend(source);
    let mut imports = BTreeSet::new();
    let mut type_checking_imports = BTreeSet::new();
    let mut test_symbols = BTreeSet::new();
    let mut defined_symbols = BTreeSet::new();
    let mut data_refs = BTreeSet::new();
    let mut markers = BTreeSet::new();
    let mut module_markers = BTreeSet::new();
    let mut env_vars = BTreeSet::new();

    for (idx, tok) in tokens.iter().enumerate() {
        if let JsToken::Ident(id) = tok
            && id == "createRequire"
        {
            // Node.js `createRequire(...)` creates an arbitrarily-named `require` function
            // (e.g. `const req = createRequire(import.meta.url); req("node:os").homedir()`) that
            // can dynamically load builtins or local files outside static analysis: fail closed!
            defined_symbols.insert("fe_dynamic_import".to_string());
            defined_symbols.insert("fe_dynamic_env".to_string());
        }
        if let JsToken::StringLit(lit) = tok {
            if lit.trim() == "createRequire" {
                defined_symbols.insert("fe_dynamic_import".to_string());
                defined_symbols.insert("fe_dynamic_env".to_string());
            }
            if matches!(lit.trim(), "module" | "node:module")
                && ((idx > 0
                    && matches!(
                        tokens.get(idx - 1),
                        Some(JsToken::Ident(kw)) if kw == "from" || kw == "import"
                    ))
                    || (idx >= 2
                        && matches!(tokens.get(idx - 1), Some(JsToken::LParen))
                        && matches!(
                            tokens.get(idx - 2),
                            Some(JsToken::Ident(kw))
                                if matches!(
                                    kw.as_str(),
                                    "import"
                                        | "require"
                                        | "importActual"
                                        | "requireActual"
                                        | "getBuiltinModule"
                                )
                        )))
            {
                // Node.js `module` / `node:module` exposes `createRequire`, `Module._load`, and
                // dynamic loader hooks that can bypass static import/builtin tracking: fail closed!
                defined_symbols.insert("fe_dynamic_import".to_string());
                defined_symbols.insert("fe_dynamic_env".to_string());
            }
            if matches!(lit.trim(), "os" | "node:os")
                && ((idx > 0
                    && matches!(
                        tokens.get(idx - 1),
                        Some(JsToken::Ident(kw)) if kw == "from" || kw == "import"
                    ))
                    || (idx >= 2
                        && matches!(tokens.get(idx - 1), Some(JsToken::LParen))
                        && matches!(
                            tokens.get(idx - 2),
                            Some(JsToken::Ident(kw))
                                if matches!(
                                    kw.as_str(),
                                    "import"
                                        | "require"
                                        | "importActual"
                                        | "requireActual"
                                        | "getBuiltinModule"
                                )
                        )))
            {
                // Node.js `os` / `node:os` (`os.tmpdir()`, `os.homedir()`, `os.userInfo()`,
                // `os.hostname()`, including aliases/destructuring/dynamic imports) depends on
                // OS environment variables (`TMPDIR`, `HOME`, `USER`, etc.): fail closed!
                defined_symbols.insert("fe_dynamic_env".to_string());
            }
            if matches!(
                lit.trim(),
                "child_process" | "node:child_process"
            ) && ((idx > 0
                && matches!(
                    tokens.get(idx - 1),
                    Some(JsToken::Ident(kw)) if kw == "from" || kw == "import"
                ))
                || (idx >= 2
                    && matches!(tokens.get(idx - 1), Some(JsToken::LParen))
                    && matches!(
                        tokens.get(idx - 2),
                        Some(JsToken::Ident(kw))
                            if matches!(
                                kw.as_str(),
                                "import"
                                    | "require"
                                    | "importActual"
                                    | "requireActual"
                                    | "getBuiltinModule"
                            )
                    )))
            {
                // Node.js `child_process` (`execSync`, `exec`, `spawn`, `spawnSync`, `execFile`,
                // `fork`, including aliases/destructuring/dynamic import) can read arbitrary
                // external files or env via child processes: fail closed!
                defined_symbols.insert("fe_dynamic_import".to_string());
            }
            if lit.trim().starts_with("virtual:")
                && ((idx > 0
                    && matches!(
                        tokens.get(idx - 1),
                        Some(JsToken::Ident(kw)) if kw == "from" || kw == "import"
                    ))
                    || (idx >= 2
                        && matches!(tokens.get(idx - 1), Some(JsToken::LParen))
                        && matches!(
                            tokens.get(idx - 2),
                            Some(JsToken::Ident(kw))
                                if matches!(
                                    kw.as_str(),
                                    "import"
                                        | "require"
                                        | "mock"
                                        | "doMock"
                                        | "importActual"
                                        | "requireActual"
                                )
                        )))
            {
                // Vite plugin `virtual:*` modules (both static AND dynamic `import("virtual:...")`)
                // may synthesize code from local files outside static imports: fail closed!
                defined_symbols.insert("fe_dynamic_import".to_string());
            }
            if let Some(clean_spec) = clean_frontend_import_specifier(lit) {
                if let Some(clean_ref) = normalize_data_ref(&clean_spec) {
                    data_refs.insert(clean_ref);
                }
                if frontend_specifier_has_raw_query(lit) {
                    defined_symbols.insert(format!("fe_raw_import:{clean_spec}"));
                }
            }
        }
    }

    // Pre-scan default/namespace/CJS `process` aliases AND named imports / destructurings of
    // `env` from `node:process` / `process` (e.g. `import proc from 'node:process'`,
    // `import * as proc from 'node:process'`, `import { env as procEnv } from 'process'`,
    // `const { env } = proc`) so `proc.env.FLAG`, `env.FEATURE`, `{ ...env }`, and dynamic
    // `env[k]` are all tracked accurately!
    let mut node_process_aliases: BTreeSet<String> = BTreeSet::from(["process".to_string()]);
    let mut node_env_aliases: BTreeSet<String> = BTreeSet::new();
    let mut tracked_process_destructure_rbraces: BTreeSet<usize> = BTreeSet::new();
    {
        let mut ti = 0usize;
        while ti < tokens.len() {
            if let JsToken::Ident(kw) = &tokens[ti]
                && kw == "export"
            {
                // Any re-export from `node:process` / `process` (`export { env as settings } from
                // 'node:process'`, `export { default as proc } from 'node:process'`,
                // `export * [as proc] from 'node:process'`) exposes process/env/chdir under an
                // alias to downstream modules -> fail closed!
                let mut ek = ti + 1;
                if matches!(tokens.get(ek), Some(JsToken::Star)) {
                    ek += 1;
                    if matches!(tokens.get(ek), Some(JsToken::Ident(a)) if a == "as") {
                        ek += 2;
                    }
                    if matches!(tokens.get(ek), Some(JsToken::Ident(f)) if f == "from")
                        && let Some(JsToken::StringLit(mod_s)) = tokens.get(ek + 1)
                        && matches!(mod_s.trim(), "node:process" | "process")
                    {
                        defined_symbols.insert("fe_dynamic_env".to_string());
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    }
                } else if matches!(tokens.get(ek), Some(JsToken::LBrace)) {
                    ek += 1;
                    while ek < tokens.len() && !matches!(tokens[ek], JsToken::RBrace) {
                        ek += 1;
                    }
                    if matches!(tokens.get(ek + 1), Some(JsToken::Ident(f)) if f == "from")
                        && let Some(JsToken::StringLit(mod_s)) = tokens.get(ek + 2)
                        && matches!(mod_s.trim(), "node:process" | "process")
                    {
                        defined_symbols.insert("fe_dynamic_env".to_string());
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    }
                }
            }
            if let JsToken::Ident(kw) = &tokens[ti]
                && kw == "import"
            {
                // Default import: `import proc from 'node:process'`
                // Namespace import: `import * as proc from 'node:process'`
                // Combined default + namespace: `import proc, * as ns from 'node:process'`
                if let Some(JsToken::Ident(al)) = tokens.get(ti + 1)
                    && matches!(tokens.get(ti + 2), Some(JsToken::Ident(f)) if f == "from")
                    && let Some(JsToken::StringLit(mod_s)) = tokens.get(ti + 3)
                    && matches!(mod_s.trim(), "node:process" | "process")
                {
                    node_process_aliases.insert(al.clone());
                } else if matches!(tokens.get(ti + 1), Some(JsToken::Star))
                    && matches!(tokens.get(ti + 2), Some(JsToken::Ident(a)) if a == "as")
                    && let Some(JsToken::Ident(al)) = tokens.get(ti + 3)
                    && matches!(tokens.get(ti + 4), Some(JsToken::Ident(f)) if f == "from")
                    && let Some(JsToken::StringLit(mod_s)) = tokens.get(ti + 5)
                    && matches!(mod_s.trim(), "node:process" | "process")
                {
                    node_process_aliases.insert(al.clone());
                } else if let Some(JsToken::Ident(def_al)) = tokens.get(ti + 1)
                    && matches!(tokens.get(ti + 2), Some(JsToken::Comma))
                    && matches!(tokens.get(ti + 3), Some(JsToken::Star))
                    && matches!(tokens.get(ti + 4), Some(JsToken::Ident(a)) if a == "as")
                    && let Some(JsToken::Ident(ns_al)) = tokens.get(ti + 5)
                    && matches!(tokens.get(ti + 6), Some(JsToken::Ident(f)) if f == "from")
                    && let Some(JsToken::StringLit(mod_s)) = tokens.get(ti + 7)
                    && matches!(mod_s.trim(), "node:process" | "process")
                {
                    node_process_aliases.insert(def_al.clone());
                    node_process_aliases.insert(ns_al.clone());
                }
                let (default_proc_al, brace_start) =
                    if matches!(tokens.get(ti + 1), Some(JsToken::LBrace)) {
                        (None, Some(ti + 2))
                    } else if let Some(JsToken::Ident(def_al)) = tokens.get(ti + 1)
                        && matches!(tokens.get(ti + 2), Some(JsToken::Comma))
                        && matches!(tokens.get(ti + 3), Some(JsToken::LBrace))
                    {
                        (Some(def_al.clone()), Some(ti + 4))
                    } else {
                        (None, None)
                    };
                if let Some(mut k) = brace_start {
                    let mut found_aliases: Vec<String> = Vec::new();
                    let mut found_default_procs: Vec<String> = Vec::new();
                    while k < tokens.len() && !matches!(tokens[k], JsToken::RBrace) {
                        if let JsToken::Ident(sym) = &tokens[k] {
                            if matches!(sym.as_str(), "chdir" | "getBuiltinModule") {
                                // `import { chdir } from 'node:process'` / `import { getBuiltinModule }`:
                                // mutating cwd or dynamically loading builtins fails closed!
                                defined_symbols.insert("fe_dynamic_import".to_string());
                            }
                            if sym == "env" {
                                if matches!(tokens.get(k + 1), Some(JsToken::Ident(as_kw)) if as_kw == "as")
                                    && let Some(JsToken::Ident(al)) = tokens.get(k + 2)
                                {
                                    found_aliases.push(al.clone());
                                    k += 3;
                                    continue;
                                }
                                found_aliases.push("env".to_string());
                            } else if sym == "default"
                                && matches!(tokens.get(k + 1), Some(JsToken::Ident(as_kw)) if as_kw == "as")
                                && let Some(JsToken::Ident(al)) = tokens.get(k + 2)
                            {
                                found_default_procs.push(al.clone());
                                k += 3;
                                continue;
                            }
                        }
                        k += 1;
                    }
                    if matches!(tokens.get(k + 1), Some(JsToken::Ident(f)) if f == "from")
                        && let Some(JsToken::StringLit(mod_s)) = tokens.get(k + 2)
                        && matches!(mod_s.trim(), "node:process" | "process")
                    {
                        if let Some(def_al) = default_proc_al {
                            node_process_aliases.insert(def_al);
                        }
                        for def_p in found_default_procs {
                            node_process_aliases.insert(def_p);
                        }
                        for alias in found_aliases {
                            node_env_aliases.insert(alias);
                        }
                    }
                }
            }
            // CJS `const proc = require('node:process')`
            if let JsToken::Ident(al) = &tokens[ti]
                && matches!(tokens.get(ti + 1), Some(JsToken::Other))
                && matches!(tokens.get(ti + 2), Some(JsToken::Ident(r)) if r == "require")
                && matches!(tokens.get(ti + 3), Some(JsToken::LParen))
                && let Some(JsToken::StringLit(mod_s)) = tokens.get(ti + 4)
                && matches!(mod_s.trim(), "node:process" | "process")
                && matches!(tokens.get(ti + 5), Some(JsToken::RParen))
            {
                if matches!(tokens.get(ti + 6), Some(JsToken::Dot))
                    && matches!(tokens.get(ti + 7), Some(JsToken::Ident(e)) if e == "env")
                {
                    node_env_aliases.insert(al.clone());
                } else {
                    node_process_aliases.insert(al.clone());
                }
            }
            // Local variable aliasing of `process`, `globalThis.process`, or `proc.env`:
            // `const proc = process;`, `const proc = globalThis.process;`, `const myEnv = proc.env;`
            if let JsToken::Ident(al) = &tokens[ti]
                && matches!(tokens.get(ti + 1), Some(JsToken::Other))
                && let Some(JsToken::Ident(rhs)) = tokens.get(ti + 2)
            {
                if node_process_aliases.contains(rhs) {
                    if matches!(tokens.get(ti + 3), Some(JsToken::Dot))
                        && matches!(tokens.get(ti + 4), Some(JsToken::Ident(e)) if e == "env")
                        && !matches!(tokens.get(ti + 5), Some(JsToken::Dot | JsToken::LBracket))
                    {
                        node_env_aliases.insert(al.clone());
                    } else if !matches!(tokens.get(ti + 3), Some(JsToken::Dot | JsToken::LBracket)) {
                        node_process_aliases.insert(al.clone());
                    }
                } else if matches!(rhs.as_str(), "globalThis" | "global" | "self" | "window")
                    && matches!(tokens.get(ti + 3), Some(JsToken::Dot))
                    && matches!(tokens.get(ti + 4), Some(JsToken::Ident(p)) if p == "process")
                {
                    if matches!(tokens.get(ti + 5), Some(JsToken::Dot))
                        && matches!(tokens.get(ti + 6), Some(JsToken::Ident(e)) if e == "env")
                        && !matches!(tokens.get(ti + 7), Some(JsToken::Dot | JsToken::LBracket))
                    {
                        node_env_aliases.insert(al.clone());
                    } else if !matches!(tokens.get(ti + 5), Some(JsToken::Dot | JsToken::LBracket)) {
                        node_process_aliases.insert(al.clone());
                    }
                }
            }
            if let JsToken::LBrace = &tokens[ti] {
                let mut k = ti + 1;
                let mut brace_depth = 1usize;
                let mut has_nested_or_complex = false;
                let mut found_aliases: Vec<String> = Vec::new();
                while k < tokens.len() && brace_depth > 0 {
                    match &tokens[k] {
                        JsToken::LBrace => {
                            brace_depth += 1;
                            has_nested_or_complex = true;
                        }
                        JsToken::RBrace => {
                            brace_depth -= 1;
                            if brace_depth == 0 {
                                break;
                            }
                        }
                        JsToken::LBracket => {
                            has_nested_or_complex = true;
                        }
                        JsToken::Dot
                            if matches!(tokens.get(k + 1), Some(JsToken::Dot))
                                && matches!(tokens.get(k + 2), Some(JsToken::Dot)) =>
                        {
                            // Rest destructuring (`const { ...rest } = process`) captures `env`.
                            has_nested_or_complex = true;
                        }
                        JsToken::Ident(sym) if brace_depth == 1 && sym == "env" => {
                            if matches!(tokens.get(k + 1), Some(JsToken::Colon)) {
                                if let Some(JsToken::Ident(al)) = tokens.get(k + 2)
                                    && matches!(
                                        tokens.get(k + 3),
                                        Some(JsToken::Comma | JsToken::RBrace)
                                    )
                                {
                                    found_aliases.push(al.clone());
                                    k += 3;
                                    continue;
                                } else {
                                    // Nested destructuring (`const { env: { NODE_ENV } } = ...`)
                                    // or alias with default (`const { env: e = {} } = ...`): fail closed!
                                    has_nested_or_complex = true;
                                }
                            } else if matches!(tokens.get(k + 1), Some(JsToken::Other)) {
                                // Default-value destructuring (`const { env = defaults.value } = process`):
                                // fail closed instead of mistaking `defaults` for an alias!
                                has_nested_or_complex = true;
                            } else {
                                found_aliases.push("env".to_string());
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                let is_process_rhs = brace_depth == 0
                    && matches!(tokens.get(k + 1), Some(JsToken::Other))
                    && (matches!(tokens.get(k + 2), Some(JsToken::Ident(p)) if node_process_aliases.contains(p))
                        || (matches!(tokens.get(k + 2), Some(JsToken::Ident(r)) if r == "require")
                            && matches!(tokens.get(k + 3), Some(JsToken::LParen))
                            && matches!(tokens.get(k + 4), Some(JsToken::StringLit(ms)) if matches!(ms.trim(), "node:process" | "process"))));
                if is_process_rhs {
                    if has_nested_or_complex {
                        defined_symbols.insert("fe_dynamic_env".to_string());
                    } else {
                        for alias in found_aliases {
                            node_env_aliases.insert(alias);
                        }
                        tracked_process_destructure_rbraces.insert(k);
                    }
                }
            }
            ti += 1;
        }
    }

    // Also pre-scan Node.js `fs` / `node:fs` / `fs/promises` / `node:fs/promises` imports &
    // destructurings (`import { readFileSync as load } from 'node:fs'`,
    // `const { readFileSync: load } = require('node:fs')`, `import * as myfs from 'node:fs'`,
    // `import myfs from 'fs'`) so aliased fs calls and indirect fs function values are tracked!
    let is_node_fs_mod = |m: &str| {
        matches!(
            m.trim(),
            "fs" | "node:fs" | "fs/promises" | "node:fs/promises"
        )
    };
    // Unambiguously Node.js filesystem metadata/existence/directory-listing APIs (including
    // `existsSync`, which returns true for empty directories that have no file-byte hash node).
    let is_node_fs_meta_fn = |name: &str| {
        matches!(
            name,
            "readdirSync"
                | "readdir"
                | "statSync"
                | "stat"
                | "lstatSync"
                | "lstat"
                | "fstatSync"
                | "accessSync"
                | "access"
                | "existsSync"
                | "realpathSync"
                | "readlinkSync"
                | "statfsSync"
                | "opendirSync"
                | "globSync"
                | "copyFileSync"
                | "copyFile"
                | "cpSync"
                | "linkSync"
        )
    };
    // Qualified-or-imported-only Node.js metadata/existence/copy APIs whose bare names (`exists`,
    // `fstat`, `realpath`, `readlink`, `statfs`, `opendir`, `glob`, `cp`, `link`) may collide with non-fs helpers if unqualified.
    let is_node_fs_qualified_meta_fn = |name: &str| {
        matches!(
            name,
            "exists" | "fstat" | "realpath" | "readlink" | "statfs" | "opendir" | "glob" | "cp" | "link"
        )
    };
    let is_node_fs_read_fn = |name: &str| {
        matches!(
            name,
            "readFileSync" | "readFile" | "createReadStream" | "openSync"
        ) || is_node_fs_meta_fn(name)
    };
    let is_node_fs_importable_fn =
        |name: &str| is_node_fs_read_fn(name) || is_node_fs_qualified_meta_fn(name) || name == "open";
    let mut node_fs_fn_aliases: BTreeSet<String> = BTreeSet::new();
    let mut node_fs_meta_aliases: BTreeSet<String> = BTreeSet::new();
    let mut imported_node_fs_mod_aliases: BTreeSet<String> = BTreeSet::new();
    let mut node_fs_mod_aliases: BTreeSet<String> = BTreeSet::from([
        "fs".to_string(),
        "fsPromises".to_string(),
        "promises".to_string(),
    ]);
    {
        let mut ti = 0usize;
        while ti < tokens.len() {
            // 1. ES named import / combined default+named import / re-export:
            //    `import { readFileSync as load, promises as fsp } from 'node:fs'`
            //    `import fs, { promises as fsp } from 'node:fs'`
            if let JsToken::Ident(kw) = &tokens[ti]
                && (kw == "import" || kw == "export")
            {
                let (default_alias, brace_start) = if matches!(tokens.get(ti + 1), Some(JsToken::LBrace)) {
                    (None, Some(ti + 2))
                } else if kw == "import"
                    && let Some(JsToken::Ident(def_al)) = tokens.get(ti + 1)
                    && matches!(tokens.get(ti + 2), Some(JsToken::Comma))
                    && matches!(tokens.get(ti + 3), Some(JsToken::LBrace))
                {
                    (Some(def_al.clone()), Some(ti + 4))
                } else {
                    (None, None)
                };
                if let Some(mut k) = brace_start {
                    let is_export = kw == "export";
                    let mut local_aliases: Vec<(String, bool)> = Vec::new();
                    let mut local_mod_aliases: Vec<String> = Vec::new();
                    if let Some(def_al) = default_alias {
                        local_mod_aliases.push(def_al);
                    }
                    while k < tokens.len() && !matches!(tokens[k], JsToken::RBrace) {
                        if let JsToken::Ident(sym) = &tokens[k] {
                            if sym == "promises" || (is_export && sym == "default") {
                                if matches!(tokens.get(k + 1), Some(JsToken::Ident(as_kw)) if as_kw == "as")
                                    && let Some(JsToken::Ident(al)) = tokens.get(k + 2)
                                {
                                    local_mod_aliases.push(al.clone());
                                    k += 3;
                                    continue;
                                }
                                local_mod_aliases.push(sym.clone());
                            } else if is_node_fs_importable_fn(sym) {
                                let is_meta = is_node_fs_meta_fn(sym)
                                    || is_node_fs_qualified_meta_fn(sym);
                                if matches!(tokens.get(k + 1), Some(JsToken::Ident(as_kw)) if as_kw == "as")
                                    && let Some(JsToken::Ident(al)) = tokens.get(k + 2)
                                {
                                    local_aliases.push((al.clone(), is_meta));
                                    k += 3;
                                    continue;
                                }
                                local_aliases.push((sym.clone(), is_meta));
                            }
                        }
                        k += 1;
                    }
                    if (!local_aliases.is_empty() || !local_mod_aliases.is_empty())
                        && matches!(tokens.get(k + 1), Some(JsToken::Ident(f)) if f == "from")
                        && let Some(JsToken::StringLit(mod_s)) = tokens.get(k + 2)
                        && is_node_fs_mod(mod_s)
                    {
                        if is_export {
                            defined_symbols.insert("fe_dynamic_import".to_string());
                        } else {
                            for (al, is_meta) in local_aliases {
                                if is_meta {
                                    node_fs_meta_aliases.insert(al.clone());
                                }
                                node_fs_fn_aliases.insert(al);
                            }
                            for mal in local_mod_aliases {
                                imported_node_fs_mod_aliases.insert(mal.clone());
                                node_fs_mod_aliases.insert(mal);
                            }
                        }
                    }
                } else if kw == "export" && matches!(tokens.get(ti + 1), Some(JsToken::Star)) {
                    // `export * from 'node:fs'` or `export * as disk from 'node:fs/promises'`
                    let mut ek = ti + 2;
                    if matches!(tokens.get(ek), Some(JsToken::Ident(a)) if a == "as") {
                        ek += 2;
                    }
                    if matches!(tokens.get(ek), Some(JsToken::Ident(f)) if f == "from")
                        && let Some(JsToken::StringLit(mod_s)) = tokens.get(ek + 1)
                        && is_node_fs_mod(mod_s)
                    {
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    }
                }
            }
            // 2. ES default / namespace import: `import myfs from 'node:fs'` or `import * as myfs from 'node:fs'`
            if let JsToken::Ident(kw) = &tokens[ti]
                && kw == "import"
            {
                if let Some(JsToken::Ident(al)) = tokens.get(ti + 1)
                    && matches!(tokens.get(ti + 2), Some(JsToken::Ident(f)) if f == "from")
                    && let Some(JsToken::StringLit(mod_s)) = tokens.get(ti + 3)
                    && is_node_fs_mod(mod_s)
                {
                    imported_node_fs_mod_aliases.insert(al.clone());
                    node_fs_mod_aliases.insert(al.clone());
                } else if matches!(tokens.get(ti + 1), Some(JsToken::Star))
                    && matches!(tokens.get(ti + 2), Some(JsToken::Ident(a)) if a == "as")
                    && let Some(JsToken::Ident(al)) = tokens.get(ti + 3)
                    && matches!(tokens.get(ti + 4), Some(JsToken::Ident(f)) if f == "from")
                    && let Some(JsToken::StringLit(mod_s)) = tokens.get(ti + 5)
                    && is_node_fs_mod(mod_s)
                {
                    imported_node_fs_mod_aliases.insert(al.clone());
                    node_fs_mod_aliases.insert(al.clone());
                }
            }
            // 3. CJS `const myfs = require('node:fs')`, `const fsp = require('node:fs').promises`,
            //    and indirect `const load = require('node:fs').readFileSync` / `.promises.readFile`
            if let JsToken::Ident(al) = &tokens[ti]
                && matches!(tokens.get(ti + 1), Some(JsToken::Other))
                && matches!(tokens.get(ti + 2), Some(JsToken::Ident(r)) if r == "require")
                && matches!(tokens.get(ti + 3), Some(JsToken::LParen))
                && let Some(JsToken::StringLit(mod_s)) = tokens.get(ti + 4)
                && is_node_fs_mod(mod_s)
                && matches!(tokens.get(ti + 5), Some(JsToken::RParen))
            {
                if matches!(tokens.get(ti + 6), Some(JsToken::Dot))
                    && let Some(JsToken::Ident(prop)) = tokens.get(ti + 7)
                {
                    if prop == "promises" {
                        if matches!(tokens.get(ti + 8), Some(JsToken::Dot))
                            && let Some(JsToken::Ident(pfn)) = tokens.get(ti + 9)
                            && is_node_fs_importable_fn(pfn)
                        {
                            if is_node_fs_meta_fn(pfn) || is_node_fs_qualified_meta_fn(pfn) {
                                node_fs_meta_aliases.insert(al.clone());
                            }
                            node_fs_fn_aliases.insert(al.clone());
                        } else {
                            imported_node_fs_mod_aliases.insert(al.clone());
                            node_fs_mod_aliases.insert(al.clone());
                        }
                    } else if is_node_fs_importable_fn(prop) {
                        if is_node_fs_meta_fn(prop) || is_node_fs_qualified_meta_fn(prop) {
                            node_fs_meta_aliases.insert(al.clone());
                        }
                        node_fs_fn_aliases.insert(al.clone());
                    }
                } else {
                    imported_node_fs_mod_aliases.insert(al.clone());
                    node_fs_mod_aliases.insert(al.clone());
                }
            }
            // 4. CJS / local destructuring: `const { readFileSync: load, promises: fsp } = require('node:fs')` or `= myfs`
            //    Tracks nested braces (`const { promises: { readFile: load } } = require('node:fs')`),
            //    computed keys, defaults, and rest patterns, failing closed when nested/complex!
            if let JsToken::LBrace = &tokens[ti] {
                let mut k = ti + 1;
                let mut brace_depth = 1usize;
                let mut has_nested_or_complex = false;
                let mut local_aliases: Vec<(String, bool)> = Vec::new();
                let mut local_mod_aliases: Vec<String> = Vec::new();
                while k < tokens.len() && brace_depth > 0 {
                    match &tokens[k] {
                        JsToken::LBrace => {
                            brace_depth += 1;
                            has_nested_or_complex = true;
                        }
                        JsToken::RBrace => {
                            brace_depth -= 1;
                            if brace_depth == 0 {
                                break;
                            }
                        }
                        JsToken::LBracket => {
                            has_nested_or_complex = true;
                        }
                        JsToken::Dot
                            if matches!(tokens.get(k + 1), Some(JsToken::Dot))
                                && matches!(tokens.get(k + 2), Some(JsToken::Dot)) =>
                        {
                            has_nested_or_complex = true;
                        }
                        JsToken::Ident(sym) if brace_depth == 1 => {
                            if sym == "promises" {
                                if matches!(tokens.get(k + 1), Some(JsToken::Colon)) {
                                    if let Some(JsToken::Ident(al)) = tokens.get(k + 2)
                                        && matches!(
                                            tokens.get(k + 3),
                                            Some(JsToken::Comma | JsToken::RBrace)
                                        )
                                    {
                                        local_mod_aliases.push(al.clone());
                                        k += 3;
                                        continue;
                                    } else {
                                        has_nested_or_complex = true;
                                    }
                                } else if matches!(tokens.get(k + 1), Some(JsToken::Other)) {
                                    has_nested_or_complex = true;
                                } else {
                                    local_mod_aliases.push("promises".to_string());
                                }
                            } else if is_node_fs_importable_fn(sym) {
                                let is_meta =
                                    is_node_fs_meta_fn(sym) || is_node_fs_qualified_meta_fn(sym);
                                if matches!(tokens.get(k + 1), Some(JsToken::Colon)) {
                                    if let Some(JsToken::Ident(al)) = tokens.get(k + 2)
                                        && matches!(
                                            tokens.get(k + 3),
                                            Some(JsToken::Comma | JsToken::RBrace)
                                        )
                                    {
                                        local_aliases.push((al.clone(), is_meta));
                                        k += 3;
                                        continue;
                                    } else {
                                        has_nested_or_complex = true;
                                    }
                                } else if matches!(tokens.get(k + 1), Some(JsToken::Other)) {
                                    has_nested_or_complex = true;
                                } else {
                                    local_aliases.push((sym.clone(), is_meta));
                                }
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                let is_fs_rhs = brace_depth == 0
                    && matches!(tokens.get(k + 1), Some(JsToken::Other))
                    && (matches!(tokens.get(k + 2), Some(JsToken::Ident(m)) if node_fs_mod_aliases.contains(m))
                        || (matches!(tokens.get(k + 2), Some(JsToken::Ident(r)) if r == "require")
                            && matches!(tokens.get(k + 3), Some(JsToken::LParen))
                            && matches!(tokens.get(k + 4), Some(JsToken::StringLit(ms)) if is_node_fs_mod(ms))));
                if is_fs_rhs {
                    if has_nested_or_complex {
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    } else {
                        for (al, is_meta) in local_aliases {
                            if is_meta {
                                node_fs_meta_aliases.insert(al.clone());
                            }
                            node_fs_fn_aliases.insert(al);
                        }
                        for mal in local_mod_aliases {
                            imported_node_fs_mod_aliases.insert(mal.clone());
                            node_fs_mod_aliases.insert(mal);
                        }
                    }
                }
            }
            ti += 1;
        }
    }

    let n = tokens.len();
    let mut i = 0usize;

    while i < n {
        match &tokens[i] {
            JsToken::Ident(kw) if kw == "import" => {
                // 1. `import.meta.env.VITE_...` or `import.meta.vitest`
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 2), Some(JsToken::Ident(m)) if m == "meta")
                    && matches!(tokens.get(i + 3), Some(JsToken::Dot))
                {
                    if let Some(JsToken::Ident(prop)) = tokens.get(i + 4) {
                        if prop == "vitest" {
                            markers.insert("in_source_vitest".to_string());
                        } else if prop == "glob"
                            && matches!(tokens.get(i + 5), Some(JsToken::LParen))
                        {
                            let mut j = i + 6;
                            let mut depth = 1usize;
                            let mut glob_patterns: Vec<String> = Vec::new();
                            let mut is_raw_glob = false;
                            let mut is_case_insensitive = false;
                            let mut glob_base: Option<String> = None;
                            let mut glob_dynamic_opt = false;
                            let mut in_options_obj = false;
                            while j < n && depth > 0 {
                                match &tokens[j] {
                                    JsToken::LParen => depth += 1,
                                    JsToken::RParen => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                    }
                                    JsToken::LBrace if depth == 1 => {
                                        in_options_obj = true;
                                    }
                                    JsToken::Ident(k) | JsToken::StringLit(k)
                                        if in_options_obj
                                            && k == "base"
                                            && matches!(tokens.get(j + 1), Some(JsToken::Colon)) =>
                                    {
                                        if let Some(JsToken::StringLit(val)) = tokens.get(j + 2)
                                            && !val.trim().is_empty()
                                        {
                                            glob_base = Some(val.trim().trim_end_matches('/').to_string());
                                        } else {
                                            glob_dynamic_opt = true;
                                        }
                                    }
                                    JsToken::Ident(k) | JsToken::StringLit(k)
                                        if in_options_obj
                                            && k == "caseSensitive"
                                            && matches!(tokens.get(j + 1), Some(JsToken::Colon)) =>
                                    {
                                        if !matches!(tokens.get(j + 2), Some(JsToken::Ident(v)) if v == "true") {
                                            is_case_insensitive = true;
                                        }
                                    }
                                    JsToken::Ident(k) | JsToken::StringLit(k)
                                        if in_options_obj
                                            && (k == "query" || k == "as")
                                            && matches!(tokens.get(j + 1), Some(JsToken::Colon)) =>
                                    {
                                        if let Some(JsToken::StringLit(val)) = tokens.get(j + 2) {
                                            let vl = val.trim();
                                            if vl == "raw"
                                                || vl == "url"
                                                || vl == "inline"
                                                || frontend_specifier_has_raw_query(vl)
                                            {
                                                is_raw_glob = true;
                                            }
                                        } else if k == "query"
                                            && matches!(tokens.get(j + 2), Some(JsToken::LBrace))
                                        {
                                            // Object-form Vite glob query: `{ query: { raw: '' } }`
                                            // or `{ query: { url: true } }`: scan nested `{ ... }`
                                            // for raw/inline/url keys (or mark raw conservatively).
                                            let mut qj = j + 3;
                                            let mut qdepth = 1usize;
                                            while qj < n && qdepth > 0 {
                                                match &tokens[qj] {
                                                    JsToken::LBrace => qdepth += 1,
                                                    JsToken::RBrace => qdepth -= 1,
                                                    JsToken::Ident(qk) | JsToken::StringLit(qk)
                                                        if qdepth == 1
                                                            && matches!(
                                                                qk.as_str(),
                                                                "raw" | "url" | "inline"
                                                            ) =>
                                                    {
                                                        is_raw_glob = true;
                                                    }
                                                    _ => {}
                                                }
                                                qj += 1;
                                            }
                                        }
                                    }
                                    JsToken::StringLit(pat) if !in_options_obj => {
                                        let trimmed = pat.trim();
                                        if !trimmed.is_empty() {
                                            glob_patterns.push(trimmed.to_string());
                                        }
                                    }
                                    _ => {}
                                }
                                j += 1;
                            }
                            if glob_dynamic_opt {
                                defined_symbols.insert("fe_dynamic_import".to_string());
                            }
                            let glob_pfx = if is_case_insensitive { "glob_ci:" } else { "glob:" };
                            for raw_pat in glob_patterns {
                                // `import.meta.glob(...)` dynamically enumerates matching files on
                                // disk (including gitignored/generated files omitted from default
                                // inventory and newly created files): keep the glob edge for
                                // `affected_tests` while marking the consumer uncacheable!
                                defined_symbols.insert("fe_dynamic_import".to_string());
                                let eff_pat = if let Some(ref b) = glob_base
                                    && (raw_pat.starts_with("./") || raw_pat.starts_with("../"))
                                {
                                    let stripped = raw_pat.trim_start_matches("./");
                                    format!("{b}/{stripped}")
                                } else {
                                    raw_pat
                                };
                                imports.insert(ImportSpec {
                                    level: 0,
                                    module: format!("{glob_pfx}{eff_pat}"),
                                    symbols: vec!["*".to_string()],
                                });
                                if is_raw_glob {
                                    defined_symbols.insert(format!("fe_raw_import:{glob_pfx}{eff_pat}"));
                                }
                            }
                            i = (j + 1).min(n);
                            continue;
                        } else if prop == "env" {
                            if matches!(tokens.get(i + 5), Some(JsToken::Dot))
                                && let Some(JsToken::Ident(env_key)) = tokens.get(i + 6)
                                && is_valid_env_var_name(env_key)
                            {
                                env_vars.insert(env_key.clone());
                                i += 7;
                                continue;
                            }
                            if matches!(tokens.get(i + 5), Some(JsToken::LBracket)) {
                                if let Some(JsToken::StringLit(env_key)) = tokens.get(i + 6)
                                    && matches!(tokens.get(i + 7), Some(JsToken::RBracket))
                                    && !env_key.is_empty()
                                    && !env_key.contains(['=', '\0'])
                                {
                                    env_vars.insert(env_key.clone());
                                    i += 8;
                                    continue;
                                }
                                defined_symbols.insert("fe_dynamic_env".to_string());
                                i += 6;
                                continue;
                            }
                            // Direct `import.meta.env` value (e.g. `const { MODE } = import.meta.env`):
                            defined_symbols.insert("fe_dynamic_env".to_string());
                            i += 5;
                            continue;
                        }
                    }
                    i += 4;
                    continue;
                }
                // 2. Dynamic `import('...')` vs computed `import(expr)`
                if matches!(tokens.get(i + 1), Some(JsToken::LParen)) {
                    if let Some(JsToken::StringLit(spec)) = tokens.get(i + 2)
                        && matches!(
                            tokens.get(i + 3),
                            Some(JsToken::RParen | JsToken::Comma)
                        )
                    {
                        // Dynamic `await import('node:fs/promises')` or `import('node:process')`
                        // can bind destructured fs/env aliases dynamically: fail closed!
                        if is_node_fs_mod(spec) {
                            defined_symbols.insert("fe_dynamic_import".to_string());
                        }
                        if matches!(spec.trim(), "node:process" | "process") {
                            defined_symbols.insert("fe_dynamic_env".to_string());
                        }
                        if let Some(clean) = clean_frontend_import_specifier(spec) {
                            imports.insert(ImportSpec {
                                level: 0,
                                module: clean,
                                symbols: Vec::new(),
                            });
                        }
                    } else {
                        // Computed dynamic import (`import('./plugins/' + name + '.js')`):
                        // fail closed instead of silently missing the target module.
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    }
                    i += 2;
                    continue;
                }
                // 3. Static `import ...`
                let (parsed_spec, is_type_only, next_i) = parse_js_import_clause(&tokens, i + 1);
                if let Some(spec) = parsed_spec {
                    if spec.module == "@playwright/test" || spec.module.starts_with("@playwright/") {
                        markers.insert("playwright".to_string());
                        module_markers.insert("e2e".to_string());
                    } else if spec.module == "@storybook/test"
                        || spec.module.starts_with("@storybook/testing-")
                    {
                        markers.insert("storybook_play".to_string());
                    } else if spec.module.contains("msw") {
                        markers.insert("msw".to_string());
                    }
                    for sym in &spec.symbols {
                        defined_symbols.insert(format!("wasm_use:{sym}"));
                    }
                    if let Some(proto_stem) = spec
                        .module
                        .rsplit('/')
                        .next()
                        .and_then(|s| s.strip_suffix("_pb").or_else(|| s.strip_suffix(".pb")))
                    {
                        defined_symbols.insert(format!("proto_use:{proto_stem}"));
                    }
                    if is_type_only {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
                }
                i = next_i;
            }
            JsToken::Ident(kw) if kw == "fetch" => {
                if matches!(tokens.get(i + 1), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(url)) = tokens.get(i + 2)
                    && url.trim().starts_with("/api/")
                {
                    let route = url.trim().split('?').next().unwrap_or(url.trim());
                    defined_symbols.insert(format!("api_call:{route}"));
                    i += 3;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "axios" => {
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 2), Some(JsToken::Ident(_)))
                    && matches!(tokens.get(i + 3), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(url)) = tokens.get(i + 4)
                    && url.trim().starts_with("/api/")
                {
                    let route = url.trim().split('?').next().unwrap_or(url.trim());
                    defined_symbols.insert(format!("api_call:{route}"));
                    i += 5;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "useContext" => {
                if matches!(tokens.get(i + 1), Some(JsToken::LParen))
                    && let Some(JsToken::Ident(ctx_name)) = tokens.get(i + 2)
                {
                    defined_symbols.insert(format!("ctx_use:{ctx_name}"));
                    i += 3;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw)
                if matches!(kw.as_str(), "beforeAll" | "afterAll" | "before" | "after")
                    && (matches!(tokens.get(i + 1), Some(JsToken::LParen))
                        || matches!(tokens.get(i + 1), Some(JsToken::Ident(a)) if a == "as")
                        || matches!(tokens.get(i + 1), Some(JsToken::Colon))
                            && i > 0
                            && matches!(tokens.get(i - 1), Some(JsToken::LBrace | JsToken::Comma))) =>
            {
                // Shared suite-level setup/teardown hooks (including aliased imports such as
                // `import { beforeAll as setup }` or `const { before: init } = ...`) mean tests
                // inside the file may rely on shared state; disable sub-file node replay (`serial`).
                markers.insert("serial".to_string());
                i += 1;
            }
            JsToken::Ident(kw) if kw == "cy" => {
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && let Some(JsToken::Ident(m)) = tokens.get(i + 2)
                    && matches!(m.as_str(), "visit" | "mount" | "get" | "intercept")
                {
                    markers.insert("cypress".to_string());
                    module_markers.insert("e2e".to_string());
                }
                i += 1;
            }
            JsToken::Ident(kw)
                if node_fs_mod_aliases.contains(kw.as_str())
                    && (matches!(tokens.get(i + 1), Some(JsToken::LBracket))
                        || (matches!(tokens.get(i + 1), Some(JsToken::Other))
                            && matches!(tokens.get(i + 2), Some(JsToken::Dot))
                            && matches!(tokens.get(i + 3), Some(JsToken::LBracket)))) =>
            {
                // Computed/bracket Node `fs` member access (`fs["existsSync"]("ready")`,
                // `fs[method](p)`, `fsp?.["readFile"](p)`): fail closed!
                defined_symbols.insert("fe_dynamic_import".to_string());
                i += 1;
            }
            // Detect `process.chdir(...)` / `<proc_alias>.chdir(...)` / `const { chdir } = process`
            // and `process.getBuiltinModule(...)` independently of the Node `fs` arm so cwd
            // mutations and dynamic builtin module loads always fail closed!
            JsToken::Ident(kw)
                if (kw == "chdir" || kw == "getBuiltinModule")
                    && ((i >= 2
                        && matches!(tokens.get(i - 1), Some(JsToken::Dot))
                        && matches!(tokens.get(i - 2), Some(JsToken::Ident(p)) if node_process_aliases.contains(p)))
                        || (i >= 3
                            && matches!(tokens.get(i - 1), Some(JsToken::Dot))
                            && matches!(tokens.get(i - 2), Some(JsToken::Other))
                            && matches!(tokens.get(i - 3), Some(JsToken::Ident(p)) if node_process_aliases.contains(p)))
                        || (i >= 1
                            && matches!(tokens.get(i - 1), Some(JsToken::LBrace | JsToken::Comma)))) =>
            {
                defined_symbols.insert("fe_dynamic_import".to_string());
                i += 1;
            }
            JsToken::Ident(kw)
                if (is_node_fs_read_fn(kw.as_str())
                    || node_fs_fn_aliases.contains(kw.as_str())
                    || ((kw.as_str() == "open" || is_node_fs_qualified_meta_fn(kw.as_str()))
                        && i >= 2
                        && matches!(tokens.get(i - 1), Some(JsToken::Dot))
                        && (matches!(tokens.get(i - 2), Some(JsToken::Ident(m)) if node_fs_mod_aliases.contains(m))
                            || (i >= 5
                                && matches!(tokens.get(i - 2), Some(JsToken::RParen))
                                && matches!(tokens.get(i - 3), Some(JsToken::StringLit(ms)) if is_node_fs_mod(ms))
                                && matches!(tokens.get(i - 4), Some(JsToken::LParen))
                                && matches!(tokens.get(i - 5), Some(JsToken::Ident(r)) if r == "require")))))
                    && !matches!(
                        tokens.get(i.saturating_sub(1)),
                        Some(JsToken::Ident(prev))
                            if matches!(prev.as_str(), "function" | "class" | "as" | "import" | "export")
                    ) =>
            {
                let is_call = matches!(tokens.get(i + 1), Some(JsToken::LParen));
                let is_aliased_fn = node_fs_fn_aliases.contains(kw.as_str());
                let is_meta_fn = is_node_fs_meta_fn(kw.as_str())
                    || is_node_fs_qualified_meta_fn(kw.as_str())
                    || node_fs_meta_aliases.contains(kw.as_str());
                let is_fs_mod_member = i >= 2
                    && matches!(tokens.get(i - 1), Some(JsToken::Dot))
                    && (matches!(tokens.get(i - 2), Some(JsToken::Ident(m)) if node_fs_mod_aliases.contains(m))
                        || (i >= 5
                            && matches!(tokens.get(i - 2), Some(JsToken::RParen))
                            && matches!(tokens.get(i - 3), Some(JsToken::StringLit(ms)) if is_node_fs_mod(ms))
                            && matches!(tokens.get(i - 4), Some(JsToken::LParen))
                            && matches!(tokens.get(i - 5), Some(JsToken::Ident(r)) if r == "require")));
                if is_call {
                    if is_meta_fn {
                        // Directory listings (`readdirSync`, `readdir`, which observe empty subdirs)
                        // and metadata queries (`statSync`, `lstatSync`, `accessSync`, which observe
                        // permissions/timestamps) cannot be captured by file-byte hashes alone: fail closed.
                        defined_symbols.insert("fe_dynamic_import".to_string());
                        i += 1;
                        continue;
                    }
                    // Track explicit Node.js filesystem reads (`readFileSync("fixtures/input.bin")`,
                    // `fs.promises.open("assets/input.bin", "r")`, aliased `load("input.bin")`,
                    // `existsSync(...)`) regardless of extension AND hash their raw bytes.
                    // Fail closed on computed paths unless handled via exact `new URL('<static>', import.meta.url)`.
                    let is_static_new_url = matches!(tokens.get(i + 2), Some(JsToken::Ident(nw)) if nw == "new")
                        && matches!(tokens.get(i + 3), Some(JsToken::Ident(u)) if u == "URL")
                        && matches!(tokens.get(i + 4), Some(JsToken::LParen))
                        && matches!(tokens.get(i + 5), Some(JsToken::StringLit(s)) if !s.is_empty())
                        && matches!(tokens.get(i + 6), Some(JsToken::Comma))
                        && matches!(tokens.get(i + 7), Some(JsToken::Ident(im)) if im == "import")
                        && matches!(tokens.get(i + 8), Some(JsToken::Dot))
                        && matches!(tokens.get(i + 9), Some(JsToken::Ident(m)) if m == "meta")
                        && matches!(tokens.get(i + 10), Some(JsToken::Dot))
                        && matches!(tokens.get(i + 11), Some(JsToken::Ident(u)) if u == "url")
                        && matches!(tokens.get(i + 12), Some(JsToken::RParen))
                        && matches!(tokens.get(i + 13), Some(JsToken::RParen | JsToken::Comma));
                    if let Some(JsToken::StringLit(path_lit)) = tokens.get(i + 2)
                        && matches!(tokens.get(i + 3), Some(JsToken::RParen | JsToken::Comma))
                    {
                        let has_edge_ws = path_lit != path_lit.trim();
                        let has_posix_backslash = cfg!(unix) && path_lit.contains('\\');
                        let clean = path_lit
                            .trim_start_matches("./")
                            .trim_end_matches('/')
                            .replace('\\', "/");
                        if !has_edge_ws
                            && !has_posix_backslash
                            && !clean.is_empty()
                            && clean != "."
                            && !clean.starts_with('/')
                            && !clean.contains(['\n', '\r', '\0', '*', '?'])
                        {
                            data_refs.insert(clean.clone());
                            defined_symbols.insert(format!("fe_raw_fs:{clean}"));
                        } else {
                            // Normalized empty path (e.g. `"./"`), `"."`, literal edge whitespace,
                            // POSIX literal backslash, or absolute path: fail closed!
                            defined_symbols.insert("fe_dynamic_import".to_string());
                        }
                    } else if !is_static_new_url
                        && !matches!(tokens.get(i + 2), Some(JsToken::RParen))
                    {
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    }
                } else if is_fs_mod_member {
                    // Indirect filesystem function value (e.g. `const load = fs.readFileSync;`
                    // or `const load = require('node:fs').readFileSync;`): fail closed.
                    defined_symbols.insert("fe_dynamic_import".to_string());
                } else if is_aliased_fn
                    && !matches!(tokens.get(i.saturating_sub(1)), Some(JsToken::Dot | JsToken::LBrace | JsToken::Comma))
                    && !matches!(tokens.get(i + 1), Some(JsToken::RBrace | JsToken::Comma))
                {
                    defined_symbols.insert("fe_dynamic_import".to_string());
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "URL" => {
                // `new URL('./worker.ts', import.meta.url)` or `new URL('fixture.bin', import.meta.url)`
                if matches!(tokens.get(i + 1), Some(JsToken::LParen)) {
                    if let Some(JsToken::StringLit(spec)) = tokens.get(i + 2)
                        && matches!(tokens.get(i + 3), Some(JsToken::Comma))
                        && matches!(tokens.get(i + 4), Some(JsToken::Ident(im)) if im == "import")
                        && matches!(tokens.get(i + 5), Some(JsToken::Dot))
                        && matches!(tokens.get(i + 6), Some(JsToken::Ident(m)) if m == "meta")
                        && matches!(tokens.get(i + 7), Some(JsToken::Dot))
                        && matches!(tokens.get(i + 8), Some(JsToken::Ident(u)) if u == "url")
                        && matches!(tokens.get(i + 9), Some(JsToken::RParen))
                    {
                        // Per WHATWG URL basic URL parser: strip ONLY leading/trailing ASCII C0
                        // controls (`\x00..=\x1F`) and ASCII space (`0x20`) — preserving non-ASCII
                        // whitespace like U+00A0 — and remove embedded ASCII `\t`, `\n`, `\r`
                        // BEFORE splitting raw `#fragment` and `?query`.
                        let whatwg_trimmed = spec.trim_matches(|c: char| c <= ' ');
                        let whatwg_clean: String = whatwg_trimmed
                            .chars()
                            .filter(|&c| !matches!(c, '\t' | '\n' | '\r'))
                            .collect();
                        let raw_no_frag = whatwg_clean.split('#').next().unwrap_or(&whatwg_clean);
                        let raw_path_part = raw_no_frag.split('?').next().unwrap_or(raw_no_frag);
                        if !raw_path_part.is_empty() {
                            // Parse RFC 3986 scheme ONLY at the start of the URL (`^[A-Za-z][A-Za-z0-9+.-]*:`)
                            // case-insensitively so `FILE:///etc/hosts` fails closed while relative
                            // colon-bearing filenames like `./fixture:prod.bin` are tracked normally!
                            let url_scheme: Option<String> =
                                raw_path_part.find(':').and_then(|colon_idx| {
                                    let pfx = &raw_path_part[..colon_idx];
                                    let b = pfx.as_bytes();
                                    (!b.is_empty()
                                        && b[0].is_ascii_alphabetic()
                                        && b[1..].iter().all(|c| {
                                            c.is_ascii_alphanumeric()
                                                || matches!(c, b'+' | b'-' | b'.')
                                        }))
                                    .then(|| pfx.to_ascii_lowercase())
                                });
                            if url_scheme.as_deref() == Some("file") {
                                defined_symbols.insert("fe_dynamic_import".to_string());
                            } else if url_scheme.is_none() {
                                // Percent-decode URL path segments (`./foo%20bar.js` -> `./foo bar.js`)
                                // per WHATWG URL semantics; fail closed on invalid/NUL escapes.
                                let decoded_opt: Option<String> = if raw_path_part.contains('%') {
                                    let b = raw_path_part.as_bytes();
                                    let mut raw_bytes = Vec::with_capacity(b.len());
                                    let mut p = 0usize;
                                    let mut valid = true;
                                    while p < b.len() {
                                        if b[p] == b'%' {
                                            if p + 2 < b.len()
                                                && let (Some(hi), Some(lo)) = (
                                                    (b[p + 1] as char).to_digit(16),
                                                    (b[p + 2] as char).to_digit(16),
                                                )
                                            {
                                                let byte = ((hi << 4) | lo) as u8;
                                                if byte == 0 || byte == b'\n' || byte == b'\r' {
                                                    valid = false;
                                                    break;
                                                }
                                                raw_bytes.push(byte);
                                                p += 3;
                                                continue;
                                            }
                                            valid = false;
                                            break;
                                        }
                                        raw_bytes.push(b[p]);
                                        p += 1;
                                    }
                                    if valid {
                                        String::from_utf8(raw_bytes).ok()
                                    } else {
                                        None
                                    }
                                } else {
                                    Some(raw_path_part.to_string())
                                };
                                let Some(decoded_trimmed) = decoded_opt else {
                                    defined_symbols.insert("fe_dynamic_import".to_string());
                                    i += 10;
                                    continue;
                                };
                                if decoded_trimmed.starts_with('/') || decoded_trimmed.contains('\\') {
                                    // Absolute file-URL path (`new URL('/tmp/config.json', ...)`) OR
                                    // percent-decoded literal backslash (`%5C`, which on POSIX is a
                                    // filename character rather than a path separator): fail closed.
                                    defined_symbols.insert("fe_dynamic_import".to_string());
                                    i += 10;
                                    continue;
                                }
                                let rel_url = if decoded_trimmed.starts_with("./")
                                    || decoded_trimmed.starts_with("../")
                                {
                                    decoded_trimmed
                                } else {
                                    format!("./{decoded_trimmed}")
                                };
                                let data_clean = rel_url
                                    .trim_start_matches("./")
                                    .trim_end_matches('/')
                                    .replace('\\', "/");
                                if !data_clean.is_empty() {
                                    data_refs.insert(data_clean.clone());
                                }
                                if rel_url.contains(['?', '#']) || rel_url != rel_url.trim() {
                                    // Decoded literal `?`/`#` OR meaningful leading/trailing path
                                    // whitespace (e.g. `'./settings.json ?v=1'` -> `'./settings.json '`):
                                    // route via source-relative `fe_raw_url_fs:` (NOT `fe_raw_import:`,
                                    // whose specifier cleaner trims spaces and strips `?query`).
                                    if !data_clean.is_empty() {
                                        defined_symbols.insert(format!("fe_raw_url_fs:{data_clean}"));
                                    }
                                } else {
                                    imports.insert(ImportSpec {
                                        level: 0,
                                        module: rel_url.clone(),
                                        symbols: Vec::new(),
                                    });
                                    // Assets/modules referenced via `new URL(..., import.meta.url)` are
                                    // commonly read directly as raw bytes (`readFileSync`, `fetch`): hash
                                    // raw bytes so comment-only edits to fixture JS/CSS/TS invalidate!
                                    defined_symbols.insert(format!("fe_raw_import:{rel_url}"));
                                }
                            }
                        } else if spec.is_empty() {
                            // Interpolated template literal (`new URL(\`./assets/${name}.txt\`, import.meta.url)`)
                            // tokenizes to an empty `StringLit("")`: fail closed!
                            defined_symbols.insert("fe_dynamic_import".to_string());
                        }
                        i += 10;
                        continue;
                    }
                    // Also check non-literal first argument OR parenthesized/computed `import.meta.*`
                    // base (e.g. `new URL(relPath, import.meta.url)`, `new URL('./a.js', (import.meta.url))`,
                    // or `new URL('./a.bin', import.meta.resolve('./other.js'))`) at any paren nesting depth:
                    let mut j = i + 2;
                    let mut depth = 1usize;
                    let mut saw_import_meta = false;
                    while j < n && depth > 0 {
                        match &tokens[j] {
                            JsToken::LParen => depth += 1,
                            JsToken::RParen => depth -= 1,
                            JsToken::Ident(im)
                                if im == "import"
                                    && matches!(tokens.get(j + 1), Some(JsToken::Dot))
                                    && matches!(tokens.get(j + 2), Some(JsToken::Ident(m)) if m == "meta")
                                    && matches!(tokens.get(j + 3), Some(JsToken::Dot))
                                    && matches!(
                                        tokens.get(j + 4),
                                        Some(JsToken::Ident(u)) if u == "url" || u == "resolve"
                                    ) =>
                            {
                                saw_import_meta = true;
                                break;
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    if saw_import_meta {
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    }
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "play" => {
                // Storybook interaction test (`Primary.play = async ...` or `play: async ({ canvasElement }) => ...`)
                if matches!(tokens.get(i + 1), Some(JsToken::Other | JsToken::Colon)) {
                    markers.insert("storybook_play".to_string());
                    test_symbols.insert("play".to_string());
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "export" => {
                let (reexport_opt, is_type_only, exported_defs, reexport_links, next_i) =
                    parse_js_export_clause(&tokens, i + 1);
                // For `export default <expr>`, `parse_js_export_clause` stops immediately after
                // `default`, so scan the ENTIRE default-export expression (including arrow
                // functions `export default () => disk`, object/array literals `export default { disk }`,
                // `export default Object.freeze({ disk })`, and `export default function() { return disk }`)
                // up to the statement boundary without stopping at an intermediate `)` / `}`!
                let export_scan_end = if matches!(
                    tokens.get(i + 1),
                    Some(JsToken::Ident(kw)) if kw == "default"
                ) {
                    let start_de = next_i.min(n);
                    let mut de = start_de;
                    let mut brace_d = 0usize;
                    let mut paren_d = 0usize;
                    let mut brack_d = 0usize;
                    // Scan the complete default-export expression (stopping only at `;` or a new
                    // top-level declaration keyword `export`/`import`/`const`/`let`/`var` — never
                    // `function`/`class`, which can appear inside expressions like
                    // `export default cond ? function() {} : disk`!).
                    while de < n {
                        match &tokens[de] {
                            JsToken::LBrace => brace_d += 1,
                            JsToken::RBrace => brace_d = brace_d.saturating_sub(1),
                            JsToken::LParen => paren_d += 1,
                            JsToken::RParen => paren_d = paren_d.saturating_sub(1),
                            JsToken::LBracket => brack_d += 1,
                            JsToken::RBracket => brack_d = brack_d.saturating_sub(1),
                            JsToken::Semi if brace_d == 0 && paren_d == 0 && brack_d == 0 => {
                                break;
                            }
                            JsToken::Ident(kw)
                                if de > start_de
                                    && brace_d == 0
                                    && paren_d == 0
                                    && brack_d == 0
                                    && matches!(
                                        kw.as_str(),
                                        "export" | "import" | "const" | "let" | "var"
                                    ) =>
                            {
                                break;
                            }
                            _ => {}
                        }
                        de += 1;
                    }
                    de
                } else {
                    next_i.min(n)
                };
                // If an `export` clause re-exports `node:process` / `process` OR exports any
                // tracked `env`/`process` alias (`export { env };`, `export default env;`,
                // `export { env } from 'node:process';`), fail closed so downstream consumers
                // cannot read env vars without invalidation!
                if reexport_opt
                    .as_ref()
                    .is_some_and(|s| matches!(s.module.trim(), "node:process" | "process"))
                    || (!is_type_only
                        && tokens[i + 1..export_scan_end].iter().any(|t| {
                            matches!(t, JsToken::Ident(id)
                                if node_env_aliases.contains(id)
                                    || node_process_aliases.contains(id))
                        }))
                {
                    defined_symbols.insert("fe_dynamic_env".to_string());
                }
                // Also fail closed when exporting any tracked Node.js `fs` function or module
                // alias (`import { readFileSync as load } from 'node:fs'; export { load }`,
                // `import myfs from 'node:fs'; export default myfs;`, `export * from 'node:fs'`),
                // so downstream consumers cannot read fixtures through re-exported fs aliases!
                if reexport_opt
                    .as_ref()
                    .is_some_and(|s| is_node_fs_mod(&s.module))
                    || (!is_type_only
                        && tokens[i + 1..export_scan_end].iter().any(|t| {
                            matches!(t, JsToken::Ident(id)
                                if node_fs_fn_aliases.contains(id)
                                    || node_fs_meta_aliases.contains(id)
                                    || imported_node_fs_mod_aliases.contains(id))
                        }))
                {
                    defined_symbols.insert("fe_dynamic_import".to_string());
                }
                for d in exported_defs {
                    defined_symbols.insert(d);
                }
                for link in reexport_links {
                    defined_symbols.insert(link);
                }
                if let Some(spec) = reexport_opt {
                    if is_type_only {
                        type_checking_imports.insert(spec);
                    } else {
                        imports.insert(spec);
                    }
                }
                i = next_i;
            }
            JsToken::Ident(kw) if kw == "require" => {
                let prev_is_dot = i > 0 && matches!(tokens.get(i - 1), Some(JsToken::Dot));
                if !prev_is_dot && matches!(tokens.get(i + 1), Some(JsToken::LParen)) {
                    if let Some(JsToken::StringLit(spec)) = tokens.get(i + 2)
                        && matches!(tokens.get(i + 3), Some(JsToken::RParen))
                    {
                        if let Some(clean) = clean_frontend_import_specifier(spec) {
                            imports.insert(ImportSpec {
                                level: 0,
                                module: clean,
                                symbols: Vec::new(),
                            });
                        }
                        // Track inline `require('node:process').env.FLAG` / `require('process').env`
                        if is_node_fs_mod(spec)
                            && (matches!(tokens.get(i + 4), Some(JsToken::LBracket))
                                || (matches!(tokens.get(i + 4), Some(JsToken::Other))
                                    && matches!(tokens.get(i + 5), Some(JsToken::Dot))
                                    && matches!(tokens.get(i + 6), Some(JsToken::LBracket))))
                        {
                            // Inline computed `require('node:fs')["existsSync"](...)`: fail closed!
                            defined_symbols.insert("fe_dynamic_import".to_string());
                        }
                        if matches!(spec.trim(), "node:process" | "process") {
                            let after_req = i + 4;
                            let env_after: Option<usize> =
                                if matches!(tokens.get(after_req), Some(JsToken::Dot))
                                    && matches!(tokens.get(after_req + 1), Some(JsToken::Ident(e)) if e == "env")
                                {
                                    Some(after_req + 2)
                                } else if matches!(tokens.get(after_req), Some(JsToken::LBracket))
                                    && matches!(tokens.get(after_req + 1), Some(JsToken::StringLit(e)) if e == "env")
                                    && matches!(tokens.get(after_req + 2), Some(JsToken::RBracket))
                                {
                                    Some(after_req + 3)
                                } else {
                                    None
                                };
                            if let Some(ea) = env_after {
                                if matches!(tokens.get(ea), Some(JsToken::Dot))
                                    && let Some(JsToken::Ident(k)) = tokens.get(ea + 1)
                                    && is_valid_env_var_name(k)
                                {
                                    env_vars.insert(k.clone());
                                    i = ea + 2;
                                    continue;
                                }
                                if matches!(tokens.get(ea), Some(JsToken::LBracket))
                                    && let Some(JsToken::StringLit(k)) = tokens.get(ea + 1)
                                    && matches!(tokens.get(ea + 2), Some(JsToken::RBracket))
                                    && !k.is_empty()
                                    && !k.contains(['=', '\0'])
                                {
                                    env_vars.insert(k.clone());
                                    i = ea + 3;
                                    continue;
                                }
                                defined_symbols.insert("fe_dynamic_env".to_string());
                                i = ea;
                                continue;
                            } else if (matches!(tokens.get(after_req), Some(JsToken::Dot))
                                && matches!(
                                    tokens.get(after_req + 1),
                                    Some(JsToken::Ident(m)) if matches!(m.as_str(), "chdir" | "getBuiltinModule")
                                ))
                                || (matches!(tokens.get(after_req), Some(JsToken::Other))
                                    && matches!(tokens.get(after_req + 1), Some(JsToken::Dot))
                                    && matches!(
                                        tokens.get(after_req + 2),
                                        Some(JsToken::Ident(m)) if matches!(m.as_str(), "chdir" | "getBuiltinModule")
                                    ))
                                || (matches!(tokens.get(after_req), Some(JsToken::LBracket))
                                    && matches!(
                                        tokens.get(after_req + 1),
                                        Some(JsToken::StringLit(s))
                                            if matches!(s.as_str(), "chdir" | "getBuiltinModule")
                                    )
                                    && matches!(tokens.get(after_req + 2), Some(JsToken::RBracket)))
                            {
                                // Inline `require('node:process').chdir(...)` / `.getBuiltinModule(...)`
                                // / `["chdir"]`: fail closed!
                                defined_symbols.insert("fe_dynamic_import".to_string());
                            } else if !matches!(tokens.get(after_req), Some(JsToken::Dot))
                                && !(matches!(tokens.get(after_req), Some(JsToken::LBracket))
                                    && matches!(
                                        tokens.get(after_req + 1),
                                        Some(JsToken::StringLit(s))
                                            if !matches!(s.as_str(), "env" | "chdir" | "getBuiltinModule")
                                    )
                                    && matches!(tokens.get(after_req + 2), Some(JsToken::RBracket)))
                            {
                                // Only allow plain `const proc = require('node:process')` (already
                                // tracked in `node_process_aliases`) or destructuring `const { env } = require('node:process')`;
                                // fail closed on inline escapes like `require('node:process')[k]` or `fn(require('node:process'))`.
                                let is_tracked_decl = i >= 2
                                    && matches!(tokens.get(i - 1), Some(JsToken::Other))
                                    && (matches!(tokens.get(i - 2), Some(JsToken::Ident(al)) if node_process_aliases.contains(al))
                                        || tracked_process_destructure_rbraces.contains(&(i - 2)));
                                if !is_tracked_decl {
                                    defined_symbols.insert("fe_dynamic_env".to_string());
                                }
                            }
                        }
                        i += 4;
                        continue;
                    }
                    // Computed CommonJS `require(name)` or `require('./plugins/' + name)`:
                    // fail closed instead of silently missing or partially matching the target.
                    defined_symbols.insert("fe_dynamic_import".to_string());
                    i += 2;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if kw == "vi" || kw == "jest" => {
                if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && let Some(JsToken::Ident(method)) = tokens.get(i + 2)
                    && matches!(
                        method.as_str(),
                        "mock" | "doMock" | "importActual" | "requireActual"
                    )
                    && matches!(tokens.get(i + 3), Some(JsToken::LParen))
                {
                    if let Some(JsToken::StringLit(spec)) = tokens.get(i + 4)
                        && matches!(
                            tokens.get(i + 5),
                            Some(JsToken::RParen | JsToken::Comma)
                        )
                    {
                        if let Some(clean) = clean_frontend_import_specifier(spec) {
                            imports.insert(ImportSpec {
                                level: 0,
                                module: clean,
                                symbols: Vec::new(),
                            });
                        } else if spec.is_empty() {
                            defined_symbols.insert("fe_dynamic_import".to_string());
                        }
                    } else {
                        // Computed `vi.importActual(pathVar)` / `vi.mock(expr)` -> fail closed!
                        defined_symbols.insert("fe_dynamic_import".to_string());
                    }
                    i += 4;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw) if node_env_aliases.contains(kw) => {
                // Detect object spread (`{ ...e }` -> three consecutive `Dot` tokens) FIRST so
                // spreading an `env` alias is NOT mistaken for a property access `obj.e`!
                let prev_is_spread = i >= 3
                    && matches!(tokens.get(i - 1), Some(JsToken::Dot))
                    && matches!(tokens.get(i - 2), Some(JsToken::Dot))
                    && matches!(tokens.get(i - 3), Some(JsToken::Dot));
                if prev_is_spread {
                    defined_symbols.insert("fe_dynamic_env".to_string());
                    i += 1;
                    continue;
                }
                // Skip ONLY actual declaration/binding/property-access sites (`import { env }`,
                // `import { env as x }`, `const { env } = ...`, `obj.env`), NEVER expression
                // positions such as `return env.FEATURE_FLAG`, `typeof env.X`, or `await env.X`!
                let prev_is_prop_or_as = i > 0
                    && (matches!(tokens.get(i - 1), Some(JsToken::Dot))
                        || matches!(tokens.get(i - 1), Some(JsToken::Ident(p)) if p == "as"));
                let is_binding_in_braces = i > 0
                    && matches!(tokens.get(i - 1), Some(JsToken::LBrace | JsToken::Comma))
                    && (matches!(
                        tokens.get(i + 1),
                        Some(JsToken::RBrace | JsToken::Comma | JsToken::Colon | JsToken::Other)
                    ) || matches!(tokens.get(i + 1), Some(JsToken::Ident(nx)) if nx == "as"))
                    && {
                        // Distinguish real import/export/destructuring bindings (`import { env }`,
                        // `const { env } = process`) from object literals (`const config = { env }`)
                        // where `env` escapes into an untracked object property!
                        let mut b = i;
                        let mut found_lbrace = None;
                        while b > 0 {
                            b -= 1;
                            if matches!(tokens[b], JsToken::LBrace) {
                                found_lbrace = Some(b);
                                break;
                            }
                            if matches!(tokens[b], JsToken::RBrace | JsToken::Semi) {
                                break;
                            }
                        }
                        found_lbrace.is_some_and(|lb| {
                            let before_lb = lb.checked_sub(1).and_then(|idx| tokens.get(idx));
                            let before_comma = lb.checked_sub(2).and_then(|idx| tokens.get(idx));
                            matches!(
                                before_lb,
                                Some(JsToken::Ident(p))
                                    if matches!(p.as_str(), "import" | "export" | "const" | "let" | "var")
                            ) || (matches!(before_lb, Some(JsToken::Comma))
                                && matches!(before_comma, Some(JsToken::Ident(_)))
                                && lb.checked_sub(3).and_then(|idx| tokens.get(idx)).is_some_and(
                                    |t| matches!(t, JsToken::Ident(p) if p == "import"),
                                ))
                        })
                    };
                let is_decl_or_prop = prev_is_prop_or_as || is_binding_in_braces;
                if !is_decl_or_prop {
                    let next_dot = if matches!(tokens.get(i + 1), Some(JsToken::Dot)) {
                        Some(i + 1)
                    } else if matches!(tokens.get(i + 1), Some(JsToken::Other))
                        && matches!(tokens.get(i + 2), Some(JsToken::Dot))
                    {
                        // Optional chaining: `env?.FEATURE` or `env?.['FEATURE']`
                        Some(i + 2)
                    } else {
                        None
                    };
                    if let Some(dot_idx) = next_dot {
                        if let Some(JsToken::Ident(env_key)) = tokens.get(dot_idx + 1)
                            && is_valid_env_var_name(env_key)
                        {
                            env_vars.insert(env_key.clone());
                            i = dot_idx + 2;
                            continue;
                        }
                        if matches!(tokens.get(dot_idx + 1), Some(JsToken::LBracket)) {
                            if let Some(JsToken::StringLit(env_key)) = tokens.get(dot_idx + 2)
                                && matches!(tokens.get(dot_idx + 3), Some(JsToken::RBracket))
                                && !env_key.is_empty()
                                && !env_key.contains(['=', '\0'])
                            {
                                env_vars.insert(env_key.clone());
                                i = dot_idx + 4;
                                continue;
                            }
                            defined_symbols.insert("fe_dynamic_env".to_string());
                            i = dot_idx + 2;
                            continue;
                        }
                        defined_symbols.insert("fe_dynamic_env".to_string());
                        i = dot_idx + 1;
                        continue;
                    } else if matches!(tokens.get(i + 1), Some(JsToken::LBracket)) {
                        if let Some(JsToken::StringLit(env_key)) = tokens.get(i + 2)
                            && matches!(tokens.get(i + 3), Some(JsToken::RBracket))
                            && !env_key.is_empty()
                            && !env_key.contains(['=', '\0'])
                        {
                            env_vars.insert(env_key.clone());
                            i += 4;
                            continue;
                        }
                        defined_symbols.insert("fe_dynamic_env".to_string());
                        i += 2;
                        continue;
                    } else {
                        // Any non-property expression use of an `env` alias (`env || {}`, `env ?? {}`,
                        // `const e2 = env`, `fn(env)`, `{ env }`, `Object.keys(env)`): fail closed!
                        defined_symbols.insert("fe_dynamic_env".to_string());
                    }
                }
                i += 1;
            }
            JsToken::Ident(kw)
                if matches!(kw.as_str(), "globalThis" | "global" | "self" | "window") =>
            {
                // Computed global access (`globalThis['process'].env.FEATURE`, `global[k]`) has no
                // `process` identifier token; fail closed so env reads via global indexing cannot
                // produce stale cache hits.
                let bracket_idx = if matches!(tokens.get(i + 1), Some(JsToken::LBracket)) {
                    Some(i + 1)
                } else if matches!(tokens.get(i + 1), Some(JsToken::Other))
                    && matches!(tokens.get(i + 2), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 3), Some(JsToken::LBracket))
                {
                    Some(i + 3)
                } else {
                    None
                };
                if let Some(bi) = bracket_idx {
                    let is_safe_non_process_lit = matches!(
                        tokens.get(bi + 1),
                        Some(JsToken::StringLit(s)) if s != "process"
                    ) && matches!(tokens.get(bi + 2), Some(JsToken::RBracket));
                    if !is_safe_non_process_lit {
                        defined_symbols.insert("fe_dynamic_env".to_string());
                    }
                }
                i += 1;
            }
            JsToken::Ident(kw) if node_process_aliases.contains(kw) => {
                // Match `process.env`, `process?.env` (`Other` for `?` followed by `Dot`),
                // AND `process['env']` / `process?.['env']`.
                let env_end: Option<usize> = if matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 2), Some(JsToken::Ident(e)) if e == "env")
                {
                    Some(i + 3)
                } else if (matches!(tokens.get(i + 1), Some(JsToken::Other))
                    && matches!(tokens.get(i + 2), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 3), Some(JsToken::Ident(e)) if e == "env"))
                    || (matches!(tokens.get(i + 1), Some(JsToken::LBracket))
                        && matches!(tokens.get(i + 2), Some(JsToken::StringLit(e)) if e == "env")
                        && matches!(tokens.get(i + 3), Some(JsToken::RBracket)))
                {
                    Some(i + 4)
                } else if matches!(tokens.get(i + 1), Some(JsToken::Other))
                    && matches!(tokens.get(i + 2), Some(JsToken::Dot))
                    && matches!(tokens.get(i + 3), Some(JsToken::LBracket))
                    && matches!(tokens.get(i + 4), Some(JsToken::StringLit(e)) if e == "env")
                    && matches!(tokens.get(i + 5), Some(JsToken::RBracket))
                {
                    Some(i + 6)
                } else {
                    None
                };
                if let Some(after_env) = env_end {
                    let next_dot_idx = if matches!(tokens.get(after_env), Some(JsToken::Dot)) {
                        Some(after_env)
                    } else if matches!(tokens.get(after_env), Some(JsToken::Other))
                        && matches!(tokens.get(after_env + 1), Some(JsToken::Dot))
                    {
                        Some(after_env + 1)
                    } else {
                        None
                    };
                    if let Some(dot_idx) = next_dot_idx {
                        if let Some(JsToken::Ident(env_key)) = tokens.get(dot_idx + 1)
                            && is_valid_env_var_name(env_key)
                        {
                            env_vars.insert(env_key.clone());
                            i = dot_idx + 2;
                            continue;
                        }
                        if matches!(tokens.get(dot_idx + 1), Some(JsToken::LBracket)) {
                            if let Some(JsToken::StringLit(env_key)) = tokens.get(dot_idx + 2)
                                && matches!(tokens.get(dot_idx + 3), Some(JsToken::RBracket))
                                && !env_key.is_empty()
                                && !env_key.contains(['=', '\0'])
                            {
                                env_vars.insert(env_key.clone());
                                i = dot_idx + 4;
                                continue;
                            }
                            defined_symbols.insert("fe_dynamic_env".to_string());
                            i = dot_idx + 2;
                            continue;
                        }
                    }
                    if matches!(tokens.get(after_env), Some(JsToken::LBracket)) {
                        if let Some(JsToken::StringLit(env_key)) = tokens.get(after_env + 1)
                            && matches!(tokens.get(after_env + 2), Some(JsToken::RBracket))
                            && !env_key.is_empty()
                            && !env_key.contains(['=', '\0'])
                        {
                            env_vars.insert(env_key.clone());
                            i = after_env + 3;
                            continue;
                        }
                        // Computed `process.env[key]` -> fail closed.
                        defined_symbols.insert("fe_dynamic_env".to_string());
                        i = after_env + 1;
                        continue;
                    }
                    // Direct `process.env` value (e.g. `const { MODE } = process.env` or
                    // `Object.keys(process.env)`): fail closed.
                    defined_symbols.insert("fe_dynamic_env".to_string());
                    i = after_env;
                    continue;
                }
                // Check if the `process` alias itself escapes as a value OR is indexed by a
                // computed property (`process[key]`, which could evaluate to `process['env']`)
                // rather than a safe static non-env member access (`process.cwd()`, `process['platform']`)
                // or declaration/import/local-alias assignment.
                let is_static_safe_member = matches!(tokens.get(i + 1), Some(JsToken::Dot))
                    || (matches!(tokens.get(i + 1), Some(JsToken::LBracket))
                        && matches!(
                            tokens.get(i + 2),
                            Some(JsToken::StringLit(s))
                                if !matches!(s.as_str(), "env" | "chdir" | "getBuiltinModule")
                        )
                        && matches!(tokens.get(i + 3), Some(JsToken::RBracket)))
                    || (matches!(tokens.get(i + 1), Some(JsToken::Other))
                        && matches!(tokens.get(i + 2), Some(JsToken::Dot))
                        && (matches!(tokens.get(i + 3), Some(JsToken::Ident(_)))
                            || (matches!(tokens.get(i + 3), Some(JsToken::LBracket))
                                && matches!(
                                    tokens.get(i + 4),
                                    Some(JsToken::StringLit(s))
                                        if !matches!(s.as_str(), "env" | "chdir" | "getBuiltinModule")
                                )
                                && matches!(tokens.get(i + 5), Some(JsToken::RBracket)))));
                let is_global_process_prop = i >= 2
                    && matches!(tokens.get(i - 1), Some(JsToken::Dot))
                    && matches!(
                        tokens.get(i - 2),
                        Some(JsToken::Ident(g))
                            if matches!(g.as_str(), "globalThis" | "global" | "self" | "window")
                    );
                let is_member_or_binding = is_static_safe_member
                    || (i > 0
                        && ((matches!(tokens.get(i - 1), Some(JsToken::Dot))
                            && !is_global_process_prop)
                            || matches!(
                                tokens.get(i - 1),
                                Some(JsToken::Ident(p))
                                    if matches!(p.as_str(), "as" | "const" | "let" | "var" | "import" | "function" | "class")
                            )))
                    || (i >= 2
                        && matches!(tokens.get(i - 1), Some(JsToken::Other))
                        && (matches!(tokens.get(i - 2), Some(JsToken::Ident(al)) if node_process_aliases.contains(al))
                            || tracked_process_destructure_rbraces.contains(&(i - 2)))
                        && matches!(tokens.get(i + 1), Some(JsToken::Semi | JsToken::Ident(_) | JsToken::RBrace)))
                    || (is_global_process_prop
                        && i >= 4
                        && matches!(tokens.get(i - 3), Some(JsToken::Other))
                        && matches!(tokens.get(i - 4), Some(JsToken::Ident(al)) if node_process_aliases.contains(al))
                        && matches!(tokens.get(i + 1), Some(JsToken::Semi | JsToken::Ident(_) | JsToken::RBrace)));
                if !is_member_or_binding {
                    defined_symbols.insert("fe_dynamic_env".to_string());
                }
                i += 1;
            }
            JsToken::Ident(kw) if matches!(kw.as_str(), "test" | "it" | "describe") => {
                let mut j = i + 1;
                while j < n && matches!(tokens.get(j), Some(JsToken::Dot)) {
                    if let Some(JsToken::Ident(modifier)) = tokens.get(j + 1) {
                        match modifier.as_str() {
                            "skip" | "todo" => {
                                markers.insert(modifier.clone());
                            }
                            "concurrent" => {
                                markers.insert("concurrent".to_string());
                                module_markers.insert("parallel_safe".to_string());
                            }
                            "sequential" | "serial" => {
                                markers.insert("serial".to_string());
                                module_markers.insert("serial".to_string());
                            }
                            "only" => {
                                markers.insert("only".to_string());
                            }
                            _ => {}
                        }
                        j += 2;
                        // Handle `.each(...)` call before `('test name', ...)`
                        if modifier == "each" && matches!(tokens.get(j), Some(JsToken::LParen)) {
                            let mut depth = 1usize;
                            j += 1;
                            while j < n && depth > 0 {
                                match &tokens[j] {
                                    JsToken::LParen => depth += 1,
                                    JsToken::RParen => depth -= 1,
                                    _ => {}
                                }
                                j += 1;
                            }
                        }
                    } else {
                        break;
                    }
                }
                if matches!(tokens.get(j), Some(JsToken::LParen))
                    && let Some(JsToken::StringLit(name)) = tokens.get(j + 1)
                {
                    let trimmed = name.trim();
                    if !trimmed.is_empty() && trimmed.len() <= 200 {
                        test_symbols.insert(trimmed.to_string());
                    }
                    i = j + 2;
                    continue;
                }
                i += 1;
            }
            JsToken::Ident(kw)
                if matches!(kw.as_str(), "function" | "class" | "const" | "let") =>
            {
                if let Some(JsToken::Ident(sym_name)) = tokens.get(i + 1) {
                    defined_symbols.insert(sym_name.clone());
                    // Detect `const ThemeContext = createContext(...)` or `React.createContext(...)`
                    for lookahead in 2..=6 {
                        if matches!(
                            tokens.get(i + lookahead),
                            Some(JsToken::Ident(fn_id)) if fn_id == "createContext"
                        ) {
                            defined_symbols.insert(format!("ctx_def:{sym_name}"));
                            break;
                        }
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Detect whether this file is a pure re-export barrel AND whether it executes top-level
    // side effects when imported (e.g. bare function calls / assignments at module scope).
    if !frontend_tokens_are_pure_reexports(&tokens) {
        defined_symbols.insert("fe_has_side_effects".to_string());
    }
    if frontend_tokens_have_top_level_side_effects(&tokens) {
        defined_symbols.insert("fe_mod_side_effects".to_string());
    }

    ParsedPythonFile {
        imports: imports.into_iter().collect(),
        test_symbols: test_symbols.into_iter().collect(),
        fixtures_defined: Vec::new(),
        fixtures_used: Vec::new(),
        defined_symbols: defined_symbols.into_iter().collect(),
        data_refs: data_refs.into_iter().collect(),
        markers: markers.into_iter().collect(),
        module_markers: module_markers.into_iter().collect(),
        rust_mods: Vec::new(),
        type_checking_imports: type_checking_imports.into_iter().collect(),
        env_vars_referenced: env_vars.into_iter().collect(),
    }
}


