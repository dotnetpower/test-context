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


mod types;
mod wal;
mod store;

#[cfg(test)]
mod tests;

use types::*;
use wal::*;

pub use types::{TestOutcome, CachedNodeResult, ExternalModuleStamp, CachedTestRecord, InvocationPattern, RootCache};
pub use wal::{PendingWalFlush, WalFlushOutcome};
pub use store::{gc_cache_dir, now_unix_ms};
