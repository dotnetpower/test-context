use super::*;

pub(super) fn escape_go_regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if matches!(
            ch,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Collects existing directories (including empty hidden directories like `.hidden/tests` and
/// their existing ancestors) reachable by configured pytest `testpaths` patterns.
pub(super) fn collect_testpath_watch_dirs(
    root: &Path,
    testpaths: &[String],
    policy: &crate::policy::MemoryPolicy,
) -> Vec<PathBuf> {
    if testpaths.is_empty() {
        return Vec::new();
    }
    let mut dirs = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let p = entry.path();
            let dname = entry.file_name();
            if matches!(dname.to_str(), Some(".git" | ".hg" | ".svn" | "node_modules" | "target")) {
                continue;
            }
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            let rel_s = normalize_rel_str(rel);
            if crate::ast_graph::dir_could_match_testpaths(&rel_s, testpaths) {
                visited += 1;
                if visited > policy.max_watch_dirs {
                    break;
                }
                if dirs.insert(p.clone()) {
                    stack.push(p);
                }
            }
        }
    }
    dirs.into_iter().collect()
}

pub(super) fn find_enclosing_go_module_dir(workspace_root: &Path, test_file: &str) -> String {
    // Respect `GOWORK` from process env OR persisted `GOENV`:
    // - Explicit `GOWORK=<path>`: applies globally from `workspace_root`.
    // - Unset or `GOWORK=auto`: walk up from `test_file`'s directory to find the NEAREST enclosing
    //   `go.work` (so nested `go.work` workspaces run from their own directory instead of an outer
    //   `go.work` that doesn't list the nested module!).
    // - `GOWORK=off` or no enclosing `go.work`: fall back to the nearest enclosing `go.mod`.
    let gowork_env = crate::ast_graph::effective_gowork_value().unwrap_or_default();
    let gowork_trimmed = gowork_env.trim();
    if gowork_trimmed != "off" {
        if !gowork_trimmed.is_empty() && gowork_trimmed != "auto" {
            let p = Path::new(gowork_trimmed);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                workspace_root.join(p)
            };
            if abs.is_file() {
                return String::new();
            }
        } else {
            let mut cur = Path::new(test_file).parent();
            while let Some(dir) = cur {
                let rel_dir = normalize_rel_str(dir);
                let work_file = if rel_dir.is_empty() {
                    workspace_root.join("go.work")
                } else {
                    workspace_root.join(&rel_dir).join("go.work")
                };
                if work_file.is_file() {
                    return rel_dir;
                }
                if rel_dir.is_empty() {
                    break;
                }
                cur = dir.parent();
            }
        }
    }
    let mut cur = Path::new(test_file).parent();
    while let Some(dir) = cur {
        let rel_dir = normalize_rel_str(dir);
        let mod_file = if rel_dir.is_empty() {
            workspace_root.join("go.mod")
        } else {
            workspace_root.join(&rel_dir).join("go.mod")
        };
        if mod_file.is_file() {
            return rel_dir;
        }
        if rel_dir.is_empty() {
            break;
        }
        cur = dir.parent();
    }
    String::new()
}

pub(super) fn go_pkg_arg_relative_to_mod(test_file: &str, mod_dir: &str) -> String {
    let dir = Path::new(test_file)
        .parent()
        .map(normalize_rel_str)
        .unwrap_or_default();
    let rel_from_mod = if mod_dir.is_empty() {
        dir.as_str()
    } else if dir == mod_dir {
        ""
    } else {
        dir.strip_prefix(&format!("{mod_dir}/")).unwrap_or(dir.as_str())
    };
    if rel_from_mod.is_empty() {
        ".".to_string()
    } else {
        format!("./{rel_from_mod}")
    }
}

/// Reads a Go test file respecting any `-overlay` JSON replacement map in `extra_args`.
/// Returns `Err` if `-overlay` was passed but its JSON file cannot be read/parsed.
pub(super) fn read_go_test_file_with_overlay(
    workspace_root: &Path,
    mod_workdir: &Path,
    test_file: &str,
    extra_args: &[String],
) -> Result<Vec<u8>> {
    let mut overlay_path: Option<String> = None;
    let mut scan_overlay_tokens = |toks: &[String]| {
        let mut idx = 0usize;
        while idx < toks.len() {
            let a = toks[idx].as_str();
            if matches!(a, "-overlay" | "--overlay") && idx + 1 < toks.len() {
                overlay_path = Some(toks[idx + 1].clone());
                idx += 2;
                continue;
            }
            if let Some(p) = a
                .strip_prefix("-overlay=")
                .or_else(|| a.strip_prefix("--overlay="))
            {
                overlay_path = Some(p.to_string());
            }
            idx += 1;
        }
    };
    // Check persisted `go env -w` (`GOENV`) first, then process `GOFLAGS`, then explicit CLI
    // `extra_args` (later sources override earlier ones, matching Go's exact precedence).
    if let Some(env_gf) = std::env::var("GOFLAGS").ok().or_else(|| {
        crate::ast_graph::read_effective_goenv_var("GOFLAGS")
    }) {
        let gf_tokens: Vec<String> = env_gf.split_whitespace().map(ToOwned::to_owned).collect();
        scan_overlay_tokens(&gf_tokens);
    }
    scan_overlay_tokens(extra_args);
    let abs_orig = workspace_root.join(test_file);
    if let Some(ov_raw) = overlay_path.as_deref() {
        let ov_file = if Path::new(ov_raw).is_absolute() {
            PathBuf::from(ov_raw)
        } else {
            mod_workdir.join(ov_raw)
        };
        let ov_bytes = std::fs::read(&ov_file).map_err(|e| {
            invalid(format!(
                "Cannot read Go -overlay file '{}' for selector ownership: {e}",
                ov_file.display()
            ))
        })?;
        let ov_json: serde_json::Value = serde_json::from_slice(&ov_bytes).map_err(|e| {
            invalid(format!(
                "Invalid Go -overlay JSON '{}': {e}",
                ov_file.display()
            ))
        })?;
        if let Some(replace_map) = ov_json.get("Replace").and_then(serde_json::Value::as_object) {
            let orig_canon = std::fs::canonicalize(&abs_orig).unwrap_or_else(|_| abs_orig.clone());
            for (k, v) in replace_map {
                let k_abs = if Path::new(k).is_absolute() {
                    PathBuf::from(k)
                } else {
                    mod_workdir.join(k)
                };
                let k_canon = std::fs::canonicalize(&k_abs).unwrap_or(k_abs);
                if k_canon == orig_canon
                    && let Some(repl) = v.as_str()
                {
                    if repl.is_empty() {
                        return Ok(Vec::new());
                    }
                    let repl_abs = if Path::new(repl).is_absolute() {
                        PathBuf::from(repl)
                    } else {
                        mod_workdir.join(repl)
                    };
                    return std::fs::read(&repl_abs).map_err(|e| {
                        invalid(format!(
                            "Cannot read Go overlay replacement '{}': {e}",
                            repl_abs.display()
                        ))
                        .into()
                    });
                }
            }
        }
    }
    Ok(std::fs::read(&abs_orig).unwrap_or_default())
}

/// Mirrors Go's `testing.rewrite` (`testing/match.go`) + `strconv.IsPrint` / `strconv.QuoteRune`:
/// replaces ALL Unicode whitespace with `'_'` and quotes every non-printable Unicode code point
/// (control `Cc`, format `Cf`, private-use `Co`, unassigned `Cn` like `U+0378`, etc.) while
/// preserving ALL printable Unicode categories `L`, `M` (including variation selectors like `U+FE0F`
/// in `❤️`), `N`, `P`, `S` using Rust's built-in Unicode `Debug` printability + sorted Unicode
/// Mark/filler ranges!
pub(super) fn go_testing_rewrite(s: &str) -> String {
    pub(super) fn is_go_printable(c: char) -> bool {
        if c.is_ascii() {
            return (' '..='~').contains(&c);
        }
        // Rust's `format!("{s:?}")` uses stdlib's full Unicode table for L, N, P, S (rejecting
        // all Cc, Cf, Cs, Co, Cn including unassigned U+0378, and Zs/Zl/Zp).
        let mut buf = [0u8; 5];
        buf[0] = b'a';
        let n = 1 + c.encode_utf8(&mut buf[1..]).len();
        if let Ok(pair) = std::str::from_utf8(&buf[..n])
            && format!("{pair:?}").len() == n + 2
        {
            return true;
        }
        // Unicode `M` (Mn/Mc/Me Marks) + the 4 Hangul filler `Lo` chars that Go `strconv.IsPrint`
        // considers printable while Rust's `Debug` escapes them.
        const GO_EXTRA_PRINTABLE_RANGES: &[(u32, u32)] = &[
            (0x300, 0x36F), (0x483, 0x489), (0x591, 0x5BD), (0x5BF, 0x5BF), (0x5C1, 0x5C2), (0x5C4, 0x5C5),
            (0x5C7, 0x5C7), (0x610, 0x61A), (0x64B, 0x65F), (0x670, 0x670), (0x6D6, 0x6DC), (0x6DF, 0x6E4),
            (0x6E7, 0x6E8), (0x6EA, 0x6ED), (0x711, 0x711), (0x730, 0x74A), (0x7A6, 0x7B0), (0x7EB, 0x7F3),
            (0x7FD, 0x7FD), (0x816, 0x819), (0x81B, 0x823), (0x825, 0x827), (0x829, 0x82D), (0x859, 0x85B),
            (0x898, 0x89F), (0x8CA, 0x8E1), (0x8E3, 0x903), (0x93A, 0x93C), (0x93E, 0x94F), (0x951, 0x957),
            (0x962, 0x963), (0x981, 0x983), (0x9BC, 0x9BC), (0x9BE, 0x9C4), (0x9C7, 0x9C8), (0x9CB, 0x9CD),
            (0x9D7, 0x9D7), (0x9E2, 0x9E3), (0x9FE, 0x9FE), (0xA01, 0xA03), (0xA3C, 0xA3C), (0xA3E, 0xA42),
            (0xA47, 0xA48), (0xA4B, 0xA4D), (0xA51, 0xA51), (0xA70, 0xA71), (0xA75, 0xA75), (0xA81, 0xA83),
            (0xABC, 0xABC), (0xABE, 0xAC5), (0xAC7, 0xAC9), (0xACB, 0xACD), (0xAE2, 0xAE3), (0xAFA, 0xAFF),
            (0xB01, 0xB03), (0xB3C, 0xB3C), (0xB3E, 0xB44), (0xB47, 0xB48), (0xB4B, 0xB4D), (0xB55, 0xB57),
            (0xB62, 0xB63), (0xB82, 0xB82), (0xBBE, 0xBC2), (0xBC6, 0xBC8), (0xBCA, 0xBCD), (0xBD7, 0xBD7),
            (0xC00, 0xC04), (0xC3C, 0xC3C), (0xC3E, 0xC44), (0xC46, 0xC48), (0xC4A, 0xC4D), (0xC55, 0xC56),
            (0xC62, 0xC63), (0xC81, 0xC83), (0xCBC, 0xCBC), (0xCBE, 0xCC4), (0xCC6, 0xCC8), (0xCCA, 0xCCD),
            (0xCD5, 0xCD6), (0xCE2, 0xCE3), (0xCF3, 0xCF3), (0xD00, 0xD03), (0xD3B, 0xD3C), (0xD3E, 0xD44),
            (0xD46, 0xD48), (0xD4A, 0xD4D), (0xD57, 0xD57), (0xD62, 0xD63), (0xD81, 0xD83), (0xDCA, 0xDCA),
            (0xDCF, 0xDD4), (0xDD6, 0xDD6), (0xDD8, 0xDDF), (0xDF2, 0xDF3), (0xE31, 0xE31), (0xE34, 0xE3A),
            (0xE47, 0xE4E), (0xEB1, 0xEB1), (0xEB4, 0xEBC), (0xEC8, 0xECE), (0xF18, 0xF19), (0xF35, 0xF35),
            (0xF37, 0xF37), (0xF39, 0xF39), (0xF3E, 0xF3F), (0xF71, 0xF84), (0xF86, 0xF87), (0xF8D, 0xF97),
            (0xF99, 0xFBC), (0xFC6, 0xFC6), (0x102B, 0x103E), (0x1056, 0x1059), (0x105E, 0x1060), (0x1062, 0x1064),
            (0x1067, 0x106D), (0x1071, 0x1074), (0x1082, 0x108D), (0x108F, 0x108F), (0x109A, 0x109D), (0x115F, 0x1160),
            (0x135D, 0x135F), (0x1712, 0x1715), (0x1732, 0x1734), (0x1752, 0x1753), (0x1772, 0x1773), (0x17B4, 0x17D3),
            (0x17DD, 0x17DD), (0x180B, 0x180D), (0x180F, 0x180F), (0x1885, 0x1886), (0x18A9, 0x18A9), (0x1920, 0x192B),
            (0x1930, 0x193B), (0x1A17, 0x1A1B), (0x1A55, 0x1A5E), (0x1A60, 0x1A7C), (0x1A7F, 0x1A7F), (0x1AB0, 0x1ACE),
            (0x1B00, 0x1B04), (0x1B34, 0x1B44), (0x1B6B, 0x1B73), (0x1B80, 0x1B82), (0x1BA1, 0x1BAD), (0x1BE6, 0x1BF3),
            (0x1C24, 0x1C37), (0x1CD0, 0x1CD2), (0x1CD4, 0x1CE8), (0x1CED, 0x1CED), (0x1CF4, 0x1CF4), (0x1CF7, 0x1CF9),
            (0x1DC0, 0x1DFF), (0x20D0, 0x20F0), (0x2CEF, 0x2CF1), (0x2D7F, 0x2D7F), (0x2DE0, 0x2DFF), (0x302A, 0x302F),
            (0x3099, 0x309A), (0x3164, 0x3164), (0xA66F, 0xA672), (0xA674, 0xA67D), (0xA69E, 0xA69F), (0xA6F0, 0xA6F1),
            (0xA802, 0xA802), (0xA806, 0xA806), (0xA80B, 0xA80B), (0xA823, 0xA827), (0xA82C, 0xA82C), (0xA880, 0xA881),
            (0xA8B4, 0xA8C5), (0xA8E0, 0xA8F1), (0xA8FF, 0xA8FF), (0xA926, 0xA92D), (0xA947, 0xA953), (0xA980, 0xA983),
            (0xA9B3, 0xA9C0), (0xA9E5, 0xA9E5), (0xAA29, 0xAA36), (0xAA43, 0xAA43), (0xAA4C, 0xAA4D), (0xAA7B, 0xAA7D),
            (0xAAB0, 0xAAB0), (0xAAB2, 0xAAB4), (0xAAB7, 0xAAB8), (0xAABE, 0xAABF), (0xAAC1, 0xAAC1), (0xAAEB, 0xAAEF),
            (0xAAF5, 0xAAF6), (0xABE3, 0xABEA), (0xABEC, 0xABED), (0xFB1E, 0xFB1E), (0xFE00, 0xFE0F), (0xFE20, 0xFE2F),
            (0xFFA0, 0xFFA0), (0x101FD, 0x101FD), (0x102E0, 0x102E0), (0x10376, 0x1037A), (0x10A01, 0x10A03), (0x10A05, 0x10A06),
            (0x10A0C, 0x10A0F), (0x10A38, 0x10A3A), (0x10A3F, 0x10A3F), (0x10AE5, 0x10AE6), (0x10D24, 0x10D27), (0x10EAB, 0x10EAC),
            (0x10EFD, 0x10EFF), (0x10F46, 0x10F50), (0x10F82, 0x10F85), (0x11000, 0x11002), (0x11038, 0x11046), (0x11070, 0x11070),
            (0x11073, 0x11074), (0x1107F, 0x11082), (0x110B0, 0x110BA), (0x110C2, 0x110C2), (0x11100, 0x11102), (0x11127, 0x11134),
            (0x11145, 0x11146), (0x11173, 0x11173), (0x11180, 0x11182), (0x111B3, 0x111C0), (0x111C9, 0x111CC), (0x111CE, 0x111CF),
            (0x1122C, 0x11237), (0x1123E, 0x1123E), (0x11241, 0x11241), (0x112DF, 0x112EA), (0x11300, 0x11303), (0x1133B, 0x1133C),
            (0x1133E, 0x11344), (0x11347, 0x11348), (0x1134B, 0x1134D), (0x11357, 0x11357), (0x11362, 0x11363), (0x11366, 0x1136C),
            (0x11370, 0x11374), (0x11435, 0x11446), (0x1145E, 0x1145E), (0x114B0, 0x114C3), (0x115AF, 0x115B5), (0x115B8, 0x115C0),
            (0x115DC, 0x115DD), (0x11630, 0x11640), (0x116AB, 0x116B7), (0x1171D, 0x1172B), (0x1182C, 0x1183A), (0x11930, 0x11935),
            (0x11937, 0x11938), (0x1193B, 0x1193E), (0x11940, 0x11940), (0x11942, 0x11943), (0x119D1, 0x119D7), (0x119DA, 0x119E0),
            (0x119E4, 0x119E4), (0x11A01, 0x11A0A), (0x11A33, 0x11A39), (0x11A3B, 0x11A3E), (0x11A47, 0x11A47), (0x11A51, 0x11A5B),
            (0x11A8A, 0x11A99), (0x11C2F, 0x11C36), (0x11C38, 0x11C3F), (0x11C92, 0x11CA7), (0x11CA9, 0x11CB6), (0x11D31, 0x11D36),
            (0x11D3A, 0x11D3A), (0x11D3C, 0x11D3D), (0x11D3F, 0x11D45), (0x11D47, 0x11D47), (0x11D8A, 0x11D8E), (0x11D90, 0x11D91),
            (0x11D93, 0x11D97), (0x11EF3, 0x11EF6), (0x11F00, 0x11F01), (0x11F03, 0x11F03), (0x11F34, 0x11F3A), (0x11F3E, 0x11F42),
            (0x13440, 0x13440), (0x13447, 0x13455), (0x16AF0, 0x16AF4), (0x16B30, 0x16B36), (0x16F4F, 0x16F4F), (0x16F51, 0x16F87),
            (0x16F8F, 0x16F92), (0x16FE4, 0x16FE4), (0x16FF0, 0x16FF1), (0x1BC9D, 0x1BC9E), (0x1CF00, 0x1CF2D), (0x1CF30, 0x1CF46),
            (0x1D165, 0x1D169), (0x1D16D, 0x1D172), (0x1D17B, 0x1D182), (0x1D185, 0x1D18B), (0x1D1AA, 0x1D1AD), (0x1D242, 0x1D244),
            (0x1DA00, 0x1DA36), (0x1DA3B, 0x1DA6C), (0x1DA75, 0x1DA75), (0x1DA84, 0x1DA84), (0x1DA9B, 0x1DA9F), (0x1DAA1, 0x1DAAF),
            (0x1E000, 0x1E006), (0x1E008, 0x1E018), (0x1E01B, 0x1E021), (0x1E023, 0x1E024), (0x1E026, 0x1E02A), (0x1E08F, 0x1E08F),
            (0x1E130, 0x1E136), (0x1E2AE, 0x1E2AE), (0x1E2EC, 0x1E2EF), (0x1E4EC, 0x1E4EF), (0x1E8D0, 0x1E8D6), (0x1E944, 0x1E94A),
            (0xE0100, 0xE01EF),
        ];
        let cp = c as u32;
        GO_EXTRA_PRINTABLE_RANGES
            .binary_search_by(|&(lo, hi)| {
                if cp < lo {
                    std::cmp::Ordering::Greater
                } else if cp > hi {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            out.push('_');
        } else if !is_go_printable(c) {
            match c {
                '\x07' => out.push_str("\\a"),
                '\x08' => out.push_str("\\b"),
                _ => {
                    let code = c as u32;
                    if code <= 0xff {
                        out.push_str(&format!("\\x{code:02x}"));
                    } else if code <= 0xffff {
                        out.push_str(&format!("\\u{code:04x}"));
                    } else {
                        out.push_str(&format!("\\U{code:08x}"));
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Lightweight Go RE2-compatible regex evaluator for single Go `-run` subtest segments (`^`, `$`,
/// `.`, `*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}`, `[...]`, `[^...]`, POSIX `[:alpha:]`/`[:^digit:]`,
/// `(a|b)`, `(?:...)`, `(?i)`/`(?-i)`/`(?s)`/`(?m)`/`(?U)`/`(?flags:...)`, `(?P<name>...)`,
/// `\d`/`\D`/`\w`/`\W`/`\s`/`\S`, `\a`/`\f`/`\v`/`\t`/`\n`/`\r`/`\xHH`/`\x{...}`, escaped
/// literals). Returns `Err(msg)` on malformed/unsupported syntax so invalid regexes NEVER silently
/// match or report green!
pub(super) fn go_re2_segment_matches(pattern: &str, text: &str) -> std::result::Result<bool, String> {
    #[derive(Clone, Copy, Default)]
    struct Re2Flags {
        ci: bool,
        dot_all: bool,
        multi_line: bool,
    }
    #[derive(Clone)]
    enum ClassItem {
        Range(char, char),
        NegRanges(Vec<(char, char)>),
    }
    #[derive(Clone)]
    enum Atom {
        Lit { ch: char, ci: bool },
        Any { dot_all: bool },
        Class { neg: bool, ci: bool, items: Vec<ClassItem> },
        Group(Vec<Vec<(Atom, usize, Option<usize>)>>),
        Start { multi_line: bool },
        End { multi_line: bool },
    }
    type Seq = Vec<(Atom, usize, Option<usize>)>;
    type Alts = Vec<Seq>;

    pub(super) fn posix_class_ranges(name: &str) -> Option<Vec<(char, char)>> {
        match name {
            "alnum" => Some(vec![('0', '9'), ('A', 'Z'), ('a', 'z')]),
            "alpha" => Some(vec![('A', 'Z'), ('a', 'z')]),
            "ascii" => Some(vec![('\x00', '\x7f')]),
            "blank" => Some(vec![('\t', '\t'), (' ', ' ')]),
            "cntrl" => Some(vec![('\x00', '\x1f'), ('\x7f', '\x7f')]),
            "digit" => Some(vec![('0', '9')]),
            "graph" => Some(vec![('!', '~')]),
            "lower" => Some(vec![('a', 'z')]),
            "print" => Some(vec![(' ', '~')]),
            "punct" => Some(vec![('!', '/'), (':', '@'), ('[', '`'), ('{', '~')]),
            "space" => Some(vec![
                ('\t', '\t'),
                ('\n', '\n'),
                ('\x0b', '\x0b'),
                ('\x0c', '\x0c'),
                ('\r', '\r'),
                (' ', ' '),
            ]),
            "upper" => Some(vec![('A', 'Z')]),
            "word" => Some(vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')]),
            "xdigit" => Some(vec![('0', '9'), ('A', 'F'), ('a', 'f')]),
            _ => None,
        }
    }

    pub(super) fn parse_hex_escape(chars: &[char], pos: &mut usize) -> std::result::Result<char, String> {
        if *pos < chars.len() && chars[*pos] == '{' {
            *pos += 1;
            let start = *pos;
            while *pos < chars.len() && chars[*pos] != '}' {
                *pos += 1;
            }
            if *pos >= chars.len() || *pos == start {
                return Err("invalid '\\x{...}' escape".to_string());
            }
            let hex: String = chars[start..*pos].iter().collect();
            *pos += 1;
            let code = u32::from_str_radix(&hex, 16)
                .map_err(|_| format!("invalid hex escape '\\x{{{hex}}}'"))?;
            char::from_u32(code).ok_or_else(|| format!("invalid Unicode scalar '\\x{{{hex}}}'"))
        } else if *pos + 1 < chars.len() {
            let hex: String = chars[*pos..*pos + 2].iter().collect();
            *pos += 2;
            let code = u32::from_str_radix(&hex, 16)
                .map_err(|_| format!("invalid hex escape '\\x{hex}'"))?;
            char::from_u32(code).ok_or_else(|| format!("invalid byte escape '\\x{hex}'"))
        } else {
            Err("truncated '\\x' escape".to_string())
        }
    }

    pub(super) fn parse_alts(
        chars: &[char],
        pos: &mut usize,
        in_group: bool,
        depth: usize,
        outer_flags: Re2Flags,
    ) -> std::result::Result<Alts, String> {
        if depth > 64 {
            return Err("regex group nesting depth limit exceeded".to_string());
        }
        let mut alts = Vec::new();
        // RE2 inline flags like `(?i)` apply lexically to ALL subsequent alternatives within the
        // enclosing group (`(?:(?i)foo|bar)` matches `BAR`), not only the current branch!
        let mut flags = outer_flags;
        loop {
            let mut seq: Seq = Vec::new();
            while *pos < chars.len() {
                let c = chars[*pos];
                if c == '|' || (in_group && c == ')') {
                    break;
                }
                if !in_group && c == ')' {
                    return Err("unmatched ')'".to_string());
                }
                let atom = match c {
                    '^' => {
                        *pos += 1;
                        seq.push((Atom::Start { multi_line: flags.multi_line }, 1, Some(1)));
                        continue;
                    }
                    '$' => {
                        *pos += 1;
                        seq.push((Atom::End { multi_line: flags.multi_line }, 1, Some(1)));
                        continue;
                    }
                    '.' => {
                        *pos += 1;
                        Atom::Any { dot_all: flags.dot_all }
                    }
                    '(' => {
                        *pos += 1;
                        let mut sub_flags = flags;
                        let mut flag_only = false;
                        if *pos < chars.len() && chars[*pos] == '?' {
                            *pos += 1;
                            if *pos < chars.len() && chars[*pos] == ':' {
                                *pos += 1;
                            } else if *pos + 1 < chars.len()
                                && chars[*pos] == 'P'
                                && chars[*pos + 1] == '<'
                            {
                                *pos += 2;
                                let nstart = *pos;
                                while *pos < chars.len() && chars[*pos] != '>' {
                                    *pos += 1;
                                }
                                if *pos >= chars.len() || *pos == nstart {
                                    return Err("invalid named capture group '(?P<...)'".to_string());
                                }
                                *pos += 1;
                            } else {
                                let mut neg_f = false;
                                let mut saw_flag = false;
                                while *pos < chars.len()
                                    && chars[*pos] != ':'
                                    && chars[*pos] != ')'
                                {
                                    match chars[*pos] {
                                        '-' if !neg_f => neg_f = true,
                                        'i' => {
                                            sub_flags.ci = !neg_f;
                                            saw_flag = true;
                                        }
                                        's' => {
                                            sub_flags.dot_all = !neg_f;
                                            saw_flag = true;
                                        }
                                        'm' => {
                                            sub_flags.multi_line = !neg_f;
                                            saw_flag = true;
                                        }
                                        'U' => {
                                            saw_flag = true;
                                        }
                                        other => {
                                            return Err(format!(
                                                "unsupported RE2 group/flag syntax '(?...{other})'; simplify k or omit ::subtest"
                                            ));
                                        }
                                    }
                                    *pos += 1;
                                }
                                if !saw_flag || *pos >= chars.len() {
                                    return Err("invalid RE2 flag group".to_string());
                                }
                                if chars[*pos] == ')' {
                                    *pos += 1;
                                    flags = sub_flags;
                                    flag_only = true;
                                } else {
                                    *pos += 1; // consume ':'
                                }
                            }
                        }
                        if flag_only {
                            continue;
                        }
                        let sub = parse_alts(chars, pos, true, depth + 1, sub_flags)?;
                        if *pos >= chars.len() || chars[*pos] != ')' {
                            return Err("unclosed '('".to_string());
                        }
                        *pos += 1;
                        Atom::Group(sub)
                    }
                    '[' => {
                        *pos += 1;
                        let neg = if *pos < chars.len() && chars[*pos] == '^' {
                            *pos += 1;
                            true
                        } else {
                            false
                        };
                        let parse_class_single = |chars: &[char],
                                                  p: &mut usize|
                         -> std::result::Result<Option<char>, String> {
                            if *p >= chars.len() {
                                return Err("unclosed '['".to_string());
                            }
                            if chars[*p] == '\\' {
                                *p += 1;
                                if *p >= chars.len() {
                                    return Err("trailing '\\' in character class".to_string());
                                }
                                let ec = chars[*p];
                                *p += 1;
                                match ec {
                                    'd' | 'D' | 'w' | 'W' | 's' | 'S' => Ok(None),
                                    'x' => Ok(Some(parse_hex_escape(chars, p)?)),
                                    'a' => Ok(Some('\x07')),
                                    'f' => Ok(Some('\x0c')),
                                    'v' => Ok(Some('\x0b')),
                                    't' => Ok(Some('\t')),
                                    'n' => Ok(Some('\n')),
                                    'r' => Ok(Some('\r')),
                                    c if c.is_ascii_alphabetic() || matches!(c, '0'..='7') => {
                                        Err(format!("unsupported or invalid RE2 character-class escape '\\{c}'"))
                                    }
                                    other => Ok(Some(other)),
                                }
                            } else {
                                let ch = chars[*p];
                                *p += 1;
                                Ok(Some(ch))
                            }
                        };
                        let mut items = Vec::new();
                        let mut first = true;
                        let mut closed = false;
                        while *pos < chars.len() {
                            if !first && chars[*pos] == ']' {
                                *pos += 1;
                                closed = true;
                                break;
                            }
                            first = false;
                            if *pos + 1 < chars.len() && chars[*pos] == '[' && chars[*pos + 1] == ':' {
                                let mut p_end = *pos + 2;
                                while p_end + 1 < chars.len()
                                    && !(chars[p_end] == ':' && chars[p_end + 1] == ']')
                                {
                                    p_end += 1;
                                }
                                if p_end + 1 >= chars.len() {
                                    return Err("unclosed RE2 POSIX character class '[:...:]'".to_string());
                                }
                                let raw_name: String = chars[*pos + 2..p_end].iter().collect();
                                let (pneg, cname) = match raw_name.strip_prefix('^') {
                                    Some(rest) => (true, rest),
                                    None => (false, raw_name.as_str()),
                                };
                                let Some(pranges) = posix_class_ranges(cname) else {
                                    return Err(format!("unknown RE2 POSIX character class '[:{raw_name}:]'"));
                                };
                                if pneg {
                                    items.push(ClassItem::NegRanges(pranges));
                                } else {
                                    for (lo, hi) in pranges {
                                        items.push(ClassItem::Range(lo, hi));
                                    }
                                }
                                *pos = p_end + 2;
                                continue;
                            }
                            let esc_peek = if chars[*pos] == '\\' && *pos + 1 < chars.len() {
                                Some(chars[*pos + 1])
                            } else {
                                None
                            };
                            let start_opt = parse_class_single(chars, pos)?;
                            let Some(start_ch) = start_opt else {
                                // Shorthand class (`\d`, `\D`, `\w`, `\W`, `\s`, `\S`)
                                match esc_peek {
                                    Some('d') => items.push(ClassItem::Range('0', '9')),
                                    Some('D') => items.push(ClassItem::NegRanges(vec![('0', '9')])),
                                    Some('w') => {
                                        for (lo, hi) in [('0', '9'), ('a', 'z'), ('A', 'Z'), ('_', '_')] {
                                            items.push(ClassItem::Range(lo, hi));
                                        }
                                    }
                                    Some('W') => items.push(ClassItem::NegRanges(vec![
                                        ('0', '9'),
                                        ('a', 'z'),
                                        ('A', 'Z'),
                                        ('_', '_'),
                                    ])),
                                    Some('s') => {
                                        for (lo, hi) in [
                                            (' ', ' '),
                                            ('\t', '\t'),
                                            ('\n', '\n'),
                                            ('\x0c', '\x0c'),
                                            ('\r', '\r'),
                                        ] {
                                            items.push(ClassItem::Range(lo, hi));
                                        }
                                    }
                                    Some('S') => items.push(ClassItem::NegRanges(vec![
                                        (' ', ' '),
                                        ('\t', '\t'),
                                        ('\n', '\n'),
                                        ('\x0c', '\x0c'),
                                        ('\r', '\r'),
                                    ])),
                                    _ => {}
                                }
                                continue;
                            };
                            if *pos + 1 < chars.len() && chars[*pos] == '-' && chars[*pos + 1] != ']' {
                                *pos += 1;
                                let Some(end_ch) = parse_class_single(chars, pos)? else {
                                    return Err("invalid character-class range endpoint".to_string());
                                };
                                if end_ch < start_ch {
                                    return Err(format!(
                                        "invalid reversed character-class range '{start_ch}-{end_ch}'"
                                    ));
                                }
                                items.push(ClassItem::Range(start_ch, end_ch));
                            } else {
                                items.push(ClassItem::Range(start_ch, start_ch));
                            }
                        }
                        if !closed {
                            return Err("unclosed '['".to_string());
                        }
                        Atom::Class { neg, ci: flags.ci, items }
                    }
                    '\\' => {
                        *pos += 1;
                        if *pos >= chars.len() {
                            return Err("trailing '\\'".to_string());
                        }
                        let ec = chars[*pos];
                        *pos += 1;
                        match ec {
                            'd' => Atom::Class {
                                neg: false,
                                ci: false,
                                items: vec![ClassItem::Range('0', '9')],
                            },
                            'D' => Atom::Class {
                                neg: true,
                                ci: false,
                                items: vec![ClassItem::Range('0', '9')],
                            },
                            'w' => Atom::Class {
                                neg: false,
                                ci: false,
                                items: vec![
                                    ClassItem::Range('0', '9'),
                                    ClassItem::Range('a', 'z'),
                                    ClassItem::Range('A', 'Z'),
                                    ClassItem::Range('_', '_'),
                                ],
                            },
                            'W' => Atom::Class {
                                neg: true,
                                ci: false,
                                items: vec![
                                    ClassItem::Range('0', '9'),
                                    ClassItem::Range('a', 'z'),
                                    ClassItem::Range('A', 'Z'),
                                    ClassItem::Range('_', '_'),
                                ],
                            },
                            's' => Atom::Class {
                                neg: false,
                                ci: false,
                                items: vec![
                                    ClassItem::Range(' ', ' '),
                                    ClassItem::Range('\t', '\t'),
                                    ClassItem::Range('\n', '\n'),
                                    ClassItem::Range('\x0c', '\x0c'),
                                    ClassItem::Range('\r', '\r'),
                                ],
                            },
                            'S' => Atom::Class {
                                neg: true,
                                ci: false,
                                items: vec![
                                    ClassItem::Range(' ', ' '),
                                    ClassItem::Range('\t', '\t'),
                                    ClassItem::Range('\n', '\n'),
                                    ClassItem::Range('\x0c', '\x0c'),
                                    ClassItem::Range('\r', '\r'),
                                ],
                            },
                            'a' => Atom::Lit { ch: '\x07', ci: flags.ci },
                            'f' => Atom::Lit { ch: '\x0c', ci: flags.ci },
                            'v' => Atom::Lit { ch: '\x0b', ci: flags.ci },
                            't' => Atom::Lit { ch: '\t', ci: flags.ci },
                            'n' => Atom::Lit { ch: '\n', ci: flags.ci },
                            'r' => Atom::Lit { ch: '\r', ci: flags.ci },
                            'x' => Atom::Lit { ch: parse_hex_escape(chars, pos)?, ci: flags.ci },
                            c if c.is_ascii_alphabetic() || matches!(c, '0'..='7') => {
                                return Err(format!(
                                    "unsupported or invalid RE2 escape '\\{c}'; simplify k or omit ::subtest"
                                ));
                            }
                            other => Atom::Lit { ch: other, ci: flags.ci },
                        }
                    }
                    '*' | '+' | '?' | '{' => {
                        return Err(format!("unexpected quantifier '{c}'"));
                    }
                    other => {
                        *pos += 1;
                        Atom::Lit { ch: other, ci: flags.ci }
                    }
                };
                // Parse optional quantifier (`*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}`)
                let (min_r, max_r) = if *pos < chars.len() {
                    match chars[*pos] {
                        '*' => {
                            *pos += 1;
                            if *pos < chars.len() && chars[*pos] == '?' {
                                *pos += 1;
                            }
                            (0, None)
                        }
                        '+' => {
                            *pos += 1;
                            if *pos < chars.len() && chars[*pos] == '?' {
                                *pos += 1;
                            }
                            (1, None)
                        }
                        '?' => {
                            *pos += 1;
                            if *pos < chars.len() && chars[*pos] == '?' {
                                *pos += 1;
                            }
                            (0, Some(1))
                        }
                        '{' => {
                            let qstart = *pos + 1;
                            let mut qend = qstart;
                            while qend < chars.len() && chars[qend] != '}' {
                                qend += 1;
                            }
                            if qend >= chars.len() {
                                return Err("unclosed '{'".to_string());
                            }
                            let inner: String = chars[qstart..qend].iter().collect();
                            let parsed_q = if let Some((a, b)) = inner.split_once(',') {
                                let lo: usize = a.trim().parse().map_err(|_| format!("invalid quantifier '{{{inner}}}'"))?;
                                let hi: Option<usize> = if b.trim().is_empty() {
                                    None
                                } else {
                                    let h: usize = b.trim().parse().map_err(|_| format!("invalid quantifier '{{{inner}}}'"))?;
                                    if h < lo {
                                        return Err(format!("invalid range '{{{inner}}}'"));
                                    }
                                    Some(h)
                                };
                                (lo, hi)
                            } else {
                                let n: usize = inner.trim().parse().map_err(|_| format!("invalid quantifier '{{{inner}}}'"))?;
                                (n, Some(n))
                            };
                            *pos = qend + 1;
                            if *pos < chars.len() && chars[*pos] == '?' {
                                *pos += 1;
                            }
                            parsed_q
                        }
                        _ => (1, Some(1)),
                    }
                } else {
                    (1, Some(1))
                };
                seq.push((atom, min_r, max_r));
            }
            alts.push(seq);
            if *pos < chars.len() && chars[*pos] == '|' {
                *pos += 1;
                continue;
            }
            break;
        }
        Ok(alts)
    }

    /// Returns the RE2 / Go `unicode.SimpleFold` equivalence class for `ch`, covering ASCII case
    /// pairs, single-char Unicode uppercase/lowercase mappings, AND multi-way simple-fold rings
    /// (e.g. `Σ`/`σ`/`ς`, `K`/`k`/`K`, `S`/`s`/`ſ`, `Β`/`β`/`ϐ`, `Μ`/`μ`/`µ`, `Å`/`å`/`Å`).
    pub(super) fn re2_simple_fold_equivalents(ch: char) -> Vec<char> {
        let mut eq = vec![ch];
        let mut push_uniq = |c: char| {
            if !eq.contains(&c) {
                eq.push(c);
            }
        };
        if ch.is_ascii_alphabetic() {
            push_uniq(ch.to_ascii_lowercase());
            push_uniq(ch.to_ascii_uppercase());
            if matches!(ch, 's' | 'S') {
                push_uniq('\u{017F}'); // ſ
            } else if matches!(ch, 'k' | 'K') {
                push_uniq('\u{212A}'); // K
            }
            return eq;
        }
        if matches!(ch, '\u{0130}' | '\u{0131}') {
            // Go `unicode.SimpleFold` uses Unicode CaseFolding `C` + `S` (not Turkic `T`), so
            // `İ` (U+0130) and `ı` (U+0131) are singletons and NEVER fold to ASCII `I`/`i`!
            return eq;
        }
        // Complete Go `unicode.SimpleFold` multi-way & asymmetric fold equivalence classes across
        // Unicode (covering `ß`/`ẞ`, `S`/`s`/`ſ`, `K`/`k`/`K`, `Ω`/`ω`/`Ω`, Greek/Cyrillic/Latin
        // titlecase/compat rings, and Greek extended titlecase pairs `0x1F80..=0x1FAF`).
        const MULTI_RINGS: &[&[char]] = &[
            &['S', 's', '\u{017F}'],
            &['K', 'k', '\u{212A}'],
            &['\u{00B5}', '\u{039C}', '\u{03BC}'], // µ / Μ / μ
            &['\u{00C5}', '\u{00E5}', '\u{212B}'], // Å / å / Å
            &['\u{00DF}', '\u{1E9E}'],             // ß / ẞ
            &['\u{01C4}', '\u{01C5}', '\u{01C6}'], // Ǆ / ǅ / ǆ
            &['\u{01C7}', '\u{01C8}', '\u{01C9}'], // Ǉ / ǈ / ǉ
            &['\u{01CA}', '\u{01CB}', '\u{01CC}'], // Ǌ / ǋ / ǌ
            &['\u{01F1}', '\u{01F2}', '\u{01F3}'], // Ǳ / ǲ / ǳ
            &['\u{0345}', '\u{0399}', '\u{03B9}', '\u{1FBE}'], // ͅ / Ι / ι / ι
            &['\u{0392}', '\u{03B2}', '\u{03D0}'], // Β / β / ϐ
            &['\u{0395}', '\u{03B5}', '\u{03F5}'], // Ε / ε / ϵ
            &['\u{0398}', '\u{03B8}', '\u{03D1}', '\u{03F4}'], // Θ / θ / ϑ / ϴ
            &['\u{039A}', '\u{03BA}', '\u{03F0}'], // Κ / κ / ϰ
            &['\u{03A0}', '\u{03C0}', '\u{03D6}'], // Π / π / ϖ
            &['\u{03A1}', '\u{03C1}', '\u{03F1}'], // Ρ / ρ / ϱ
            &['\u{03A3}', '\u{03C3}', '\u{03C2}'], // Σ / σ / ς
            &['\u{03A6}', '\u{03C6}', '\u{03D5}'], // Φ / φ / ϕ
            &['\u{03A9}', '\u{03C9}', '\u{2126}'], // Ω / ω / Ω
            &['\u{0412}', '\u{0432}', '\u{1C80}'], // В / в / ᲀ
            &['\u{0414}', '\u{0434}', '\u{1C81}'], // Д / д / ᲁ
            &['\u{041E}', '\u{043E}', '\u{1C82}'], // О / о / ᲂ
            &['\u{0421}', '\u{0441}', '\u{1C83}'], // С / с / ᲃ
            &['\u{0422}', '\u{0442}', '\u{1C84}', '\u{1C85}'], // Т / т / ᲄ / ᲅ
            &['\u{042A}', '\u{044A}', '\u{1C86}'], // Ъ / ъ / ᲆ
            &['\u{0462}', '\u{0463}', '\u{1C87}'], // Ѣ / ѣ / ᲇ
            &['\u{1C88}', '\u{A64A}', '\u{A64B}'], // ᲈ / Ꙋ / ꙋ
            &['\u{1E60}', '\u{1E61}', '\u{1E9B}'], // Ṡ / ṡ / ẛ
            &['\u{1FB3}', '\u{1FBC}'],             // ᾳ / ᾼ
            &['\u{1FC3}', '\u{1FCC}'],             // ῃ / ῌ
            &['\u{1FF3}', '\u{1FFC}'],             // ῳ / ῼ
        ];
        for ring in MULTI_RINGS {
            if ring.contains(&ch) {
                for &r in *ring {
                    push_uniq(r);
                }
                return eq;
            }
        }
        // Contiguous Greek extended prosgegrammeni/ypogegrammeni titlecase blocks:
        // 0x1F80..=0x1F87 <-> 0x1F88..=0x1F8F, 0x1F90..=0x1F97 <-> 0x1F98..=0x1F9F, 0x1FA0..=0x1FA7 <-> 0x1FA8..=0x1FAF.
        let cp = ch as u32;
        if (0x1F80..=0x1FAF).contains(&cp) {
            let pair_cp = if (cp & 0x08) == 0 { cp + 8 } else { cp - 8 };
            if let Some(pc) = char::from_u32(pair_cp) {
                push_uniq(pc);
            }
            return eq;
        }
        let mut lo_it = ch.to_lowercase();
        if let (Some(lc), None) = (lo_it.next(), lo_it.next()) {
            push_uniq(lc);
        }
        let mut up_it = ch.to_uppercase();
        if let (Some(uc), None) = (up_it.next(), up_it.next()) {
            push_uniq(uc);
        }
        eq
    }

    pub(super) fn char_in_range(ch: char, lo: char, hi: char, ci: bool) -> bool {
        if ch >= lo && ch <= hi {
            return true;
        }
        if ci {
            for eq in re2_simple_fold_equivalents(ch) {
                if eq >= lo && eq <= hi {
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn match_atom_once(
        atom: &Atom,
        text: &[char],
        idx: usize,
        budget: &mut usize,
    ) -> std::result::Result<Vec<usize>, String> {
        if *budget == 0 {
            return Err("Go subtest regex evaluation budget exceeded".to_string());
        }
        *budget -= 1;
        match atom {
            Atom::Start { multi_line } => {
                let ok = idx == 0 || (*multi_line && idx > 0 && text[idx - 1] == '\n');
                Ok(if ok { vec![idx] } else { Vec::new() })
            }
            Atom::End { multi_line } => {
                let ok = idx == text.len() || (*multi_line && idx < text.len() && text[idx] == '\n');
                Ok(if ok { vec![idx] } else { Vec::new() })
            }
            Atom::Any { dot_all } => Ok(if idx < text.len() && (*dot_all || text[idx] != '\n') {
                vec![idx + 1]
            } else {
                Vec::new()
            }),
            Atom::Lit { ch, ci } => {
                if idx < text.len() {
                    let t = text[idx];
                    let hit = if *ci {
                        t == *ch || re2_simple_fold_equivalents(t).contains(ch)
                    } else {
                        t == *ch
                    };
                    Ok(if hit { vec![idx + 1] } else { Vec::new() })
                } else {
                    Ok(Vec::new())
                }
            }
            Atom::Class { neg, ci, items } => {
                if idx < text.len() {
                    let ch = text[idx];
                    let hit = items.iter().any(|item| match item {
                        ClassItem::Range(lo, hi) => char_in_range(ch, *lo, *hi, *ci),
                        ClassItem::NegRanges(rs) => {
                            !rs.iter().any(|&(lo, hi)| char_in_range(ch, lo, hi, *ci))
                        }
                    });
                    Ok(if hit ^ neg { vec![idx + 1] } else { Vec::new() })
                } else {
                    Ok(Vec::new())
                }
            }
            Atom::Group(alts) => match_alts(alts, text, idx, budget),
        }
    }

    // Iterative Thompson state-set simulation over `seq`: O(len(seq) * len(text)) polynomial time
    // with ZERO per-term stack recursion (prevents stack overflow on long flat regexes like 10,000
    // `a?` terms AND prevents 2^N backtracking blowup on `a?^30`!).
    pub(super) fn match_seq_iter(
        seq: &Seq,
        text: &[char],
        start_idx: usize,
        budget: &mut usize,
    ) -> std::result::Result<Vec<usize>, String> {
        let mut current: BTreeSet<usize> = BTreeSet::from([start_idx]);
        for (atom, min_r, max_r) in seq {
            if current.is_empty() {
                break;
            }
            let mut next_set: BTreeSet<usize> = BTreeSet::new();
            for &pos0 in &current {
                let mut frontier: BTreeSet<usize> = BTreeSet::from([pos0]);
                let limit = max_r.unwrap_or(text.len().saturating_sub(pos0) + 1);
                for rep in 0..=limit {
                    if rep >= *min_r {
                        next_set.extend(frontier.iter().copied());
                    }
                    if rep == limit || frontier.is_empty() {
                        break;
                    }
                    let mut step_next = BTreeSet::new();
                    for &p in &frontier {
                        for nxt in match_atom_once(atom, text, p, budget)? {
                            if nxt > p || rep < *min_r {
                                step_next.insert(nxt);
                            }
                        }
                    }
                    frontier = step_next;
                }
            }
            current = next_set;
        }
        Ok(current.into_iter().collect())
    }

    pub(super) fn match_alts(
        alts: &Alts,
        text: &[char],
        idx: usize,
        budget: &mut usize,
    ) -> std::result::Result<Vec<usize>, String> {
        let mut ends = BTreeSet::new();
        for seq in alts {
            for e in match_seq_iter(seq, text, idx, budget)? {
                ends.insert(e);
            }
        }
        Ok(ends.into_iter().collect())
    }

    let pat_chars: Vec<char> = pattern.chars().collect();
    if pat_chars.len() > 4096 {
        return Err("Go subtest regex exceeds maximum length".to_string());
    }
    let mut pos = 0usize;
    let alts = parse_alts(&pat_chars, &mut pos, false, 0, Re2Flags::default())?;
    if pos != pat_chars.len() {
        return Err("unexpected trailing characters in regex".to_string());
    }
    let text_chars: Vec<char> = text.chars().collect();
    let mut budget = 200_000usize;
    // Go `-run` uses unanchored substring search per `/` segment unless `^`/`$` are specified.
    for start in 0..=text_chars.len() {
        if !match_alts(&alts, &text_chars, start, &mut budget)?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Splits a Go `-run` pattern matching Go 1.20+ `testing.splitRegexp`:
/// 1. Split top-level `|` alternations (where `paren_depth == 0 && !in_class` and not escaped) so
///    `TestA/x|TestB/y` becomes TWO independent branches `[["TestA", "x"], ["TestB", "y"]]`!
/// 2. Within each alternation branch, split on `/` ONLY when `paren_depth == 0 && !in_class` so
///    `TestFoo/[/a]` stays `["TestFoo", "[/a]"]`, `TestFoo/[[:alpha:]|0]` stays intact (handling
///    nested POSIX `[:...:]` classes and leading `[]...]` / `[^]...]` literal `]`), and
///    `TestFoo/(a/b)` stays `["TestFoo", "(a/b)"]`!
pub(super) fn split_go_run_alternations(pattern: &str) -> Vec<Vec<&str>> {
    pub(super) fn split_delim(s: &str, delim: u8) -> Vec<&str> {
        let mut out = Vec::new();
        let bytes = s.as_bytes();
        let mut start = 0usize;
        let mut in_class = false;
        let mut class_first = false;
        let mut in_posix_delim: Option<u8> = None;
        let mut paren_depth = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 2;
                class_first = false;
                continue;
            }
            if !in_class && bytes[i] == b'[' {
                in_class = true;
                class_first = true;
                i += 1;
                if i < bytes.len() && bytes[i] == b'^' {
                    i += 1;
                }
                continue;
            }
            if in_class {
                if let Some(pd) = in_posix_delim {
                    if bytes[i] == pd && i + 1 < bytes.len() && bytes[i + 1] == b']' {
                        in_posix_delim = None;
                        i += 2;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                if bytes[i] == b'['
                    && i + 1 < bytes.len()
                    && matches!(bytes[i + 1], b':' | b'.' | b'=')
                {
                    in_posix_delim = Some(bytes[i + 1]);
                    class_first = false;
                    i += 2;
                    continue;
                }
                if bytes[i] == b']' && !class_first {
                    in_class = false;
                }
                class_first = false;
                i += 1;
                continue;
            }
            if bytes[i] == b'(' {
                paren_depth += 1;
            } else if bytes[i] == b')' {
                paren_depth = paren_depth.saturating_sub(1);
            } else if paren_depth == 0 && bytes[i] == delim {
                out.push(&s[start..i]);
                start = i + 1;
            }
            i += 1;
        }
        out.push(&s[start..]);
        out
    }
    split_delim(pattern, b'|')
        .into_iter()
        .map(|branch| split_delim(branch, b'/'))
        .collect()
}

pub(super) fn go_top_list_regex_for_branches(branches: &[Vec<String>]) -> String {
    let tops: Vec<String> = branches
        .iter()
        .filter_map(|b| b.first().map(String::as_str))
        .map(|t| format!("(?:{t})"))
        .collect();
    if tops.is_empty() {
        String::new()
    } else if tops.len() == 1 {
        branches[0][0].clone()
    } else {
        tops.join("|")
    }
}

/// Checks whether a Go test file or node selector batch conflicts with an explicit `-run` flag in `extra_args`.
pub(super) fn extra_args_have_go_run_flag(extra_args: &[String]) -> bool {
    extra_args.iter().any(|a| {
        a == "-run"
            || a.starts_with("-run=")
            || a == "--run"
            || a.starts_with("--run=")
            || a == "-test.run"
            || a.starts_with("-test.run=")
            || a == "--test.run"
            || a.starts_with("--test.run=")
    })
}


