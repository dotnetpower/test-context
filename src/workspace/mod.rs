use crate::exclusion_policy::{is_excluded_dir_name, is_excluded_rel_path};
use crate::policy::MemoryPolicy;
use crate::{Result, invalid};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};


mod gitignore;
mod inventory;
mod process;

#[cfg(test)]
mod tests;

use gitignore::*;

pub use inventory::{PYTEST_CONFIG_FILES, RUST_CONFIG_FILES, FRONTEND_CONFIG_FILES, POLYGLOT_CONFIG_FILES, POLYGLOT_SOURCE_EXTENSIONS, FRONTEND_SOURCE_EXTENSIONS, DATA_FILE_EXTENSIONS, Workspace, Inventory, is_dotenv_filename, is_frontend_config_filename, is_cargo_dot_config_rel};
pub use process::{capture_with_timeout, BoundedOutput, run_command_with_timeout, run_command_with_timeout_tracked};
