use super::*;

pub(super) const AST_SNAPSHOT_SCHEMA: u32 = 174;
pub(super) const MAX_AST_SNAPSHOT_BYTES: usize = 48 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub(super) struct PersistedEdgeGraph {
    pub(super) file_count: usize,
    pub(super) forward_adj: Vec<Vec<u32>>,
    pub(super) reverse_adj: Vec<Vec<u32>>,
    pub(super) conftest_for_test: BTreeMap<String, Vec<String>>,
    pub(super) transitive_dep_ids_for_test: BTreeMap<String, Vec<u32>>,
    pub(super) local_python_roots: BTreeSet<String>,
    pub(super) runtime_deps_for_test: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct PersistedAstSnapshot {
    pub(super) schema: u32,
    pub(super) root: String,
    pub(super) pythonpath_env_bytes: Option<Vec<u8>>,
    pub(super) gowork_fingerprint: String,
    pub(super) test_file_patterns: Vec<String>,
    pub(super) test_function_patterns: Vec<String>,
    pub(super) testpaths: Vec<String>,
    pub(super) testpaths_fallback_active: bool,
    pub(super) nodes: Vec<FileNode>,
    pub(super) edges: Option<PersistedEdgeGraph>,
}

/// Resolves effective `GOWORK` honoring process `GOWORK` first, then persisted `go env -w` (`GOENV`).
pub fn effective_gowork_value() -> Option<String> {
    if let Ok(v) = std::env::var("GOWORK") {
        return Some(v);
    }
    read_effective_goenv_var("GOWORK")
}

pub(super) fn compute_gowork_snapshot_fingerprint(root: &Path, indexed_go_mod_dirs: &[String]) -> String {
    let mut h = Sha256::new();
    let eff_gowork = effective_gowork_value();
    match eff_gowork.as_deref() {
        Some(v) => {
            h.update(b"GOWORK=1:");
            h.update(v.as_bytes());
        }
        None => h.update(b"GOWORK=0"),
    }
    h.update(b"\0");
    match std::env::var_os("GOENV") {
        Some(v) => {
            h.update(b"GOENV=1:");
            h.update(v.as_encoded_bytes());
        }
        None => h.update(b"GOENV=0"),
    }
    h.update(b"\0");
    // Collect EVERY effective `go.work` file discovered for `root` AND every indexed Go module
    // directory (including persisted `GOENV` `GOWORK=...` AND nested ignored `go.work` files!) so
    // editing/creating/deleting ANY active `go.work` immediately invalidates persisted snapshot edges.
    let mut gowork_paths: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    if let Some(ref gw) = eff_gowork {
        let t = gw.trim();
        if t == "off" {
            return hex_encode(&h.finalize());
        }
        if !t.is_empty() && t != "auto" {
            let p = Path::new(t);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            };
            if abs.is_file() {
                gowork_paths.insert(abs.canonicalize().unwrap_or(abs));
            }
            for canon in &gowork_paths {
                h.update(b"WORKFILE:");
                h.update(canon.to_string_lossy().as_bytes());
                h.update(b"\0");
                if let Ok(bytes) = std::fs::read(canon) {
                    h.update(&bytes);
                }
            }
            return hex_encode(&h.finalize());
        }
    }
    let mut search_roots: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    search_roots.insert(root.to_path_buf());
    for rel_dir in indexed_go_mod_dirs {
        if !rel_dir.is_empty() {
            search_roots.insert(root.join(rel_dir));
        }
    }
    for start_dir in search_roots {
        let mut cur: Option<&Path> = Some(&start_dir);
        while let Some(dir) = cur {
            let cand = dir.join("go.work");
            if cand.is_file() {
                gowork_paths.insert(cand.canonicalize().unwrap_or(cand));
                break;
            }
            cur = dir.parent();
        }
    }
    for canon in &gowork_paths {
        h.update(b"WORKFILE:");
        h.update(canon.to_string_lossy().as_bytes());
        h.update(b"\0");
        if let Ok(bytes) = std::fs::read(canon) {
            h.update(&bytes);
        }
    }
    hex_encode(&h.finalize())
}

pub(super) type LoadedAstSnapshot = (BTreeMap<String, FileNode>, Option<PersistedEdgeGraph>);

pub(super) fn ast_snapshot_path(root: &Path) -> Result<std::path::PathBuf> {
    let root_hash = sha256_hex(root.to_string_lossy().as_bytes());
    Ok(crate::catalog::cache_dir()?.join(format!("{root_hash}.v{AST_SNAPSHOT_SCHEMA}.ast.bin")))
}

pub(super) fn load_persisted_ast_nodes(
    root: &Path,
    patterns: &[String],
    fn_patterns: &[String],
    testpaths: &[String],
    testpaths_fallback_active: bool,
) -> LoadedAstSnapshot {
    use std::os::unix::fs::OpenOptionsExt;

    let Ok(path) = ast_snapshot_path(root) else {
        return (BTreeMap::new(), None);
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return (BTreeMap::new(), None);
    };
    let Ok(meta) = file.metadata() else {
        return (BTreeMap::new(), None);
    };
    if usize::try_from(meta.len()).unwrap_or(usize::MAX) > MAX_AST_SNAPSHOT_BYTES {
        return (BTreeMap::new(), None);
    }
    let cap = usize::try_from(meta.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(cap);
    if std::io::Read::take(file, MAX_AST_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_AST_SNAPSHOT_BYTES
    {
        return (BTreeMap::new(), None);
    }
    let Ok(snap) = bincode::deserialize::<PersistedAstSnapshot>(&bytes) else {
        return (BTreeMap::new(), None);
    };
    if snap.schema != AST_SNAPSHOT_SCHEMA
        || snap.root != root.to_string_lossy()
        || snap.test_file_patterns != patterns
        || snap.test_function_patterns != fn_patterns
        || snap.testpaths != testpaths
        || snap.testpaths_fallback_active != testpaths_fallback_active
    {
        return (BTreeMap::new(), None);
    }
    let map: BTreeMap<String, FileNode> = snap
        .nodes
        .into_iter()
        .map(|node| (node.path.clone(), node))
        .collect();
    // If `PYTHONPATH` or effective Go workspace (`GOWORK` / `GOENV` / any ancestor or nested
    // `go.work` content for indexed Go modules) changed since the snapshot was written, keep
    // parsed file ASTs but discard persisted edges so imports/replacements are recomputed cleanly.
    let current_pythonpath =
        std::env::var_os("PYTHONPATH").map(|v| v.as_encoded_bytes().to_vec());
    let go_mod_dirs: Vec<String> = map
        .keys()
        .filter(|p| p.ends_with("/go.mod") || p.ends_with(".go"))
        .filter_map(|p| {
            Path::new(p)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let current_gowork_fp = compute_gowork_snapshot_fingerprint(root, &go_mod_dirs);
    let edges = if snap.pythonpath_env_bytes == current_pythonpath
        && snap.gowork_fingerprint == current_gowork_fp
    {
        snap.edges
    } else {
        None
    };
    (map, edges)
}

pub fn save_persisted_ast_snapshot(root: &Path, graph: &DependencyGraph) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let Ok(path) = ast_snapshot_path(root) else {
        return;
    };
    let edges = if !graph.file_index_to_path.is_empty()
        && graph.file_index_to_path.len() == graph.files.len()
        && graph.forward_adj.len() == graph.files.len()
    {
        Some(PersistedEdgeGraph {
            file_count: graph.files.len(),
            forward_adj: graph.forward_adj.clone(),
            reverse_adj: graph.reverse_adj.clone(),
            conftest_for_test: graph.conftest_for_test.clone(),
            transitive_dep_ids_for_test: graph.transitive_dep_ids_for_test.clone(),
            local_python_roots: graph.local_python_roots.clone(),
            runtime_deps_for_test: graph.runtime_deps_for_test.clone(),
        })
    } else {
        None
    };
    let go_mod_dirs: Vec<String> = graph
        .files
        .keys()
        .filter(|p| p.ends_with("/go.mod") || p.ends_with(".go"))
        .filter_map(|p| {
            Path::new(p)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let snap = PersistedAstSnapshot {
        schema: AST_SNAPSHOT_SCHEMA,
        root: root.to_string_lossy().into_owned(),
        pythonpath_env_bytes: std::env::var_os("PYTHONPATH")
            .map(|v| v.as_encoded_bytes().to_vec()),
        gowork_fingerprint: compute_gowork_snapshot_fingerprint(root, &go_mod_dirs),
        test_file_patterns: graph.test_file_patterns.clone(),
        test_function_patterns: graph.test_function_patterns.clone(),
        testpaths: graph.testpaths.clone(),
        testpaths_fallback_active: graph.testpaths_fallback_active,
        nodes: graph.files.values().cloned().collect(),
        edges,
    };
    let Ok(bytes) = bincode::serialize(&snap) else {
        return;
    };
    if bytes.len() > MAX_AST_SNAPSHOT_BYTES {
        return;
    }
    static SNAPSHOT_TMP_SEQ: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let seq = SNAPSHOT_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("bin.tmp.{}.{seq}", std::process::id()));
    let write_ok = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(&bytes)?;
            f.flush()
        })
        .is_ok();
    if !write_ok || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}


