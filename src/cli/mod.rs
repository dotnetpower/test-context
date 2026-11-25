use crate::print_json;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use test_context::{Result, daemon};

mod cargo_dropin;
mod cargo_dropin_args;
mod frontend_dropin;
mod pytest_dropin;

use cargo_dropin_args::*;
pub(crate) use cargo_dropin::run_cargo_test_dropin;
pub(crate) use frontend_dropin::run_frontend_dropin;
pub(crate) use pytest_dropin::run_pytest_dropin;
#[cfg(test)]
pub(crate) use cargo_dropin_args::parse_cargo_test_dropin_args;
