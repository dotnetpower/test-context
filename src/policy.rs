use serde::{Deserialize, Serialize};

pub const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryPolicy {
    pub max_files: usize,
    pub max_source_bytes: usize,
    pub max_file_bytes: usize,
    pub max_total_path_bytes: usize,
    pub max_watch_dirs: usize,
    pub max_cached_results: usize,
    pub max_cache_bytes: usize,
    pub max_node_results_per_file: usize,
    pub pytest_timeout_secs: u64,
    pub background_batch_size: usize,
    pub background_enabled: bool,
    pub background_safe_only: bool,
    pub background_max_load_ratio_percent: usize,
    pub parallel_delta_enabled: bool,
    pub predictive_background_enabled: bool,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_source_bytes: 256 * 1024 * 1024,
            max_file_bytes: 2 * 1024 * 1024,
            max_total_path_bytes: 16 * 1024 * 1024,
            max_watch_dirs: 50_000,
            max_cached_results: 50_000,
            max_cache_bytes: 128 * 1024 * 1024,
            max_node_results_per_file: 10_000,
            pytest_timeout_secs: 180,
            background_batch_size: 4,
            background_enabled: true,
            background_safe_only: true,
            background_max_load_ratio_percent: 75,
            parallel_delta_enabled: false,
            predictive_background_enabled: true,
        }
    }
}

impl MemoryPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_files == 0
            || self.max_source_bytes == 0
            || self.max_file_bytes == 0
            || self.max_total_path_bytes == 0
            || self.max_watch_dirs == 0
            || self.max_cached_results == 0
            || self.max_cache_bytes == 0
            || self.max_node_results_per_file == 0
            || self.pytest_timeout_secs == 0
            || self.background_batch_size == 0
            || self.background_max_load_ratio_percent == 0
        {
            return Err("all memory and runner policy limits must be positive");
        }
        if self.max_file_bytes > self.max_source_bytes {
            return Err("max_file_bytes cannot exceed max_source_bytes");
        }
        if self.background_max_load_ratio_percent > 200 {
            return Err("background_max_load_ratio_percent cannot exceed 200");
        }
        Ok(())
    }
}
