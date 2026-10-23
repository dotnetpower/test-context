use crate::Result;
use crate::ast_graph::sha256_hex;
use crate::catalog;
use crate::policy::MemoryPolicy;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Seek, Write};
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
    6
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvocationPattern {
    pub test_files: Vec<String>,
    pub frequency: u64,
    pub last_used_unix_ms: u64,
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
    #[serde(default)]
    pub invocation_patterns: BTreeMap<String, InvocationPattern>,
    #[serde(skip)]
    pending_events: Vec<CacheWalEvent>,
    #[serde(default)]
    wal_sequence: u64,
    #[serde(skip)]
    wal_valid_bytes: u64,
    #[serde(skip)]
    wal_needs_repair: bool,
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
    Invocation {
        key: String,
        pattern: InvocationPattern,
    },
}

#[derive(Serialize, Deserialize)]
struct CacheWalEnvelope {
    sequence: u64,
    checksum: String,
    event: CacheWalEvent,
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
            invocation_patterns: BTreeMap::new(),
            pending_events: Vec::new(),
            wal_sequence: 0,
            wal_valid_bytes: 0,
            wal_needs_repair: false,
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
        let mut record = record;
        if record.outcome == TestOutcome::Skipped || record.skipped > 0 {
            return;
        }
        truncate_utf8(&mut record.output_snippet, 8 * 1024);
        record.failed_nodeids.truncate(10_000);
        record.dynamic_dependencies.sort();
        record.dynamic_dependencies.dedup();
        record.dynamic_dependencies.truncate(4_096);
        if record.node_results.len() > policy.max_node_results_per_file {
            record.node_results = record
                .node_results
                .into_iter()
                .take(policy.max_node_results_per_file)
                .collect();
        }
        for node in record.node_results.values_mut() {
            truncate_utf8(&mut node.output_snippet, 2 * 1024);
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
        while self.history_by_key.len() > policy.max_cached_results
            || self.estimated_bytes() > policy.max_cache_bytes
        {
            if let Some(oldest_key) = self
                .history_by_key
                .iter()
                .min_by_key(|(_, r)| r.updated_unix_ms)
                .map(|(k, _)| k.clone())
            {
                self.history_by_key.remove(&oldest_key);
                if let Some((test_file, dep_hash)) = oldest_key.rsplit_once('@')
                    && self
                        .latest_by_file
                        .get(test_file)
                        .is_some_and(|record| record.dep_hash == dep_hash)
                {
                    self.latest_by_file.remove(test_file);
                }
                self.quarantined_keys.remove(&oldest_key);
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

    pub fn record_invocation(&mut self, test_files: &[String]) {
        if test_files.is_empty() || test_files.len() > 256 {
            return;
        }
        let mut files = test_files.to_vec();
        files.sort();
        files.dedup();
        let key = sha256_hex(files.join("\0").as_bytes());
        let pattern = self
            .invocation_patterns
            .entry(key.clone())
            .or_insert_with(|| InvocationPattern {
                test_files: files,
                frequency: 0,
                last_used_unix_ms: 0,
            });
        pattern.frequency = pattern.frequency.saturating_add(1);
        pattern.last_used_unix_ms = now_unix_ms();
        let updated_pattern = pattern.clone();
        if let Some(pending_pattern) = self.pending_events.iter_mut().rev().find_map(|event| {
            if let CacheWalEvent::Invocation {
                key: pending_key,
                pattern,
            } = event
                && pending_key == &key
            {
                return Some(pattern);
            }
            None
        }) {
            *pending_pattern = updated_pattern;
        } else {
            self.pending_events.push(CacheWalEvent::Invocation {
                key,
                pattern: updated_pattern,
            });
        }
        self.trim_invocation_patterns();
    }

    fn trim_invocation_patterns(&mut self) {
        while self.invocation_patterns.len() > 128 {
            let Some(oldest) = self
                .invocation_patterns
                .iter()
                .min_by_key(|(_, pattern)| pattern.last_used_unix_ms)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.invocation_patterns.remove(&oldest);
            self.pending_events.retain(|event| {
                !matches!(
                    event,
                    CacheWalEvent::Invocation { key, .. } if key == &oldest
                )
            });
        }
    }

    pub fn predicted_affected_tests(&self, affected: &[String]) -> Vec<String> {
        let affected: BTreeSet<&str> = affected.iter().map(String::as_str).collect();
        let mut scores: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for pattern in self.invocation_patterns.values() {
            for test_file in &pattern.test_files {
                if affected.contains(test_file.as_str()) {
                    let entry = scores.entry(test_file.clone()).or_insert((0, 0));
                    entry.0 = entry.0.saturating_add(pattern.frequency);
                    entry.1 = entry.1.max(pattern.last_used_unix_ms);
                }
            }
        }
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_by_key(|(_, (frequency, last_used))| {
            (std::cmp::Reverse(*frequency), std::cmp::Reverse(*last_used))
        });
        ranked.into_iter().map(|(test_file, _)| test_file).collect()
    }

    pub fn was_invoked(&self, test_file: &str) -> bool {
        self.invocation_patterns
            .values()
            .any(|pattern| pattern.test_files.iter().any(|path| path == test_file))
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
        Self::load_unlocked(root)
    }

    fn load_unlocked(root: &Path) -> Self {
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
            let mut expected_sequence = cache.wal_sequence.saturating_add(1);
            let mut valid_bytes = 0u64;
            while total <= 64 * 1024 * 1024 {
                line.clear();
                let Ok(read) = reader.read_line(&mut line) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                total = total.saturating_add(read);
                let Ok(envelope) = serde_json::from_str::<CacheWalEnvelope>(&line) else {
                    // A crash can leave only the last line truncated; preserve all earlier events.
                    cache.persistence_error = Some("CACHE_WAL_TRUNCATED_TAIL".to_string());
                    cache.wal_needs_repair = true;
                    break;
                };
                let Ok(event_bytes) = serde_json::to_vec(&envelope.event) else {
                    cache.persistence_error = Some("CACHE_WAL_EVENT_SERIALIZE_FAILED".to_string());
                    cache.wal_needs_repair = true;
                    break;
                };
                if envelope.sequence != expected_sequence
                    || sha256_hex(&event_bytes) != envelope.checksum
                {
                    cache.persistence_error = Some(format!(
                        "CACHE_WAL_CORRUPT expected_sequence={} actual_sequence={}",
                        expected_sequence, envelope.sequence
                    ));
                    cache.wal_needs_repair = true;
                    break;
                }
                cache.apply_wal_event(envelope.event);
                cache.wal_sequence = envelope.sequence;
                expected_sequence = expected_sequence.saturating_add(1);
                valid_bytes = valid_bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            }
            cache.wal_valid_bytes = valid_bytes;
        }
        cache.pending_events.clear();
        cache.trim_invocation_patterns();
        let default_policy = MemoryPolicy::default();
        while cache.history_by_key.len() > default_policy.max_cached_results
            || cache.estimated_bytes() > default_policy.max_cache_bytes
        {
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
            if let Some((test_file, dep_hash)) = oldest.rsplit_once('@')
                && cache
                    .latest_by_file
                    .get(test_file)
                    .is_some_and(|record| record.dep_hash == dep_hash)
            {
                cache.latest_by_file.remove(test_file);
            }
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
        let pending_events = std::mem::take(&mut self.pending_events);
        let mut durable = Self::load_unlocked(&self.root);
        for event in pending_events.iter().cloned() {
            durable.apply_wal_event(event);
        }
        durable.pending_events = pending_events;
        *self = durable;

        let wal_path = Self::wal_path(&self.root)?;
        {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).read(true).write(true);
            let mut wal = options.mode(0o600).open(&wal_path)?;
            if self.wal_needs_repair {
                wal.set_len(self.wal_valid_bytes)?;
                wal.sync_data()?;
            }
            self.wal_needs_repair = false;
            wal.seek(std::io::SeekFrom::End(0))?;
            let mut next_sequence = self.wal_sequence.saturating_add(1);
            for event in &self.pending_events {
                let event_bytes = serde_json::to_vec(event)?;
                let envelope = CacheWalEnvelope {
                    sequence: next_sequence,
                    checksum: sha256_hex(&event_bytes),
                    event: event.clone(),
                };
                serde_json::to_writer(&mut wal, &envelope)?;
                wal.write_all(b"\n")?;
                next_sequence = next_sequence.saturating_add(1);
            }
            wal.sync_data()?;
            self.wal_sequence = next_sequence.saturating_sub(1);
            self.wal_valid_bytes = wal.stream_position()?;
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
                if let Some((test_file, dep_hash)) = key.rsplit_once('@')
                    && self
                        .latest_by_file
                        .get(test_file)
                        .is_some_and(|record| record.dep_hash == dep_hash)
                {
                    self.latest_by_file.remove(test_file);
                }
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
            CacheWalEvent::Invocation { key, pattern } => {
                self.invocation_patterns.insert(key, pattern);
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
        std::fs::File::open(catalog::cache_dir()?)?.sync_all()?;
        let wal = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(wal_path)?;
        wal.sync_all()?;
        Ok(())
    }

    pub fn storage_metrics(&self) -> serde_json::Value {
        let snapshot_bytes = Self::storage_path(&self.root)
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let wal_bytes = Self::wal_path(&self.root)
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        serde_json::json!({
            "schema": self.schema,
            "snapshot_bytes": snapshot_bytes,
            "wal_bytes": wal_bytes,
            "wal_sequence": self.wal_sequence,
            "history_records": self.history_by_key.len(),
            "latest_records": self.latest_by_file.len(),
            "estimated_memory_bytes": self.estimated_bytes(),
            "invocation_patterns": self.invocation_patterns.len(),
        })
    }

    pub fn estimated_bytes(&self) -> usize {
        let record_bytes = |record: &CachedTestRecord| {
            record.test_file.len()
                + record.dep_hash.len()
                + record.output_snippet.len()
                + record.environment_hash.len()
                + record.tctx_version.len()
                + record.runner_mode.len()
                + record.failed_nodeids.iter().map(String::len).sum::<usize>()
                + record
                    .dynamic_dependencies
                    .iter()
                    .map(String::len)
                    .sum::<usize>()
                + record
                    .node_results
                    .values()
                    .map(|node| {
                        node.nodeid.len()
                            + node.output_snippet.len()
                            + std::mem::size_of::<CachedNodeResult>()
                    })
                    .sum::<usize>()
                + std::mem::size_of::<CachedTestRecord>()
        };
        self.latest_by_file
            .values()
            .map(record_bytes)
            .sum::<usize>()
            .saturating_add(
                self.history_by_key
                    .values()
                    .map(record_bytes)
                    .sum::<usize>(),
            )
            .saturating_add(self.quarantined_keys.iter().map(String::len).sum::<usize>())
            .saturating_add(
                self.invocation_patterns
                    .values()
                    .map(|pattern| {
                        std::mem::size_of::<InvocationPattern>()
                            + pattern.test_files.iter().map(String::len).sum::<usize>()
                    })
                    .sum::<usize>(),
            )
    }

    pub fn has_pending_events(&self) -> bool {
        !self.pending_events.is_empty()
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
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
    use std::sync::{Mutex, OnceLock};

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

    #[test]
    fn wal_recovers_valid_prefix_from_truncated_tail_and_compacts_atomically() {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            state.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let previous = std::env::var_os("TCTX_HOME");
        unsafe {
            std::env::set_var("TCTX_HOME", state.path());
        }
        let root = state.path().join("project");
        std::fs::create_dir_all(&root).unwrap();

        let mut cache = RootCache::new(root.clone());
        cache.insert(record(TestOutcome::Passed), &MemoryPolicy::default());
        cache.save().unwrap();
        let wal_path = RootCache::wal_path(&root).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .unwrap()
            .write_all(b"{\"sequence\":")
            .unwrap();

        let mut loaded = RootCache::load_or_new(&root);
        assert!(
            loaded
                .get_matching("tests/test_one.py", "same-hash")
                .is_some()
        );
        assert_eq!(
            loaded.persistence_error.as_deref(),
            Some("CACHE_WAL_TRUNCATED_TAIL")
        );

        let mut second = record(TestOutcome::Passed);
        second.test_file = "tests/test_two.py".to_string();
        second.dep_hash = "second-hash".to_string();
        loaded.insert(second, &MemoryPolicy::default());
        loaded.save().unwrap();
        let repaired = RootCache::load_or_new(&root);
        assert!(
            repaired
                .get_matching("tests/test_one.py", "same-hash")
                .is_some()
        );
        assert!(
            repaired
                .get_matching("tests/test_two.py", "second-hash")
                .is_some()
        );

        let mut concurrent_a = RootCache::load_or_new(&root);
        let mut concurrent_b = RootCache::load_or_new(&root);
        let mut third = record(TestOutcome::Passed);
        third.test_file = "tests/test_three.py".to_string();
        third.dep_hash = "third-hash".to_string();
        concurrent_a.insert(third, &MemoryPolicy::default());
        let mut fourth = record(TestOutcome::Passed);
        fourth.test_file = "tests/test_four.py".to_string();
        fourth.dep_hash = "fourth-hash".to_string();
        concurrent_b.insert(fourth, &MemoryPolicy::default());
        concurrent_a.save().unwrap();
        concurrent_b.save().unwrap();
        concurrent_a.record_invocation(&["tests/test_three.py".to_string()]);
        concurrent_a.save().unwrap();
        concurrent_a.compact_snapshot(&wal_path).unwrap();
        let mut merged = RootCache::load_or_new(&root);
        assert!(
            merged
                .get_matching("tests/test_three.py", "third-hash")
                .is_some()
        );
        assert!(
            merged
                .get_matching("tests/test_four.py", "fourth-hash")
                .is_some()
        );

        for index in 0..200 {
            merged.record_invocation(&[format!("tests/test_pattern_{index}.py")]);
        }
        merged.save().unwrap();
        let bounded = RootCache::load_or_new(&root);
        assert!(bounded.invocation_patterns.len() <= 128);

        merged.compact_snapshot(&wal_path).unwrap();
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0);
        assert!(RootCache::storage_path(&root).unwrap().is_file());

        unsafe {
            if let Some(value) = previous {
                std::env::set_var("TCTX_HOME", value);
            } else {
                std::env::remove_var("TCTX_HOME");
            }
        }
    }

    #[test]
    fn enforces_record_and_total_memory_bounds() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = RootCache::new(root.path().to_path_buf());
        let policy = MemoryPolicy {
            max_cache_bytes: 32 * 1024,
            max_node_results_per_file: 4,
            ..MemoryPolicy::default()
        };
        for index in 0..20 {
            let mut item = record(TestOutcome::Failed);
            item.test_file = format!("tests/test_{index}.py");
            item.dep_hash = format!("hash-{index}");
            item.output_snippet = "실패".repeat(10_000);
            for node_index in 0..20 {
                let nodeid = format!("tests/test_{index}.py::test_{node_index}");
                item.node_results.insert(
                    nodeid.clone(),
                    CachedNodeResult {
                        nodeid,
                        outcome: TestOutcome::Failed,
                        duration_ms: 1,
                        output_snippet: "trace".repeat(2_000),
                    },
                );
            }
            cache.insert(item, &policy);
        }
        assert!(cache.estimated_bytes() <= policy.max_cache_bytes);
        assert!(cache.latest_by_file.values().all(|record| {
            record.node_results.len() <= 4
                && record.output_snippet.len() <= 8 * 1024
                && record
                    .node_results
                    .values()
                    .all(|node| node.output_snippet.len() <= 2 * 1024)
        }));
    }

    #[test]
    fn learns_bounded_invocation_patterns_and_ranks_affected_tests() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = RootCache::new(root.path().to_path_buf());
        cache.record_invocation(&["tests/test_recent.py".to_string()]);
        for _ in 0..3 {
            cache.record_invocation(&["tests/test_frequent.py".to_string()]);
        }
        let ranked = cache.predicted_affected_tests(&[
            "tests/test_other.py".to_string(),
            "tests/test_recent.py".to_string(),
            "tests/test_frequent.py".to_string(),
        ]);
        assert_eq!(
            ranked,
            vec![
                "tests/test_frequent.py".to_string(),
                "tests/test_recent.py".to_string()
            ]
        );
        assert!(cache.was_invoked("tests/test_frequent.py"));
        assert!(!cache.was_invoked("tests/test_other.py"));

        for index in 0..200 {
            cache.record_invocation(&[format!("tests/test_pattern_{index}.py")]);
        }
        assert!(cache.invocation_patterns.len() <= 128);
        assert!(
            cache
                .pending_events
                .iter()
                .filter(|event| matches!(event, CacheWalEvent::Invocation { .. }))
                .count()
                <= 128
        );
    }
}
