use crate::Result;
use crate::ast_graph::sha256_hex;
use crate::catalog;
use crate::policy::MemoryPolicy;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
pub struct ExternalModuleStamp {
    pub resolved_path: String,
    pub stamp: crate::ast_graph::FileStamp,
    #[serde(default)]
    pub is_dir: bool,
}

fn dir_stamp_for_freshness(full_path: &Path) -> Option<crate::ast_graph::FileStamp> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(full_path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    Some(crate::ast_graph::FileStamp {
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        size: meta.len(),
        ino: meta.ino(),
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    })
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
    #[serde(default)]
    pub external_module_stamps: BTreeMap<String, ExternalModuleStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl CachedTestRecord {
    pub fn external_modules_fresh(&self) -> bool {
        self.external_module_stamps.iter().all(|(lex_path, entry)| {
            let Ok(canon) = std::fs::canonicalize(Path::new(lex_path)) else {
                return false;
            };
            if canon != Path::new(&entry.resolved_path) {
                return false;
            }
            let live = if entry.is_dir {
                dir_stamp_for_freshness(&canon)
            } else {
                crate::ast_graph::file_stamp(&canon)
            };
            live.as_ref() == Some(&entry.stamp)
        })
    }
}

fn current_cache_schema() -> u32 {
    7
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
    /// Highest WAL sequence of a `Reset` (full invalidation) applied to this cache.
    #[serde(default)]
    last_reset_seq: u64,
    /// Highest WAL sequence of an `InvalidateFile` per test file, used during cross-process
    /// rebasing to block stale pre-invalidation upserts without dropping concurrent writes to
    /// unrelated test files.
    #[serde(default)]
    last_file_invalidation_seq: BTreeMap<String, u64>,
    #[serde(skip)]
    wal_valid_bytes: u64,
    #[serde(skip)]
    wal_needs_repair: bool,
    #[serde(skip)]
    wal_needs_newline: bool,
    #[serde(skip)]
    wal_read_capped: bool,
    #[serde(skip)]
    last_snapshot_stamp: Option<(u64, i64, i64)>,
    #[serde(skip)]
    last_wal_stamp: Option<(u64, i64, i64)>,
    #[serde(skip)]
    cached_memory_bytes: Option<usize>,
    #[serde(skip)]
    last_saved_metrics: (u64, u64, u64, u64),
    #[serde(skip)]
    pending_invocation_idx: std::collections::HashMap<String, usize>,
    #[serde(skip)]
    configured_max_cached_results: usize,
    #[serde(skip)]
    configured_max_cache_bytes: usize,
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

pub struct PendingWalFlush {
    root: PathBuf,
    events: Vec<CacheWalEvent>,
    wal_sequence: u64,
    wal_valid_bytes: u64,
    wal_needs_repair: bool,
    last_snapshot_stamp: Option<(u64, i64, i64)>,
    base_metrics: (u64, u64, u64, u64),
    max_cached_results: usize,
    max_cache_bytes: usize,
}

pub struct WalFlushOutcome {
    wal_sequence: u64,
    wal_valid_bytes: u64,
    last_snapshot_stamp: Option<(u64, i64, i64)>,
    last_wal_stamp: Option<(u64, i64, i64)>,
    base_metrics: (u64, u64, u64, u64),
    rebased_cache: Option<Box<RootCache>>,
    compaction_error: Option<String>,
}

impl PendingWalFlush {
    /// Performs file locking, WAL append, fsync, and optional compaction completely outside `RootState`.
    pub fn flush(&self) -> Result<WalFlushOutcome> {
        let wal_path = RootCache::wal_path(&self.root)?;
        let lock_path = RootCache::lock_path(&self.root)?;
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        lock_file.lock_exclusive()?;
        let disk_wal_len = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        let disk_snap_stamp = RootCache::snapshot_stamp(&self.root);

        if self.wal_needs_repair
            || disk_wal_len != self.wal_valid_bytes
            || disk_snap_stamp != self.last_snapshot_stamp
        {
            let mut durable = RootCache::load_unlocked_with_limits(
                &self.root,
                self.max_cached_results,
                self.max_cache_bytes,
            );
            if durable.persistence_error.as_deref() == Some("CACHE_SNAPSHOT_UNREADABLE_OR_OVERSIZE")
                || durable.wal_read_capped
            {
                return Err(crate::invalid(
                    "CACHE_FAIL_CLOSED: refusing to overwrite snapshot or truncate WAL",
                )
                .into());
            }
            // Reconcile pending local events onto the newer durable state: ALWAYS detect and
            // emit flaky-outcome Quarantine BEFORE dropping upserts superseded by a newer
            // external `Reset` or `InvalidateFile` (while preserving concurrent writes to
            // unrelated test files).
            let events_to_write = RootCache::reconcile_pending_onto_durable(
                &mut durable,
                self.events.clone(),
                self.wal_sequence,
            );
            // Enforce configured policy limits on merged state so concurrent writers cannot exceed bounds
            let limit_policy = MemoryPolicy {
                max_cached_results: self.max_cached_results,
                max_cache_bytes: self.max_cache_bytes,
                ..MemoryPolicy::default()
            };
            let current_bytes = durable.estimated_bytes();
            if durable.history_by_key.len() > limit_policy.max_cached_results
                || current_bytes > limit_policy.max_cache_bytes
            {
                let evicted_bytes = durable.evict_to_bounds(current_bytes, &limit_policy, false);
                durable.cached_memory_bytes = Some(evicted_bytes);
            }
            durable.pending_events = events_to_write;
            // Already hold lock_file; write WAL directly
            let mut options = std::fs::OpenOptions::new();
            options.create(true).read(true).write(true);
            let mut wal = options.mode(0o600).open(&wal_path)?;
            if durable.wal_needs_repair {
                wal.set_len(durable.wal_valid_bytes)?;
                if durable.wal_needs_newline && durable.wal_valid_bytes > 0 {
                    wal.seek(std::io::SeekFrom::End(0))?;
                    wal.write_all(b"\n")?;
                }
                wal.sync_data()?;
            }
            durable.wal_needs_repair = false;
            durable.wal_needs_newline = false;
            wal.seek(std::io::SeekFrom::End(0))?;
            let wal_start_pos = wal.stream_position()?;
            let mut next_sequence = durable.wal_sequence.saturating_add(1);
            let write_res: Result<()> = (|| {
                {
                    let mut writer = BufWriter::with_capacity(64 * 1024, &mut wal);
                    for event in &durable.pending_events {
                        let event_bytes = serde_json::to_vec(event)?;
                        let checksum = sha256_hex(&event_bytes);
                        write!(
                            writer,
                            "{{\"sequence\":{next_sequence},\"checksum\":\"{checksum}\",\"event\":"
                        )?;
                        writer.write_all(&event_bytes)?;
                        writer.write_all(b"}\n")?;
                        next_sequence = next_sequence.saturating_add(1);
                    }
                    writer.flush()?;
                }
                wal.sync_data()?;
                Ok(())
            })();
            if write_res.is_ok() {
                let mut seq_cursor = durable.wal_sequence.saturating_add(1);
                for event in &durable.pending_events {
                    match event {
                        CacheWalEvent::Reset => {
                            durable.last_reset_seq = durable.last_reset_seq.max(seq_cursor);
                            durable.last_file_invalidation_seq.clear();
                        }
                        CacheWalEvent::InvalidateFile { test_file } => {
                            let slot = durable
                                .last_file_invalidation_seq
                                .entry(test_file.clone())
                                .or_insert(0);
                            *slot = (*slot).max(seq_cursor);
                        }
                        _ => {}
                    }
                    seq_cursor = seq_cursor.saturating_add(1);
                }
            }
            if let Err(err) = write_res {
                // Roll back any partially written or unsynced bytes so a retry cannot duplicate events
                // after another writer's invalidation; if rollback itself fails, fail closed.
                if wal.set_len(wal_start_pos).is_err() || wal.sync_data().is_err() {
                    return Err(crate::invalid(format!(
                        "CACHE_FAIL_CLOSED: WAL write/sync failed and rollback failed: {err}"
                    ))
                    .into());
                }
                return Err(err);
            }
            durable.wal_sequence = next_sequence.saturating_sub(1);
            durable.wal_valid_bytes = wal.stream_position()?;
            durable.pending_events.clear();
            durable.pending_invocation_idx.clear();
            // WAL events are already durably synced; if post-sync compaction fails, never return Err
            // (which would cause `restore_failed_flush` to re-append already-committed events).
            let compaction_error = if durable.wal_valid_bytes > 4 * 1024 * 1024 {
                durable
                    .compact_snapshot(&wal_path)
                    .err()
                    .map(|e| e.to_string())
            } else {
                None
            };
            durable.last_saved_metrics = (
                durable.total_saved_ms,
                durable.total_cache_hits,
                durable.total_delta_runs,
                durable.total_background_runs,
            );
            return Ok(WalFlushOutcome {
                wal_sequence: durable.wal_sequence,
                wal_valid_bytes: durable.wal_valid_bytes,
                last_snapshot_stamp: durable.last_snapshot_stamp,
                last_wal_stamp: RootCache::wal_stamp(&self.root),
                base_metrics: self.base_metrics,
                rebased_cache: Some(Box::new(durable)),
                compaction_error,
            });
        }

        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        let mut wal = options.mode(0o600).open(&wal_path)?;
        wal.seek(std::io::SeekFrom::End(0))?;
        let wal_start_pos = wal.stream_position()?;
        let mut next_sequence = self.wal_sequence.saturating_add(1);
        let write_res: Result<()> = (|| {
            {
                let mut writer = BufWriter::with_capacity(64 * 1024, &mut wal);
                for event in &self.events {
                    let event_bytes = serde_json::to_vec(event)?;
                    let checksum = sha256_hex(&event_bytes);
                    write!(
                        writer,
                        "{{\"sequence\":{next_sequence},\"checksum\":\"{checksum}\",\"event\":"
                    )?;
                    writer.write_all(&event_bytes)?;
                    writer.write_all(b"}\n")?;
                    next_sequence = next_sequence.saturating_add(1);
                }
                writer.flush()?;
            }
            wal.sync_data()?;
            Ok(())
        })();
        if let Err(err) = write_res {
            if wal.set_len(wal_start_pos).is_err() || wal.sync_data().is_err() {
                return Err(crate::invalid(format!(
                    "CACHE_FAIL_CLOSED: WAL write/sync failed and rollback failed: {err}"
                ))
                .into());
            }
            return Err(err);
        }
        let new_seq = next_sequence.saturating_sub(1);
        let new_pos = wal.stream_position()?;
        if new_pos > 4 * 1024 * 1024 {
            let mut durable = RootCache::load_unlocked_with_limits(
                &self.root,
                self.max_cached_results,
                self.max_cache_bytes,
            );
            // WAL append is already committed on disk; if reload shows fail-closed or compaction fails,
            // advance WAL state without re-queueing committed events, and surface the error.
            if durable.is_fail_closed() {
                return Ok(WalFlushOutcome {
                    wal_sequence: new_seq,
                    wal_valid_bytes: new_pos,
                    last_snapshot_stamp: self.last_snapshot_stamp,
                    last_wal_stamp: RootCache::wal_stamp(&self.root),
                    base_metrics: self.base_metrics,
                    rebased_cache: None,
                    compaction_error: Some(
                        "CACHE_FAIL_CLOSED: refusing to overwrite snapshot or truncate WAL"
                            .to_string(),
                    ),
                });
            }
            if let Err(err) = durable.compact_snapshot(&wal_path) {
                return Ok(WalFlushOutcome {
                    wal_sequence: new_seq,
                    wal_valid_bytes: new_pos,
                    last_snapshot_stamp: self.last_snapshot_stamp,
                    last_wal_stamp: RootCache::wal_stamp(&self.root),
                    base_metrics: self.base_metrics,
                    rebased_cache: None,
                    compaction_error: Some(err.to_string()),
                });
            }
            return Ok(WalFlushOutcome {
                wal_sequence: durable.wal_sequence,
                wal_valid_bytes: durable.wal_valid_bytes,
                last_snapshot_stamp: durable.last_snapshot_stamp,
                last_wal_stamp: RootCache::wal_stamp(&self.root),
                base_metrics: self.base_metrics,
                rebased_cache: Some(Box::new(durable)),
                compaction_error: None,
            });
        }
        Ok(WalFlushOutcome {
            wal_sequence: new_seq,
            wal_valid_bytes: new_pos,
            last_snapshot_stamp: self.last_snapshot_stamp,
            last_wal_stamp: RootCache::wal_stamp(&self.root),
            base_metrics: self.base_metrics,
            rebased_cache: None,
            compaction_error: None,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct CacheWalEnvelope {
    sequence: u64,
    checksum: String,
    event: CacheWalEvent,
}

fn read_line_capped<R: BufRead>(
    reader: &mut R,
    out: &mut String,
    max_bytes: usize,
) -> std::io::Result<usize> {
    out.clear();
    let mut raw = Vec::new();
    let mut total = 0usize;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let buf_len = buf.len();
        let remaining = max_bytes.saturating_sub(total);
        if let Some(nl) = memchr::memchr(b'\n', buf) {
            let take = nl + 1;
            if take > remaining {
                reader.consume(remaining.min(buf_len));
                total = max_bytes.saturating_add(1);
                return Ok(total);
            }
            raw.extend_from_slice(&buf[..take]);
            reader.consume(take);
            total = total.saturating_add(take);
            break;
        }
        if buf_len > remaining {
            reader.consume(remaining.min(buf_len));
            total = max_bytes.saturating_add(1);
            return Ok(total);
        }
        raw.extend_from_slice(buf);
        reader.consume(buf_len);
        total = total.saturating_add(buf_len);
    }
    match String::from_utf8(raw) {
        Ok(s) => {
            *out = s;
            Ok(total)
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "WAL line contains invalid UTF-8",
        )),
    }
}

impl RootCache {
    pub fn new(root: PathBuf) -> Self {
        let default_policy = MemoryPolicy::default();
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
            last_reset_seq: 0,
            last_file_invalidation_seq: BTreeMap::new(),
            wal_valid_bytes: 0,
            wal_needs_repair: false,
            wal_needs_newline: false,
            wal_read_capped: false,
            last_snapshot_stamp: None,
            last_wal_stamp: None,
            cached_memory_bytes: Some(0),
            last_saved_metrics: (0, 0, 0, 0),
            pending_invocation_idx: std::collections::HashMap::new(),
            configured_max_cached_results: default_policy.max_cached_results,
            configured_max_cache_bytes: default_policy.max_cache_bytes,
        }
    }

    fn is_fail_closed(&self) -> bool {
        self.wal_read_capped
            || self.persistence_error.as_deref().is_some_and(|err| {
                err == "CACHE_SNAPSHOT_UNREADABLE_OR_OVERSIZE"
                    || err.starts_with("CACHE_FAIL_CLOSED")
            })
    }

    fn snapshot_read_cap_bytes(max_cache_bytes: usize) -> u64 {
        // JSON escaping can expand strings up to 6x plus structural overhead; 8x (>= 256 MiB)
        // guarantees any policy-compliant self-written snapshot stays strictly below its read cap.
        (max_cache_bytes as u64)
            .saturating_mul(8)
            .max(256 * 1024 * 1024)
    }

    fn effective_limits(&self) -> (usize, usize) {
        let default_policy = MemoryPolicy::default();
        let max_results = if self.configured_max_cached_results > 0 {
            self.configured_max_cached_results
        } else {
            default_policy.max_cached_results
        };
        let max_bytes = if self.configured_max_cache_bytes > 0 {
            self.configured_max_cache_bytes
        } else {
            default_policy.max_cache_bytes
        };
        (max_results, max_bytes)
    }

    fn key(test_file: &str, dep_hash: &str) -> String {
        format!("{test_file}@{dep_hash}")
    }

    fn snapshot_stamp(root: &Path) -> Option<(u64, i64, i64)> {
        let path = Self::storage_path(root).ok()?;
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.len(), meta.mtime(), meta.mtime_nsec()))
    }

    fn wal_stamp(root: &Path) -> Option<(u64, i64, i64)> {
        let path = Self::wal_path(root).ok()?;
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.len(), meta.mtime(), meta.mtime_nsec()))
    }

    /// Returns `true` when another process modified the on-disk snapshot or WAL since our last
    /// load/flush/compaction (cost: two `stat` syscalls, zero disk reads on steady state).
    pub fn disk_changed_since_sync(&self) -> bool {
        Self::snapshot_stamp(&self.root) != self.last_snapshot_stamp
            || Self::wal_stamp(&self.root) != self.last_wal_stamp
    }

    /// Reconciles local unflushed events onto a durable on-disk cache state.
    /// 1. Always checks contradictory outcomes (`prev.is_green() != record.is_green()`) FIRST and
    ///    records `Quarantine` even when a newer external invalidation occurred.
    /// 2. Discards stale pre-invalidation `Upsert` events ONLY when a newer external `Reset` or
    ///    matching `InvalidateFile` (`seq > prev_wal_seq`) invalidated that test file, preserving
    ///    legitimate concurrent writes to unrelated files.
    fn reconcile_pending_onto_durable(
        durable: &mut RootCache,
        local_pending: Vec<CacheWalEvent>,
        prev_wal_seq: u64,
    ) -> Vec<CacheWalEvent> {
        let mut kept = Vec::with_capacity(local_pending.len());
        for ev in local_pending {
            match ev {
                CacheWalEvent::Upsert { ref record } => {
                    let key = Self::key(&record.test_file, &record.dep_hash);
                    let mut quarantine_key = None;
                    if let Some(prev) = durable.history_by_key.get(&key)
                        && prev.outcome.is_green() != record.outcome.is_green()
                        && durable.quarantined_keys.insert(key.clone())
                    {
                        quarantine_key = Some(key);
                    }
                    let invalidated_externally = durable.last_reset_seq > prev_wal_seq
                        || durable
                            .last_file_invalidation_seq
                            .get(&record.test_file)
                            .copied()
                            .unwrap_or(0)
                            > prev_wal_seq;
                    if invalidated_externally {
                        // Emit any newly detected contradiction Quarantine without resurrecting
                        // an older pre-invalidation Upsert on top of a newer external invalidation.
                        if let Some(qkey) = quarantine_key {
                            kept.push(CacheWalEvent::Quarantine { key: qkey });
                        }
                        continue;
                    }
                    durable.apply_wal_event(ev.clone());
                    kept.push(ev);
                    if let Some(qkey) = quarantine_key {
                        kept.push(CacheWalEvent::Quarantine { key: qkey });
                    }
                }
                other => {
                    durable.apply_wal_event(other.clone());
                    kept.push(other);
                }
            }
        }
        kept
    }

    /// Returns the highest WAL sequence of a full `Reset` or per-file `InvalidateFile` applied
    /// for `test_file`, used to prevent in-flight executions started before an invalidation from
    /// subsequently caching superseded results.
    pub fn invalidation_watermark_for(&self, test_file: &str) -> u64 {
        self.last_reset_seq.max(
            self.last_file_invalidation_seq
                .get(test_file)
                .copied()
                .unwrap_or(0),
        )
    }

    /// Reconciles cross-process on-disk snapshot/WAL updates (such as `tctx invalidate` run from
    /// another CLI process) before serving cache hits.
    pub fn refresh_from_disk_if_changed(&mut self, policy: &MemoryPolicy) {
        if !self.disk_changed_since_sync() {
            return;
        }
        let prev_wal_seq = self.wal_sequence;
        let mut refreshed = Self::load_or_new_with_limits(
            &self.root,
            policy.max_cached_results,
            policy.max_cache_bytes,
        );
        let local_pending = std::mem::take(&mut self.pending_events);
        let kept_pending =
            Self::reconcile_pending_onto_durable(&mut refreshed, local_pending, prev_wal_seq);
        refreshed.pending_events = kept_pending;
        refreshed.pending_invocation_idx.clear();
        *self = refreshed;
    }

    pub fn get_matching(&self, test_file: &str, dep_hash: &str) -> Option<&CachedTestRecord> {
        if self.is_fail_closed() {
            return None;
        }
        if self.quarantined_keys.is_empty()
            && let Some(rec) = self.latest_by_file.get(test_file)
            && rec.dep_hash == dep_hash
        {
            return (rec.dynamic_dependencies_complete && rec.external_modules_fresh())
                .then_some(rec);
        }
        let key = Self::key(test_file, dep_hash);
        if self.quarantined_keys.contains(&key) {
            return None;
        }
        if let Some(rec) = self.latest_by_file.get(test_file)
            && rec.dep_hash == dep_hash
        {
            return (rec.dynamic_dependencies_complete && rec.external_modules_fresh())
                .then_some(rec);
        }
        self.history_by_key
            .get(&key)
            .filter(|rec| rec.dynamic_dependencies_complete && rec.external_modules_fresh())
    }

    /// Returns sorted list of test files that have exhibited flaky (non-deterministic pass/fail) transitions on an identical `dep_hash`.
    pub fn flaky_test_files(&self) -> Vec<String> {
        let mut files = BTreeSet::new();
        for q_key in &self.quarantined_keys {
            if let Some((tf, _)) = q_key.rsplit_once('@') {
                files.insert(tf.to_string());
            }
        }
        files.into_iter().collect()
    }

    pub fn is_flaky_test_file(&self, test_file: &str) -> bool {
        let prefix = format!("{test_file}@");
        self.quarantined_keys
            .range(prefix.clone()..)
            .next()
            .is_some_and(|k| k.starts_with(&prefix))
    }

    /// Exports portable cache records for CI/CD cache sharing (`tctx cache-export`).
    pub fn export_bundle(&self) -> serde_json::Value {
        let records: Vec<&CachedTestRecord> = self.history_by_key.values().collect();
        serde_json::json!({
            "schema": self.schema,
            "records_count": records.len(),
            "quarantined_keys": self.quarantined_keys,
            "records": records,
        })
    }

    /// Imports portable cache records from a bundle (`tctx cache-import`) and returns the number of imported records.
    pub fn import_bundle(&mut self, bundle: &serde_json::Value, policy: &MemoryPolicy) -> usize {
        let Some(arr) = bundle.get("records").and_then(serde_json::Value::as_array) else {
            return 0;
        };
        let mut count = 0usize;
        for item in arr {
            if let Ok(rec) = serde_json::from_value::<CachedTestRecord>(item.clone()) {
                self.insert(rec, policy);
                count += 1;
            }
        }
        if let Some(q_arr) = bundle
            .get("quarantined_keys")
            .and_then(serde_json::Value::as_array)
        {
            for q in q_arr.iter().filter_map(serde_json::Value::as_str) {
                if self.quarantined_keys.insert(q.to_string()) {
                    self.pending_events.push(CacheWalEvent::Quarantine {
                        key: q.to_string(),
                    });
                }
            }
        }
        count
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
        if record.dynamic_dependencies.len() > 4_096 {
            // Never silently truncate dynamic_dependencies while storing a dep_hash computed from
            // all of them (which would become unmatchable after restart); mark incomplete and skip caching.
            return;
        }
        if record.node_results.len() > policy.max_node_results_per_file {
            // Never keep a truncated partial subset of `node_results`: with `rerun_failed=false`
            // a selector matching both retained passing and omitted failing nodes could falsely
            // replay green. Clear `node_results` while still caching the complete whole-file outcome.
            record.node_results.clear();
        }
        for node in record.node_results.values_mut() {
            if node.outcome.is_green() {
                node.output_snippet.clear();
                node.output_snippet.shrink_to_fit();
            } else {
                truncate_utf8(&mut node.output_snippet, 2 * 1024);
            }
        }
        let mut current_bytes = self.estimated_bytes();
        let rec_bytes = record_estimated_bytes(&record);
        let key = Self::key(&record.test_file, &record.dep_hash);
        if let Some(existing) = self.history_by_key.get(&key)
            && existing.outcome != record.outcome
        {
            if self.quarantined_keys.insert(key.clone()) {
                current_bytes = current_bytes.saturating_add(key.len());
            }
            self.pending_events
                .push(CacheWalEvent::Quarantine { key: key.clone() });
        }
        if let Some(old_latest) = self
            .latest_by_file
            .insert(record.test_file.clone(), record.clone())
        {
            current_bytes = current_bytes.saturating_sub(record_estimated_bytes(&old_latest));
        }
        current_bytes = current_bytes.saturating_add(rec_bytes);
        if let Some(old_hist) = self.history_by_key.insert(key, record.clone()) {
            current_bytes = current_bytes.saturating_sub(record_estimated_bytes(&old_hist));
        }
        current_bytes = current_bytes.saturating_add(rec_bytes);
        self.configured_max_cached_results = policy.max_cached_results;
        self.configured_max_cache_bytes = policy.max_cache_bytes;
        if !self.is_fail_closed() {
            self.pending_events.push(CacheWalEvent::Upsert {
                record: Box::new(record),
            });
        }
        if self.history_by_key.len() > policy.max_cached_results
            || current_bytes > policy.max_cache_bytes
        {
            current_bytes = self.evict_to_bounds(current_bytes, policy, !self.is_fail_closed());
        }
        self.cached_memory_bytes = Some(current_bytes);
    }

    fn evict_to_bounds(
        &mut self,
        mut current_bytes: usize,
        policy: &MemoryPolicy,
        record_wal: bool,
    ) -> usize {
        if self.history_by_key.len() <= policy.max_cached_results
            && current_bytes <= policy.max_cache_bytes
        {
            return current_bytes;
        }
        let mut by_age: Vec<(u64, String)> = self
            .history_by_key
            .iter()
            .map(|(k, r)| (r.updated_unix_ms, k.clone()))
            .collect();
        let excess_count = self
            .history_by_key
            .len()
            .saturating_sub(policy.max_cached_results);
        if current_bytes <= policy.max_cache_bytes && excess_count > 0 && excess_count < by_age.len()
        {
            by_age.select_nth_unstable_by(excess_count, |a, b| {
                a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1))
            });
            by_age.truncate(excess_count);
            by_age.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        } else {
            by_age.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        }
        for (_, oldest_key) in by_age {
            if self.history_by_key.len() <= policy.max_cached_results
                && current_bytes <= policy.max_cache_bytes
            {
                break;
            }
            if let Some(removed_hist) = self.history_by_key.remove(&oldest_key) {
                current_bytes =
                    current_bytes.saturating_sub(record_estimated_bytes(&removed_hist));
            }
            if let Some((test_file, dep_hash)) = oldest_key.rsplit_once('@')
                && self
                    .latest_by_file
                    .get(test_file)
                    .is_some_and(|record| record.dep_hash == dep_hash)
                && let Some(removed_latest) = self.latest_by_file.remove(test_file)
            {
                current_bytes =
                    current_bytes.saturating_sub(record_estimated_bytes(&removed_latest));
            }
            if self.quarantined_keys.remove(&oldest_key) {
                current_bytes = current_bytes.saturating_sub(oldest_key.len());
            }
            if record_wal {
                self.pending_events
                    .push(CacheWalEvent::RemoveKey { key: oldest_key });
            }
        }
        current_bytes
    }

    pub fn invalidate_file(&mut self, test_file: &str) -> usize {
        self.cached_memory_bytes = None;
        let mut removed = usize::from(self.latest_by_file.remove(test_file).is_some());
        let prefix = format!("{test_file}@");
        let keys: Vec<String> = self
            .history_by_key
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            if self.history_by_key.remove(&k).is_some() {
                removed += 1;
            }
        }
        let q_keys: Vec<String> = self
            .quarantined_keys
            .range(prefix.clone()..)
            .take_while(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for qk in q_keys {
            self.quarantined_keys.remove(&qk);
        }
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
        self.cached_memory_bytes = None;
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
        let is_new = !self.invocation_patterns.contains_key(&key);
        let added_bytes = if is_new {
            key.len() + files.iter().map(String::len).sum::<usize>() + 32
        } else {
            0
        };
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
        let freq = pattern.frequency;
        let ts = pattern.last_used_unix_ms;
        if let Some(&idx) = self.pending_invocation_idx.get(&key)
            && let Some(CacheWalEvent::Invocation {
                key: pending_key,
                pattern: pending_pattern,
            }) = self.pending_events.get_mut(idx)
            && pending_key == &key
        {
            pending_pattern.frequency = freq;
            pending_pattern.last_used_unix_ms = ts;
        } else {
            let idx = self.pending_events.len();
            let updated_pattern = pattern.clone();
            self.pending_invocation_idx.insert(key.clone(), idx);
            self.pending_events.push(CacheWalEvent::Invocation {
                key,
                pattern: updated_pattern,
            });
        }
        if let Some(bytes) = self.cached_memory_bytes {
            self.cached_memory_bytes = Some(bytes.saturating_add(added_bytes));
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
            if let Some(removed) = self.invocation_patterns.remove(&oldest)
                && let Some(bytes) = self.cached_memory_bytes
            {
                let removed_bytes = oldest.len()
                    + removed.test_files.iter().map(String::len).sum::<usize>()
                    + 32;
                self.cached_memory_bytes = Some(bytes.saturating_sub(removed_bytes));
            }
            self.pending_events.retain(|event| {
                !matches!(
                    event,
                    CacheWalEvent::Invocation { key, .. } if key == &oldest
                )
            });
            self.pending_invocation_idx.clear();
        }
    }

    pub fn predicted_affected_tests(&self, affected: &[String]) -> Vec<String> {
        if affected.is_empty() || self.invocation_patterns.is_empty() {
            return Vec::new();
        }
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
        let default_policy = MemoryPolicy::default();
        Self::load_or_new_with_limits(
            root,
            default_policy.max_cached_results,
            default_policy.max_cache_bytes,
        )
    }

    pub fn load_or_new_with_limits(
        root: &Path,
        max_cached_results: usize,
        max_cache_bytes: usize,
    ) -> Self {
        let Ok(lock_path) = Self::lock_path(root) else {
            return Self::new(root.to_path_buf());
        };
        let lock_file = match std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
        {
            Ok(lf) => lf,
            Err(_) => {
                let mut fc = Self::new(root.to_path_buf());
                fc.persistence_error = Some("CACHE_LOCK_UNAVAILABLE".to_string());
                return fc;
            }
        };
        if lock_file.lock_shared().is_err() {
            let mut fc = Self::new(root.to_path_buf());
            fc.persistence_error = Some("CACHE_LOCK_UNAVAILABLE".to_string());
            return fc;
        }
        Self::load_unlocked_with_limits(root, max_cached_results, max_cache_bytes)
    }

    fn load_unlocked_with_limits(
        root: &Path,
        max_cached_results: usize,
        max_cache_bytes: usize,
    ) -> Self {
        let max_snapshot_read_bytes = Self::snapshot_read_cap_bytes(max_cache_bytes);
        let snap_stamp = Self::snapshot_stamp(root);
        let approx_cap = snap_stamp
            .map_or(0, |(len, _, _)| usize::try_from(len).unwrap_or(0))
            .min(max_snapshot_read_bytes as usize);
        let mut snapshot_corrupt = false;
        let mut cache = match Self::storage_path(root) {
            Ok(path) => match std::fs::File::open(&path) {
                Ok(file) => {
                    let mut bytes = Vec::with_capacity(approx_cap);
                    let read_res = file
                        .take(max_snapshot_read_bytes + 1)
                        .read_to_end(&mut bytes);
                    if read_res.is_ok()
                        && !bytes.is_empty()
                        && bytes.len() <= max_snapshot_read_bytes as usize
                        && let Ok(parsed) = serde_json::from_slice::<Self>(&bytes)
                    {
                        if parsed.schema != current_cache_schema() {
                            // Clean schema version upgrade: start fresh without treating as corruption
                            Self::new(root.to_path_buf())
                        } else if parsed.root == root {
                            parsed
                        } else {
                            snapshot_corrupt = true;
                            Self::new(root.to_path_buf())
                        }
                    } else {
                        // Existing snapshot file that is empty, oversized, unreadable, or corrupt JSON
                        snapshot_corrupt = true;
                        Self::new(root.to_path_buf())
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::new(root.to_path_buf()),
                Err(_) => {
                    snapshot_corrupt = true;
                    Self::new(root.to_path_buf())
                }
            },
            Err(_) => Self::new(root.to_path_buf()),
        };
        cache.configured_max_cached_results = max_cached_results;
        cache.configured_max_cache_bytes = max_cache_bytes;
        if snapshot_corrupt {
            cache.persistence_error = Some("CACHE_SNAPSHOT_UNREADABLE_OR_OVERSIZE".to_string());
            // Fail closed: do NOT replay or mark WAL for repair/truncation against an empty
            // cache when an existing snapshot could not be read.
            return cache;
        }
        cache.last_snapshot_stamp = snap_stamp;
        cache.last_wal_stamp = Self::wal_stamp(root);
        cache.pending_events.clear();
        if let Ok(wal_path) = Self::wal_path(root) {
            match std::fs::File::open(wal_path) {
                Ok(file) => {
                    let mut reader = BufReader::with_capacity(64 * 1024, file);
                    let mut line = String::new();
                    let mut total = 0usize;
                    let snapshot_watermark = cache.wal_sequence;
                    let mut last_skipped_seq = 0u64;
                    let mut applied_post_snapshot = false;
                    let mut expected_sequence = cache.wal_sequence.saturating_add(1);
                    let mut valid_bytes = 0u64;
                    let mut saw_io_error = false;
                    let fail_closed = |cache: &mut Self, err_msg: String| {
                        cache.latest_by_file.clear();
                        cache.history_by_key.clear();
                        cache.quarantined_keys.clear();
                        cache.invocation_patterns.clear();
                        cache.pending_invocation_idx.clear();
                        cache.wal_read_capped = true;
                        cache.persistence_error = Some(err_msg);
                    };
                    const WAL_REPLAY_CAP_BYTES: usize = 64 * 1024 * 1024;
                    while total <= WAL_REPLAY_CAP_BYTES {
                        let remaining_budget = WAL_REPLAY_CAP_BYTES.saturating_sub(total);
                        let read = match read_line_capped(&mut reader, &mut line, remaining_budget) {
                            Ok(n) => n,
                            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                                fail_closed(
                                    &mut cache,
                                    "CACHE_WAL_MIDFILE_CORRUPT_FAIL_CLOSED".to_string(),
                                );
                                break;
                            }
                            Err(_) => {
                                saw_io_error = true;
                                break;
                            }
                        };
                        if read == 0 {
                            break;
                        }
                        total = total.saturating_add(read);
                        if total > WAL_REPLAY_CAP_BYTES {
                            break;
                        }
                        let has_trailing = reader.fill_buf().is_ok_and(|b| !b.is_empty());
                        let Ok(envelope) = serde_json::from_str::<CacheWalEnvelope>(&line) else {
                            if !has_trailing && !line.ends_with('\n') {
                                // A crash can leave only the final line incomplete; preserve all earlier events.
                                cache.persistence_error = Some("CACHE_WAL_TRUNCATED_TAIL".to_string());
                                cache.wal_needs_repair = true;
                            } else {
                                // Mid-file or complete-line WAL corruption could hide later invalidations;
                                // fail closed rather than serving potentially stale prefix passes.
                                fail_closed(&mut cache, "CACHE_WAL_MIDFILE_CORRUPT_FAIL_CLOSED".to_string());
                            }
                            break;
                        };
                        // Fast-path: hash raw `"event":<json>` slice directly to avoid double serialization
                        let raw_event_slice = line
                            .find(",\"event\":")
                            .and_then(|idx| line.get(idx + ",\"event\":".len()..))
                            .map(|rest| rest.trim_end_matches(['\n', '\r']))
                            .and_then(|rest| rest.strip_suffix('}'));
                        let checksum_ok = raw_event_slice
                            .is_some_and(|raw| sha256_hex(raw.as_bytes()) == envelope.checksum)
                            || serde_json::to_vec(&envelope.event)
                                .is_ok_and(|event_bytes| sha256_hex(&event_bytes) == envelope.checksum);
                        if !checksum_ok {
                            fail_closed(
                                &mut cache,
                                format!(
                                    "CACHE_WAL_CORRUPT_FAIL_CLOSED expected_sequence={} actual_sequence={}",
                                    expected_sequence, envelope.sequence
                                ),
                            );
                            break;
                        }
                        // Compaction renames the snapshot before truncating the WAL; if a crash occurred
                        // between those two operations, safely skip WAL records strictly <= the snapshot
                        // watermark (and strictly increasing), BEFORE any post-snapshot record is applied.
                        if !applied_post_snapshot
                            && envelope.sequence <= snapshot_watermark
                            && envelope.sequence > last_skipped_seq
                        {
                            last_skipped_seq = envelope.sequence;
                            cache.wal_needs_repair = true;
                            valid_bytes =
                                valid_bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
                            if !line.ends_with('\n') {
                                cache.wal_needs_newline = true;
                            }
                            continue;
                        }
                        if envelope.sequence != expected_sequence {
                            fail_closed(
                                &mut cache,
                                format!(
                                    "CACHE_WAL_CORRUPT_FAIL_CLOSED expected_sequence={} actual_sequence={}",
                                    expected_sequence, envelope.sequence
                                ),
                            );
                            break;
                        }
                        applied_post_snapshot = true;
                        cache.apply_wal_event_with_seq(envelope.event, envelope.sequence);
                        cache.wal_sequence = envelope.sequence;
                        expected_sequence = expected_sequence.saturating_add(1);
                        valid_bytes =
                            valid_bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
                        // If a valid final record is missing its trailing newline, append a newline during
                        // repair before writing the next record so two JSON lines cannot concatenate.
                        if !line.ends_with('\n') {
                            cache.wal_needs_repair = true;
                            cache.wal_needs_newline = true;
                        }
                    }
                    if saw_io_error {
                        fail_closed(&mut cache, "CACHE_WAL_READ_ERROR_FAIL_CLOSED".to_string());
                    } else if total > 64 * 1024 * 1024 {
                        // Fail closed: reject persisted hits and never truncate over an unread WAL suffix
                        fail_closed(&mut cache, "CACHE_WAL_CAP_REACHED".to_string());
                    }
                    cache.wal_valid_bytes = valid_bytes;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    // Fail closed if WAL exists but cannot be opened (e.g. permission error / I/O fault)
                    cache.latest_by_file.clear();
                    cache.history_by_key.clear();
                    cache.quarantined_keys.clear();
                    cache.invocation_patterns.clear();
                    cache.pending_invocation_idx.clear();
                    cache.wal_read_capped = true;
                    cache.persistence_error = Some("CACHE_WAL_OPEN_ERROR_FAIL_CLOSED".to_string());
                }
            }
        }
        cache.pending_events.clear();
        cache.trim_invocation_patterns();
        cache.cached_memory_bytes = None;
        let limit_policy = MemoryPolicy {
            max_cached_results,
            max_cache_bytes,
            ..MemoryPolicy::default()
        };
        let mut current_bytes = cache.estimated_bytes();
        if cache.history_by_key.len() > limit_policy.max_cached_results
            || current_bytes > limit_policy.max_cache_bytes
        {
            current_bytes = cache.evict_to_bounds(current_bytes, &limit_policy, false);
        }
        cache.cached_memory_bytes = Some(current_bytes);
        cache.last_saved_metrics = (
            cache.total_saved_ms,
            cache.total_cache_hits,
            cache.total_delta_runs,
            cache.total_background_runs,
        );
        cache
    }

    pub fn save(&mut self) -> Result<()> {
        if self.persistence_error.as_deref() == Some("CACHE_SNAPSHOT_UNREADABLE_OR_OVERSIZE")
            || self.wal_read_capped
        {
            return Err(crate::invalid(
                "CACHE_FAIL_CLOSED: refusing to overwrite snapshot or truncate WAL",
            )
            .into());
        }
        let current_metrics = (
            self.total_saved_ms,
            self.total_cache_hits,
            self.total_delta_runs,
            self.total_background_runs,
        );
        if current_metrics != self.last_saved_metrics {
            self.pending_events.push(CacheWalEvent::Metrics {
                total_saved_ms: self.total_saved_ms,
                total_cache_hits: self.total_cache_hits,
                total_delta_runs: self.total_delta_runs,
                total_background_runs: self.total_background_runs,
            });
        }
        let wal_path = Self::wal_path(&self.root)?;
        if self.pending_events.is_empty() && !self.wal_needs_repair && wal_path.exists() {
            return Ok(());
        }
        let lock_path = Self::lock_path(&self.root)?;
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        lock_file.lock_exclusive()?;
        let disk_wal_len = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        let disk_snap_stamp = Self::snapshot_stamp(&self.root);

        let pending_events = std::mem::take(&mut self.pending_events);
        if self.wal_needs_repair
            || disk_wal_len != self.wal_valid_bytes
            || disk_snap_stamp != self.last_snapshot_stamp
        {
            let (max_results, max_bytes) = self.effective_limits();
            let mut durable = Self::load_unlocked_with_limits(&self.root, max_results, max_bytes);
            if durable.is_fail_closed() {
                self.pending_events = pending_events;
                return Err(crate::invalid(
                    "CACHE_FAIL_CLOSED: refusing to overwrite snapshot or truncate WAL",
                )
                .into());
            }
            let events_to_write = Self::reconcile_pending_onto_durable(
                &mut durable,
                pending_events,
                self.wal_sequence,
            );
            let limit_policy = MemoryPolicy {
                max_cached_results: max_results,
                max_cache_bytes: max_bytes,
                ..MemoryPolicy::default()
            };
            let current_bytes = durable.estimated_bytes();
            if durable.history_by_key.len() > limit_policy.max_cached_results
                || current_bytes > limit_policy.max_cache_bytes
            {
                let evicted_bytes = durable.evict_to_bounds(current_bytes, &limit_policy, false);
                durable.cached_memory_bytes = Some(evicted_bytes);
            }
            durable.pending_events = events_to_write;
            *self = durable;
        } else {
            self.pending_events = pending_events;
        }

        let wal_bytes = {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).read(true).write(true);
            let mut wal = options.mode(0o600).open(&wal_path)?;
            if self.wal_needs_repair {
                wal.set_len(self.wal_valid_bytes)?;
                if self.wal_needs_newline && self.wal_valid_bytes > 0 {
                    wal.seek(std::io::SeekFrom::End(0))?;
                    wal.write_all(b"\n")?;
                }
                wal.sync_data()?;
            }
            self.wal_needs_repair = false;
            self.wal_needs_newline = false;
            wal.seek(std::io::SeekFrom::End(0))?;
            let mut next_sequence = self.wal_sequence.saturating_add(1);
            {
                let mut writer = BufWriter::with_capacity(64 * 1024, &mut wal);
                for event in &self.pending_events {
                    let event_bytes = serde_json::to_vec(event)?;
                    let checksum = sha256_hex(&event_bytes);
                    write!(
                        writer,
                        "{{\"sequence\":{next_sequence},\"checksum\":\"{checksum}\",\"event\":"
                    )?;
                    writer.write_all(&event_bytes)?;
                    writer.write_all(b"}\n")?;
                    next_sequence = next_sequence.saturating_add(1);
                }
                writer.flush()?;
            }
            wal.sync_data()?;
            let mut seq_cursor = self.wal_sequence.saturating_add(1);
            for event in &self.pending_events {
                match event {
                    CacheWalEvent::Reset => {
                        self.last_reset_seq = self.last_reset_seq.max(seq_cursor);
                        self.last_file_invalidation_seq.clear();
                    }
                    CacheWalEvent::InvalidateFile { test_file } => {
                        let slot = self
                            .last_file_invalidation_seq
                            .entry(test_file.clone())
                            .or_insert(0);
                        *slot = (*slot).max(seq_cursor);
                    }
                    _ => {}
                }
                seq_cursor = seq_cursor.saturating_add(1);
            }
            self.wal_sequence = next_sequence.saturating_sub(1);
            let pos = wal.stream_position()?;
            self.wal_valid_bytes = pos;
            self.last_wal_stamp = Self::wal_stamp(&self.root);
            pos
        };
        self.pending_events.clear();
        self.pending_invocation_idx.clear();
        self.last_saved_metrics = (
            self.total_saved_ms,
            self.total_cache_hits,
            self.total_delta_runs,
            self.total_background_runs,
        );

        if wal_bytes > 4 * 1024 * 1024 {
            self.compact_snapshot(&wal_path)?;
        }
        Ok(())
    }

    pub fn take_pending_events_for_flush(&mut self, policy: &MemoryPolicy) -> Option<PendingWalFlush> {
        self.configured_max_cached_results = policy.max_cached_results;
        self.configured_max_cache_bytes = policy.max_cache_bytes;
        if self.is_fail_closed() {
            // Bound pending_events memory while fail-closed; preserve the persistent error state.
            self.pending_events.clear();
            self.pending_invocation_idx.clear();
            return None;
        }
        let current_metrics = (
            self.total_saved_ms,
            self.total_cache_hits,
            self.total_delta_runs,
            self.total_background_runs,
        );
        if current_metrics != self.last_saved_metrics {
            self.pending_events.push(CacheWalEvent::Metrics {
                total_saved_ms: self.total_saved_ms,
                total_cache_hits: self.total_cache_hits,
                total_delta_runs: self.total_delta_runs,
                total_background_runs: self.total_background_runs,
            });
            self.last_saved_metrics = current_metrics;
        }
        if self.pending_events.is_empty() && !self.wal_needs_repair {
            return None;
        }
        let events = std::mem::take(&mut self.pending_events);
        self.pending_invocation_idx.clear();
        Some(PendingWalFlush {
            root: self.root.clone(),
            events,
            wal_sequence: self.wal_sequence,
            wal_valid_bytes: self.wal_valid_bytes,
            wal_needs_repair: self.wal_needs_repair,
            last_snapshot_stamp: self.last_snapshot_stamp,
            base_metrics: current_metrics,
            max_cached_results: policy.max_cached_results,
            max_cache_bytes: policy.max_cache_bytes,
        })
    }

    pub fn apply_flush_outcome(&mut self, outcome: WalFlushOutcome) {
        if let Some(mut rebased) = outcome.rebased_cache {
            // Preserve metric increments accrued in memory while detached I/O was running
            let saved_delta = self.total_saved_ms.saturating_sub(outcome.base_metrics.0);
            let hits_delta = self.total_cache_hits.saturating_sub(outcome.base_metrics.1);
            let delta_runs_delta = self.total_delta_runs.saturating_sub(outcome.base_metrics.2);
            let bg_runs_delta = self
                .total_background_runs
                .saturating_sub(outcome.base_metrics.3);
            rebased.total_saved_ms = rebased.total_saved_ms.saturating_add(saved_delta);
            rebased.total_cache_hits = rebased.total_cache_hits.saturating_add(hits_delta);
            rebased.total_delta_runs = rebased.total_delta_runs.saturating_add(delta_runs_delta);
            rebased.total_background_runs =
                rebased.total_background_runs.saturating_add(bg_runs_delta);
            // Re-apply any newer in-memory pending events created while the detached flush ran,
            // respecting the external invalidation watermark (`self.wal_sequence`) so stale
            // upserts never resurrect invalidated entries while contradictions still quarantine.
            let newer_events = std::mem::take(&mut self.pending_events);
            let merged_pending =
                Self::reconcile_pending_onto_durable(&mut rebased, newer_events, self.wal_sequence);
            rebased.pending_events = merged_pending;
            rebased.pending_invocation_idx.clear();
            rebased.wal_sequence = outcome.wal_sequence;
            rebased.wal_valid_bytes = outcome.wal_valid_bytes;
            rebased.wal_needs_repair = false;
            rebased.wal_needs_newline = false;
            rebased.last_snapshot_stamp = outcome.last_snapshot_stamp;
            rebased.last_wal_stamp = outcome.last_wal_stamp;
            rebased.persistence_error = outcome.compaction_error;
            *self = *rebased;
            return;
        }
        self.wal_sequence = outcome.wal_sequence;
        self.wal_valid_bytes = outcome.wal_valid_bytes;
        self.wal_needs_repair = false;
        self.wal_needs_newline = false;
        self.last_snapshot_stamp = outcome.last_snapshot_stamp;
        self.last_wal_stamp = outcome.last_wal_stamp;
        self.persistence_error = outcome.compaction_error;
    }

    pub fn restore_failed_flush(&mut self, pending: PendingWalFlush, error: String) {
        if error.starts_with("CACHE_FAIL_CLOSED") || error == "CACHE_SNAPSHOT_UNREADABLE_OR_OVERSIZE" {
            self.latest_by_file.clear();
            self.history_by_key.clear();
            self.quarantined_keys.clear();
            self.invocation_patterns.clear();
            self.pending_events.clear();
            self.pending_invocation_idx.clear();
            self.wal_read_capped = true;
            self.persistence_error = Some(error);
            return;
        }
        let mut restored = pending.events;
        restored.append(&mut self.pending_events);
        self.pending_events = restored;
        self.pending_invocation_idx.clear();
        self.persistence_error = Some(error);
    }

    fn apply_wal_event(&mut self, event: CacheWalEvent) {
        self.apply_wal_event_with_seq(event, 0);
    }

    fn apply_wal_event_with_seq(&mut self, event: CacheWalEvent, seq: u64) {
        self.cached_memory_bytes = None;
        match event {
            CacheWalEvent::Upsert { record } => {
                let record = *record;
                let key = Self::key(&record.test_file, &record.dep_hash);
                self.latest_by_file
                    .insert(record.test_file.clone(), record.clone());
                self.history_by_key.insert(key, record);
            }
            CacheWalEvent::InvalidateFile { test_file } => {
                if seq > 0 {
                    let slot = self
                        .last_file_invalidation_seq
                        .entry(test_file.clone())
                        .or_insert(0);
                    *slot = (*slot).max(seq);
                }
                self.latest_by_file.remove(&test_file);
                let prefix = format!("{test_file}@");
                let h_keys: Vec<String> = self
                    .history_by_key
                    .range(prefix.clone()..)
                    .take_while(|(k, _)| k.starts_with(&prefix))
                    .map(|(k, _)| k.clone())
                    .collect();
                for hk in h_keys {
                    self.history_by_key.remove(&hk);
                }
                let q_keys: Vec<String> = self
                    .quarantined_keys
                    .range(prefix.clone()..)
                    .take_while(|k| k.starts_with(&prefix))
                    .cloned()
                    .collect();
                for qk in q_keys {
                    self.quarantined_keys.remove(&qk);
                }
            }
            CacheWalEvent::Reset => {
                if seq > 0 {
                    self.last_reset_seq = self.last_reset_seq.max(seq);
                    self.last_file_invalidation_seq.clear();
                }
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

    fn compact_snapshot(&mut self, wal_path: &Path) -> Result<()> {
        let (_, max_bytes) = self.effective_limits();
        let max_snapshot_bytes = Self::snapshot_read_cap_bytes(max_bytes);
        let path = Self::storage_path(&self.root)?;
        let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let write_res: Result<()> = (|| {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)?;
            let mut writer = BufWriter::with_capacity(64 * 1024, file);
            serde_json::to_writer(&mut writer, self)?;
            writer.flush()?;
            let file = writer.into_inner().map_err(|e| e.into_error())?;
            let written_len = file.metadata()?.len();
            if written_len > max_snapshot_bytes {
                return Err(crate::invalid(format!(
                    "CACHE_SNAPSHOT_WRITE_OVERSIZE: {written_len} > {max_snapshot_bytes}"
                ))
                .into());
            }
            file.sync_all()?;
            std::fs::rename(&tmp_path, &path)?;
            Ok(())
        })();
        if let Err(err) = write_res {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        std::fs::File::open(catalog::cache_dir()?)?.sync_all()?;
        let wal = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(wal_path)?;
        wal.sync_all()?;
        self.wal_valid_bytes = 0;
        self.wal_read_capped = false;
        self.last_snapshot_stamp = Self::snapshot_stamp(&self.root);
        self.last_wal_stamp = Self::wal_stamp(&self.root);
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
}

pub fn gc_cache_dir(
    registered_roots: &[PathBuf],
    orphan_min_age: Duration,
    apply: bool,
) -> Result<serde_json::Value> {
    let cache_dir = catalog::cache_dir()?;
    if !cache_dir.exists() {
        return Ok(serde_json::json!({
            "applied": apply,
            "removed_files": 0,
            "reclaimed_bytes": 0,
            "removed_legacy_files": 0,
            "removed_orphan_files": 0,
        }));
    }
    let active_hashes: BTreeSet<String> = registered_roots
        .iter()
        .filter(|root| root.exists())
        .map(|root| sha256_hex(root.to_string_lossy().as_bytes()))
        .collect();
    let active_wal_suffix = format!(".v{}.wal.jsonl", current_cache_schema());
    let mut removed_files = 0usize;
    let mut reclaimed_bytes = 0u64;
    let mut removed_legacy_files = 0usize;
    let mut removed_orphan_files = 0usize;
    let mut candidate_orphan_hashes = BTreeSet::new();

    let entries: Vec<_> = std::fs::read_dir(&cache_dir)?
        .filter_map(|e| e.ok())
        .collect();
    for entry in &entries {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let age = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .unwrap_or(Duration::ZERO);

        if name.contains(".tmp.") && age >= Duration::from_secs(60) {
            reclaimed_bytes = reclaimed_bytes.saturating_add(meta.len());
            removed_files += 1;
            removed_legacy_files += 1;
            if apply {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }

        if name.ends_with(".wal.jsonl") && !name.ends_with(&active_wal_suffix) {
            reclaimed_bytes = reclaimed_bytes.saturating_add(meta.len());
            removed_files += 1;
            removed_legacy_files += 1;
            if apply {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }

        if name.ends_with(".ast.bin") && !name.ends_with(".v4.ast.bin") {
            reclaimed_bytes = reclaimed_bytes.saturating_add(meta.len());
            removed_files += 1;
            removed_legacy_files += 1;
            if apply {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }

        let Some((hash, rest)) = name.split_once('.') else {
            continue;
        };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }

        if active_hashes.contains(hash) {
            if rest == "json"
                && let Ok(mut f) = std::fs::File::open(&path)
            {
                let mut head = vec![0u8; 512];
                if let Ok(n) = f.read(&mut head) {
                    let prefix = String::from_utf8_lossy(&head[..n]);
                    let current_tag = format!("\"schema\":{}", current_cache_schema());
                    let current_tag_sp = format!("\"schema\": {}", current_cache_schema());
                    if prefix.contains("\"schema\"")
                        && !prefix.contains(&current_tag)
                        && !prefix.contains(&current_tag_sp)
                    {
                        let lock_path = cache_dir.join(format!("{hash}.lock"));
                        let lock_ok = std::fs::OpenOptions::new()
                            .create(true)
                            .read(true)
                            .write(true)
                            .truncate(false)
                            .mode(0o600)
                            .open(&lock_path)
                            .ok()
                            .and_then(|lf| lf.try_lock_exclusive().ok().map(|()| lf));
                        if lock_ok.is_some() {
                            reclaimed_bytes = reclaimed_bytes.saturating_add(meta.len());
                            removed_files += 1;
                            removed_legacy_files += 1;
                            if apply {
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                    }
                }
            }
            continue;
        }

        if age >= orphan_min_age {
            candidate_orphan_hashes.insert(hash.to_string());
        }
    }

    for hash in candidate_orphan_hashes {
        let wal_file = cache_dir.join(format!("{hash}{active_wal_suffix}"));
        let ast_file = cache_dir.join(format!("{hash}.v4.ast.bin"));
        let json_file = cache_dir.join(format!("{hash}.json"));
        let lock_file = cache_dir.join(format!("{hash}.lock"));
        // Unconditionally create & lock the lock file BEFORE checking ages or deleting so a
        // concurrent writer registering/updating this root cannot race an unlocked GC deletion.
        let lock_guard = match std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_file)
        {
            Ok(lf) => {
                if lf.try_lock_exclusive().is_err() {
                    continue;
                }
                lf
            }
            Err(_) => continue,
        };
        let recent_sibling = [&wal_file, &ast_file, &json_file].iter().any(|p| {
            std::fs::symlink_metadata(p)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < orphan_min_age)
        });
        if recent_sibling {
            drop(lock_guard);
            continue;
        }
        // Never unlink `.lock` files: unlinking a lock file while another writer waits on its
        // inode lets a third writer open a new inode and corrupt the WAL concurrently.
        for target in [&wal_file, &ast_file, &json_file] {
            if let Ok(meta) = std::fs::symlink_metadata(target) {
                reclaimed_bytes = reclaimed_bytes.saturating_add(meta.len());
                removed_files += 1;
                removed_orphan_files += 1;
                if apply {
                    let _ = std::fs::remove_file(target);
                }
            }
        }
        drop(lock_guard);
    }

    Ok(serde_json::json!({
        "applied": apply,
        "removed_files": removed_files,
        "reclaimed_bytes": reclaimed_bytes,
        "removed_legacy_files": removed_legacy_files,
        "removed_orphan_files": removed_orphan_files,
    }))
}

impl RootCache {

    pub fn estimated_bytes(&self) -> usize {
        if let Some(cached) = self.cached_memory_bytes {
            return cached;
        }
        self.latest_by_file
            .values()
            .map(record_estimated_bytes)
            .sum::<usize>()
            .saturating_add(
                self.history_by_key
                    .values()
                    .map(record_estimated_bytes)
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
            || (
                self.total_saved_ms,
                self.total_cache_hits,
                self.total_delta_runs,
                self.total_background_runs,
            ) != self.last_saved_metrics
    }
}

fn record_estimated_bytes(record: &CachedTestRecord) -> usize {
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
        + record
            .external_module_stamps
            .iter()
            .map(|(k, v)| k.len() + v.resolved_path.len() + std::mem::size_of::<ExternalModuleStamp>())
            .sum::<usize>()
        + std::mem::size_of::<CachedTestRecord>()
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
            external_module_stamps: BTreeMap::new(),
            exit_code: None,
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
