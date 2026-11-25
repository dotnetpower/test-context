use super::*;

pub(super) struct DaemonMetrics {
    pub(super) total_requests: u64,
    pub(super) error_requests: u64,
    pub(super) test_runs: u64,
    pub(super) total_saved_ms: u64,
    pub(super) total_cached_files: u64,
    pub(super) total_executed_files: u64,
}

impl DaemonMetrics {
    pub(super) fn to_json(&self) -> Value {
        json!({
            "total_requests": self.total_requests,
            "error_requests": self.error_requests,
            "test_runs": self.test_runs,
            "total_saved_ms": self.total_saved_ms,
            "total_cached_files": self.total_cached_files,
            "total_executed_files": self.total_executed_files,
        })
    }
}

pub(super) struct MetricsWriter {
    pub(super) path: PathBuf,
    pub(super) file: Option<std::fs::File>,
    pub(super) written_bytes: u64,
}

impl MetricsWriter {
    pub(super) fn open_for(dir: &Path) -> Option<Self> {
        let path = dir.join("metrics.jsonl");
        let mut writer = Self {
            path,
            file: None,
            written_bytes: 0,
        };
        writer.ensure_open(dir)?;
        Some(writer)
    }

    pub(super) fn ensure_open(&mut self, dir: &Path) -> Option<()> {
        if self.file.is_some() && self.written_bytes < 2 * 1024 * 1024 {
            return Some(());
        }
        self.file = None;
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.file_type().is_symlink() {
                return None;
            }
            if meta.len() >= 2 * 1024 * 1024 {
                let _ = std::fs::rename(&self.path, dir.join("metrics.jsonl.1"));
                self.written_bytes = 0;
            } else {
                self.written_bytes = meta.len();
            }
        } else {
            self.written_bytes = 0;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
            .ok()?;
        self.file = Some(file);
        Some(())
    }
}

pub(super) fn append_metrics_jsonl(entry: &Value) {
    static METRICS_WRITER: OnceLock<Mutex<Option<MetricsWriter>>> = OnceLock::new();
    let Ok(dir) = catalog::state_dir() else {
        return;
    };
    let expected_path = dir.join("metrics.jsonl");
    let Ok(mut line) = serde_json::to_vec(entry) else {
        return;
    };
    line.push(b'\n');
    let slot = METRICS_WRITER.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    let needs_reinit = guard
        .as_ref()
        .is_none_or(|writer| writer.path != expected_path);
    if needs_reinit {
        *guard = MetricsWriter::open_for(&dir);
    }
    if let Some(writer) = guard.as_mut() {
        if writer.ensure_open(&dir).is_none() {
            return;
        }
        if let Some(file) = writer.file.as_mut() {
            if file.write_all(&line).is_ok() {
                writer.written_bytes = writer.written_bytes.saturating_add(line.len() as u64);
            } else {
                writer.file = None;
            }
        }
    }
}


