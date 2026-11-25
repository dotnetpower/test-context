use crate::catalog::Spec;
use crate::workspace::BoundedOutput;
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};


const ZYGOTE_SCRIPT: &str = include_str!("zygote_worker.py");

mod io;
mod process;

#[cfg(test)]
mod tests;

use io::*;

pub use process::{PytestZygote, ZygoteManager, cancel_process_group, script_shebang_has_interpreter_flags, resolve_python_bin_for_root, resolve_python_bin};
