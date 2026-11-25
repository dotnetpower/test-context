use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum CacheWalEvent {
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
    pub(super) root: PathBuf,
    pub(super) events: Vec<CacheWalEvent>,
    pub(super) wal_sequence: u64,
    pub(super) wal_valid_bytes: u64,
    pub(super) wal_needs_repair: bool,
    pub(super) last_snapshot_stamp: Option<(u64, i64, i64)>,
    pub(super) base_metrics: (u64, u64, u64, u64),
    pub(super) max_cached_results: usize,
    pub(super) max_cache_bytes: usize,
}

pub struct WalFlushOutcome {
    pub(super) wal_sequence: u64,
    pub(super) wal_valid_bytes: u64,
    pub(super) last_snapshot_stamp: Option<(u64, i64, i64)>,
    pub(super) last_wal_stamp: Option<(u64, i64, i64)>,
    pub(super) base_metrics: (u64, u64, u64, u64),
    pub(super) rebased_cache: Option<Box<RootCache>>,
    pub(super) compaction_error: Option<String>,
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
pub(super) struct CacheWalEnvelope {
    pub(super) sequence: u64,
    pub(super) checksum: String,
    pub(super) event: CacheWalEvent,
}

pub(super) fn read_line_capped<R: BufRead>(
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


