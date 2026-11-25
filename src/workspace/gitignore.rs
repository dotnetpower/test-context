use super::*;

#[derive(Clone, Debug)]
pub(super) struct GitignoreMatcher {
    pub(super) ignore: GlobSet,
    pub(super) ignore_ranks: Vec<usize>,
    pub(super) dir_only_ignore: GlobSet,
    pub(super) dir_only_ignore_ranks: Vec<usize>,
    pub(super) allow: GlobSet,
    pub(super) allow_ranks: Vec<usize>,
    pub(super) allow_ancestors: BTreeSet<PathBuf>,
}


pub(super) fn compile_root_gitignore(root: &Path, enabled: bool) -> GitignoreMatcher {
    use std::os::unix::fs::OpenOptionsExt;

    let empty = || GitignoreMatcher {
        ignore: GlobSetBuilder::new().build().unwrap_or_default(),
        ignore_ranks: Vec::new(),
        dir_only_ignore: GlobSetBuilder::new().build().unwrap_or_default(),
        dir_only_ignore_ranks: Vec::new(),
        allow: GlobSetBuilder::new().build().unwrap_or_default(),
        allow_ranks: Vec::new(),
        allow_ancestors: BTreeSet::new(),
    };
    if !enabled {
        return empty();
    }
    type GitignoreCacheKey = (PathBuf, Option<crate::ast_graph::FileStamp>);
    static GITIGNORE_CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, (GitignoreCacheKey, GitignoreMatcher)>>,
    > = std::sync::OnceLock::new();
    let path = root.join(".gitignore");
    let stamp = crate::ast_graph::file_stamp(&path);
    let key = (path.clone(), stamp);
    let cache =
        GITIGNORE_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Some((cached_key, matcher)) = cache
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(root)
        && *cached_key == key
    {
        return matcher.clone();
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let m = empty();
            let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
            if guard.len() > 256 {
                guard.clear();
            }
            guard.insert(root.to_path_buf(), (key, m.clone()));
            return m;
        }
        Err(_) => {
            // Do not cache transient open errors (e.g., EACCES/EMFILE) as an empty matcher
            return empty();
        }
    };
    let mut raw_bytes = Vec::new();
    if file.take(256 * 1024).read_to_end(&mut raw_bytes).is_err() {
        return empty();
    }
    let compiled = compile_root_gitignore_from_bytes(&raw_bytes, empty);
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if guard.len() > 256 {
        guard.clear();
    }
    guard.insert(root.to_path_buf(), (key, compiled.clone()));
    compiled
}

pub(super) fn compile_root_gitignore_from_bytes(
    raw_bytes: &[u8],
    empty: fn() -> GitignoreMatcher,
) -> GitignoreMatcher {
    let text = String::from_utf8_lossy(raw_bytes);
    let mut ignore_builder = GlobSetBuilder::new();
    let mut ignore_ranks = Vec::new();
    let mut dir_only_builder = GlobSetBuilder::new();
    let mut dir_only_ranks = Vec::new();
    let mut allow_builder = GlobSetBuilder::new();
    let mut allow_ranks = Vec::new();
    let mut allow_ancestors = BTreeSet::new();

    for (line_idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed_start = line.trim_start();
        if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
            continue;
        }
        let (negated, rest) = if let Some(after_bang) = line.strip_prefix('!') {
            (true, after_bang)
        } else if let Some(escaped) = line.strip_prefix("\\!") {
            (false, escaped)
        } else if let Some(escaped_hash) = line.strip_prefix("\\#") {
            (false, escaped_hash)
        } else {
            (false, line)
        };
        let raw_trimmed = rest.trim();
        let dir_only = raw_trimmed.ends_with('/');
        let cleaned = raw_trimmed.trim_end_matches('/');
        if cleaned.is_empty() || cleaned == "." || cleaned == "**" {
            continue;
        }
        let anchored = cleaned.starts_with('/') || cleaned.contains('/');
        let base = cleaned.trim_start_matches('/');
        if base.is_empty() {
            continue;
        }
        let build_git_glob = |pat: &str| {
            globset::GlobBuilder::new(pat)
                .literal_separator(true)
                .build()
                .ok()
        };
        if negated {
            let mut cur = Path::new(base).parent();
            while let Some(anc) = cur {
                if anc.as_os_str().is_empty() {
                    break;
                }
                let clean_anc: PathBuf = anc
                    .components()
                    .take_while(|c| {
                        let s = c.as_os_str().to_string_lossy();
                        !s.contains(['*', '?', '['])
                    })
                    .collect();
                if !clean_anc.as_os_str().is_empty() {
                    allow_ancestors.insert(clean_anc);
                }
                cur = anc.parent();
            }
            let candidates = if anchored {
                vec![base.to_string(), format!("{base}/**")]
            } else {
                vec![
                    base.to_string(),
                    format!("**/{base}"),
                    format!("{base}/**"),
                    format!("**/{base}/**"),
                ]
            };
            for cand in candidates {
                if let Some(glob) = build_git_glob(&cand) {
                    allow_builder.add(glob);
                    allow_ranks.push(line_idx + 1);
                }
            }
        } else if dir_only {
            let self_dir_cands = if anchored {
                vec![base.to_string()]
            } else {
                vec![base.to_string(), format!("**/{base}")]
            };
            for cand in self_dir_cands {
                if let Some(glob) = build_git_glob(&cand) {
                    dir_only_builder.add(glob);
                    dir_only_ranks.push(line_idx + 1);
                }
            }
            let desc_cands = if anchored {
                vec![format!("{base}/*"), format!("{base}/**/*")]
            } else {
                vec![
                    format!("{base}/*"),
                    format!("{base}/**/*"),
                    format!("**/{base}/*"),
                    format!("**/{base}/**/*"),
                ]
            };
            for cand in desc_cands {
                if let Some(glob) = build_git_glob(&cand) {
                    ignore_builder.add(glob);
                    ignore_ranks.push(line_idx + 1);
                }
            }
        } else {
            let candidates = if anchored {
                vec![base.to_string(), format!("{base}/**")]
            } else {
                vec![
                    base.to_string(),
                    format!("**/{base}"),
                    format!("{base}/**"),
                    format!("**/{base}/**"),
                ]
            };
            for cand in candidates {
                if let Some(glob) = build_git_glob(&cand) {
                    ignore_builder.add(glob);
                    ignore_ranks.push(line_idx + 1);
                }
            }
        }
    }

    let Ok(ignore) = ignore_builder.build() else {
        return empty();
    };
    let Ok(dir_only_ignore) = dir_only_builder.build() else {
        return empty();
    };
    let Ok(allow) = allow_builder.build() else {
        return empty();
    };
    GitignoreMatcher {
        ignore,
        ignore_ranks,
        dir_only_ignore,
        dir_only_ignore_ranks: dir_only_ranks,
        allow,
        allow_ranks,
        allow_ancestors,
    }
}


