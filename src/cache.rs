use crate::Result;
use crate::ast_graph::sha256_hex;
use crate::catalog;
use crate::policy::MemoryPolicy;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
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
pub struct CachedNodeResult {
    pub nodeid: String,
    pub outcome: TestOutcome,
    pub duration_ms: u64,
    pub output_snippet: String,
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
    #[serde(default)]
    pub environment_hash: String,
    #[serde(default)]
    pub tctx_version: String,
    #[serde(default)]
    pub runner_mode: String,
    #[serde(default)]
    pub started_unix_ms: u64,
    #[serde(default)]
    pub completed_unix_ms: u64,
    #[serde(default)]
    pub dynamic_dependencies: Vec<String>,
    #[serde(default)]
    pub dynamic_dependencies_complete: bool,
    #[serde(default)]
    pub node_results: BTreeMap<String, CachedNodeResult>,
}

fn current_cache_schema() -> u32 {
    4
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RootCache {
    #[serde(default = "current_cache_schema")]
    pub schema: u32,
    pub root: PathBuf,
    pub latest_by_file: BTreeMap<String, CachedTestRecord>,
    pub history_by_key: BTreeMap<String, CachedTestRecord>,
    pub total_saved_ms: u64,
    pub total_cache_hits: u64,
    pub total_delta_runs: u64,
    pub total_background_runs: u64,
    #[serde(default)]
    pub quarantined_keys: BTreeSet<String>,
    #[serde(default)]
    pub persistence_error: Option<String>,
    #[serde(skip)]
    pending_events: Vec<CacheWalEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum CacheWalEvent {
    Upsert {
        record: Box<CachedTestRecord>,
    },
    InvalidateFile {
        test_file: String,
    },
    Reset,
    RemoveKey {
        key: String,
    },
    Quarantine {
        key: String,
    },
    Metrics {
        total_saved_ms: u64,
        total_cache_hits: u64,
        total_delta_runs: u64,
        total_background_runs: u64,
    },
}

impl RootCache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            schema: current_cache_schema(),
            root,
            latest_by_file: BTreeMap::new(),
            history_by_key: BTreeMap::new(),
            total_saved_ms: 0,
            total_cache_hits: 0,
            total_delta_runs: 0,
            total_background_runs: 0,
            quarantined_keys: BTreeSet::new(),
            persistence_error: None,
            pending_events: Vec::new(),
        }
    }

    fn key(test_file: &str, dep_hash: &str) -> String {
        format!("{test_file}@{dep_hash}")
    }

    pub fn get_matching(&self, test_file: &str, dep_hash: &str) -> Option<&CachedTestRecord> {
        let key = Self::key(test_file, dep_hash);
        if self.quarantined_keys.contains(&key) {
            return None;
        }
        if let Some(rec) = self.latest_by_file.get(test_file)
            && rec.dep_hash == dep_hash
        {
            return Some(rec);
        }
        self.history_by_key.get(&key)
    }

    pub fn insert(&mut self, record: CachedTestRecord, policy: &MemoryPolicy) {
        if record.outcome == TestOutcome::Skipped {
            return;
        }
        let key = Self::key(&record.test_file, &record.dep_hash);
        if let Some(existing) = self.history_by_key.get(&key)
            && existing.outcome != record.outcome
        {
            self.quarantined_keys.insert(key.clone());
            self.pending_events
                .push(CacheWalEvent::Quarantine { key: key.clone() });
        }
        self.latest_by_file
            .insert(record.test_file.clone(), record.clone());
        self.history_by_key.insert(key, record.clone());
        self.pending_events.push(CacheWalEvent::Upsert {
            record: Box::new(record),
        });
        while self.history_by_key.len() > policy.max_cached_results {
            if let Some(oldest_key) = self
                .history_by_key
                .iter()
                .min_by_key(|(_, r)| r.updated_unix_ms)
                .map(|(k, _)| k.clone())
            {
                self.history_by_key.remove(&oldest_key);
                self.pending_events
                    .push(CacheWalEvent::RemoveKey { key: oldest_key });
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
        self.quarantined_keys
            .retain(|key| !key.starts_with(&prefix));
        self.pending_events.push(CacheWalEvent::InvalidateFile {
            test_file: test_file.to_string(),
        });
        removed
    }

    pub fn invalidate_all(&mut self) -> usize {
        let count = self.latest_by_file.len() + self.history_by_key.len();
        self.latest_by_file.clear();
        self.history_by_key.clear();
        self.quarantined_keys.clear();
        self.pending_events.push(CacheWalEvent::Reset);
        count
    }

    fn storage_path(root: &Path) -> Result<PathBuf> {
        let root_hash = sha256_hex(root.to_string_lossy().as_bytes());
        Ok(catalog::cache_dir()?.join(format!("{root_hash}.json")))
    }

    fn lock_path(root: &Path) -> Result<PathBuf> {
        let root_hash = sha256_hex(root.to_string_lossy().as_bytes());
        Ok(catalog::cache_dir()?.join(format!("{root_hash}.lock")))
    }

    fn wal_path(root: &Path) -> Result<PathBuf> {
        let root_hash = sha256_hex(root.to_string_lossy().as_bytes());
        Ok(
            catalog::cache_dir()?
                .join(format!("{root_hash}.v{}.wal.jsonl", current_cache_schema())),
        )
    }

    pub fn load_or_new(root: &Path) -> Self {
        let Ok(lock_path) = Self::lock_path(root) else {
            return Self::new(root.to_path_buf());
        };
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .ok();
        if let Some(ref lf) = lock_file {
            let _ = lf.lock_shared();
        }
        let mut cache = Self::storage_path(root)
            .ok()
            .and_then(|path| std::fs::File::open(path).ok())
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(16 * 1024 * 1024)
                    .read_to_end(&mut bytes)
                    .ok()
                    .map(|_| bytes)
            })
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|cache| cache.schema == current_cache_schema() && cache.root == root)
            .unwrap_or_else(|| Self::new(root.to_path_buf()));
        cache.pending_events.clear();
        if let Ok(wal_path) = Self::wal_path(root)
            && let Ok(file) = std::fs::File::open(wal_path)
        {
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            let mut total = 0usize;
            while total <= 64 * 1024 * 1024 {
                line.clear();
                let Ok(read) = reader.read_line(&mut line) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                total = total.saturating_add(read);
                if let Ok(event) = serde_json::from_str::<CacheWalEvent>(&line) {
                    cache.apply_wal_event(event);
                }
            }
        }
        cache.pending_events.clear();
        while cache.history_by_key.len() > MemoryPolicy::default().max_cached_results {
            let Some(oldest) = cache
                .history_by_key
                .iter()
                .min_by_key(|(_, record)| record.updated_unix_ms)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.history_by_key.remove(&oldest);
            cache.quarantined_keys.remove(&oldest);
        }
        cache
    }

    pub fn save(&mut self) -> Result<()> {
        let lock_path = Self::lock_path(&self.root)?;
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        lock_file.lock_exclusive()?;
        self.pending_events.push(CacheWalEvent::Metrics {
            total_saved_ms: self.total_saved_ms,
            total_cache_hits: self.total_cache_hits,
            total_delta_runs: self.total_delta_runs,
            total_background_runs: self.total_background_runs,
        });
        let wal_path = Self::wal_path(&self.root)?;
        {
            let mut wal = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&wal_path)?;
            for event in &self.pending_events {
                serde_json::to_writer(&mut wal, event)?;
                wal.write_all(b"\n")?;
            }
            wal.sync_data()?;
        }
        self.pending_events.clear();

        let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        if wal_bytes > 16 * 1024 * 1024 {
            self.compact_snapshot(&wal_path)?;
        }
        Ok(())
    }

    fn apply_wal_event(&mut self, event: CacheWalEvent) {
        match event {
            CacheWalEvent::Upsert { record } => {
                let record = *record;
                let key = Self::key(&record.test_file, &record.dep_hash);
                self.latest_by_file
                    .insert(record.test_file.clone(), record.clone());
                self.history_by_key.insert(key, record);
            }
            CacheWalEvent::InvalidateFile { test_file } => {
                self.latest_by_file.remove(&test_file);
                let prefix = format!("{test_file}@");
                self.history_by_key
                    .retain(|key, _| !key.starts_with(&prefix));
                self.quarantined_keys
                    .retain(|key| !key.starts_with(&prefix));
            }
            CacheWalEvent::Reset => {
                self.latest_by_file.clear();
                self.history_by_key.clear();
                self.quarantined_keys.clear();
            }
            CacheWalEvent::RemoveKey { key } => {
                self.history_by_key.remove(&key);
                self.quarantined_keys.remove(&key);
            }
            CacheWalEvent::Quarantine { key } => {
                self.quarantined_keys.insert(key);
            }
            CacheWalEvent::Metrics {
                total_saved_ms,
                total_cache_hits,
                total_delta_runs,
                total_background_runs,
            } => {
                self.total_saved_ms = total_saved_ms;
                self.total_cache_hits = total_cache_hits;
                self.total_delta_runs = total_delta_runs;
                self.total_background_runs = total_background_runs;
            }
        }
    }

    fn compact_snapshot(&self, wal_path: &Path) -> Result<()> {
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
        let wal = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(wal_path)?;
        wal.sync_all()?;
        Ok(())
    }
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(outcome: TestOutcome) -> CachedTestRecord {
        CachedTestRecord {
            test_file: "tests/test_one.py".to_string(),
            dep_hash: "same-hash".to_string(),
            outcome,
            passed: usize::from(outcome == TestOutcome::Passed),
            failed: usize::from(outcome == TestOutcome::Failed),
            skipped: usize::from(outcome == TestOutcome::Skipped),
            errors: usize::from(outcome == TestOutcome::Error),
            duration_ms: 10,
            failed_nodeids: Vec::new(),
            output_snippet: String::new(),
            updated_unix_ms: now_unix_ms(),
            source: "test".to_string(),
            environment_hash: "env".to_string(),
            tctx_version: env!("CARGO_PKG_VERSION").to_string(),
            runner_mode: "zygote".to_string(),
            started_unix_ms: 1,
            completed_unix_ms: 2,
            dynamic_dependencies: Vec::new(),
            dynamic_dependencies_complete: true,
            node_results: BTreeMap::new(),
        }
    }

    #[test]
    fn quarantines_flaky_same_hash_and_never_caches_all_skipped_file() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = RootCache::new(root.path().to_path_buf());
        let policy = MemoryPolicy::default();
        cache.insert(record(TestOutcome::Passed), &policy);
        assert!(
            cache
                .get_matching("tests/test_one.py", "same-hash")
                .is_some()
        );
        cache.insert(record(TestOutcome::Failed), &policy);
        assert!(
            cache
                .get_matching("tests/test_one.py", "same-hash")
                .is_none()
        );

        let mut skipped = record(TestOutcome::Skipped);
        skipped.dep_hash = "skip-hash".to_string();
        cache.insert(skipped, &policy);
        assert!(
            cache
                .get_matching("tests/test_one.py", "skip-hash")
                .is_none()
        );
    }
}
