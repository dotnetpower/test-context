use crate::catalog;
use crate::registration;
use crate::runner::{ManagedRoot, TestRunRequest};
use crate::{Result, invalid};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, IoSlice, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};


mod protocol;
mod metrics;
mod hub;
mod server;
mod client;

#[cfg(test)]
mod tests;

use protocol::*;
use metrics::*;
use hub::*;
use client::*;

pub use protocol::{MAX_WIRE, BINARY_MAGIC, FRAMED_JSON_MAGIC, TRACE_ARGUMENT, MAX_ACTIVE_CONNECTIONS, MAX_CONNECTION_WORKERS, PERSISTENT_CONNECTION_IDLE_TIMEOUT, MAX_RESIDENT_ROOTS, MAX_DAEMON_RSS_BYTES, IDLE_ROOT_TIMEOUT, LARGE_ROOT_IDLE_TIMEOUT, LARGE_ROOT_FILE_THRESHOLD, ApplicationError, is_application_error, Request};
pub(crate) use protocol::{trim_allocator};
pub use server::{serve};
pub use client::{Client, request, request_pooled_or_local, request_or_local_cli, request_or_local, native_bridge};
