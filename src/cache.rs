use crate::ast_graph::sha256_hex;
use crate::catalog;
use crate::policy::MemoryPolicy;
use crate:: Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
    Skipped,
    Error,
}

impl TestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Error => "error",
        }
    }

    pub fn is_green(self) -> bool {
        matches!(self, Self::Passed | Self::Skipped)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedTestRecord {
    pub test_file: String,
    pub dep_hash: String,
    pub outcome: TestOutcome,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub duration_ms: u64,
    pub failed_nodeids: Vec<String>,
    pub output_snippet: String,
    pub updated_unix_ms: u64,
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RootCache {
    pub root: PathBuf,
    pub latest_by_file: BTreeMap<String, CachedTestRecord>,
    pub history_by_key: BTreeMap<String, CachedTestRecord>,
    pub total_saved_ms: u64,
    pub total_cache_hits: u64,
    pub total_delta_runs: u64,
    pub total_background_runs: u64,
}

impl RootCache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            latest_by_file: BTreeMap::new(),
            history_by_key: BTreeMap::new(),
            total_saved_ms: 0,
            total_cache_hits: 0,
            total_delta_runs: 0,
            total_background_runs: 0,
        }
    }

    fn key(test_file: &str, dep_hash: &str) -> String {
        format!("{test_file}@{dep_hash}")
    }

    pub fn get_matching(&self, test_file: &str, dep_hash: &str) -> Option<&CachedTestRecord> {
        if let Some(rec) = self.latest_by_file.get(test_file)
            && rec.dep_hash == dep_hash
        {
            return Some(rec);
        }
        self.history_by_key.get(&Self::key(test_file, dep_hash))
    }

    pub fn insert(&mut self, record: CachedTestRecord, policy: &MemoryPolicy) {
        let key = Self::key(&record.test_file, &record.dep_hash);
        self.latest_by_file
            .insert(record.test_file.clone(), record.clone());
        self.history_by_key.insert(key, record);
        while self.history_by_key.len() > policy.max_cached_results {
            if let Some(oldest_key) = self
                .history_by_key
                .iter()
                .min_by_key(|(_, r)| r.updated_unix_ms)
                .map(|(k, _)| k.clone())
            {
                self.history_by_key.remove(&oldest_key);
            } else {
                break;
            }
        }
    }

    pub fn invalidate_file(&mut self, test_file: &str) -> usize {
        let mut removed = usize::from(self.latest_by_file.remove(test_file).is_some());
        let prefix = format!("{test_file}@");
        let keys: Vec<String> = self
            .history_by_key
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in keys {
            if self.history_by_key.remove(&k).is_some() {
                removed += 1;
            }
        }
        removed
    }

    pub fn invalidate_all(&mut self) -> usize {
        let count = self.latest_by_file.len() + self.history_by_key.len();
        self.latest_by_file.clear();
        self.history_by_key.clear();
        count
    }

    fn storage_path(root: &Path) -> Result<PathBuf> {
        let root_hash = sha256_hex(root.to_string_lossy().as_bytes());
        Ok(catalog::cache_dir()?.join(format!("{root_hash}.json")))
    }

    pub fn load_or_new(root: &Path) -> Self {
        let Ok(path) = Self::storage_path(root) else {
            return Self::new(root.to_path_buf());
        };
        let Ok(file) = std::fs::File::open(&path) else {
            return Self::new(root.to_path_buf());
        };
        let mut bytes = Vec::new();
        if file.take(16 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
            return Self::new(root.to_path_buf());
        }
        serde_json::from_slice::<Self>(&bytes).unwrap_or_else(|_| Self::new(root.to_path_buf()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::storage_path(&self.root)?;
        let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let bytes = serde_json::to_vec(self)?;
        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp_path, &path)?;
        Ok(())
    }
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
