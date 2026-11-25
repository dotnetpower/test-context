use crate::ast_graph::{
    AffectedTest, DependencyGraph, FileGraphInfo, file_stamp, normalize_rel_str, sha256_hex,
};
use crate::cache::{CachedNodeResult, CachedTestRecord, RootCache, TestOutcome, now_unix_ms};
use crate::catalog::Spec;
use crate::workspace::{Workspace, run_command_with_timeout, run_command_with_timeout_tracked};
use crate::zygote::{ZygoteManager, cancel_process_group};
use crate::{Result, invalid};
use notify::{Event, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};


mod types;
mod lifecycle;
mod watcher;
mod api;
mod orchestrator;
mod linters;
mod exec_pytest_probe;
mod env_toolchains;
mod env_python;
mod exec_pytest;
mod cargo_mods;
mod cargo_output;
mod cargo_toml;
mod cargo_features;
mod cargo_targets;
mod cargo_selectors;
mod exec_cargo;
mod exec_frontend;
mod exec_go_re2;
mod exec_go;
mod reporting;

#[cfg(test)]
mod tests;

use types::*;
use linters::*;
use exec_pytest_probe::*;
use env_toolchains::*;
use env_python::*;
use exec_pytest::*;
use cargo_mods::*;
use cargo_output::*;
use cargo_toml::*;
use cargo_features::*;
use cargo_targets::*;
use cargo_selectors::*;
use exec_cargo::*;
use exec_frontend::*;
use exec_go_re2::*;
use exec_go::*;
use reporting::*;

pub use types::{TestRunRequest, RuffDiagnostic, StructuredFailure, FileRunSummary, TestRunResponse, AffectedTestStatus, RootLease, ManagedRoot};
pub use linters::{parse_clippy_json_output, parse_biome_json_output, parse_eslint_json_output, parse_ruff_json_output, sort_targets_fail_first};
pub(crate) use env_python::{resolve_executable_on_path_in_dir, resolve_executable_on_path};
pub use cargo_mods::{rust_crate_dir_on_disk};
pub use cargo_output::{cargo_compile_timeout_for_policy};
pub use cargo_toml::{parse_rustup_override_dirs};
pub use cargo_features::{collect_rust_target_tree_files, rust_target_tree_may_have_dynamic_tests, rust_lib_has_potential_doctests, compute_cargo_enabled_features, workspace_needs_unified_cargo_features, cargo_file_url_to_path, cargo_required_features_satisfied, cargo_test_target_features_satisfied, normalize_cargo_config_args};
pub use cargo_targets::{collect_rust_test_enabled_examples, collect_rust_bin_roots_and_names, cargo_manifest_package_name_and_version, cargo_manifest_has_examples, parse_cargo_pkg_spec, resolve_cargo_package_dirs_via_metadata, cargo_version_spec_matches, collect_rust_integration_test_roots_and_names, cargo_manifest_package_name, discover_cargo_workspace_root_and_members, rust_lib_target_needs_direct_cargo_mode, rust_lib_target_needs_direct_cargo, rust_named_bin_needs_direct_cargo, rust_any_bin_has_dynamic_tests, rust_all_bins_need_direct_cargo_mode, rust_all_bins_need_direct_cargo, collect_rust_package_all_target_files_for_args, resolve_rust_target_trees_for_selector, resolve_rust_target_tree_files_for_selector};
pub use cargo_selectors::{collect_rust_fq_test_names_in_file, resolve_cargo_target_args_for_file, collect_rust_package_all_target_files, collect_rust_lib_target_files, collect_rust_all_bin_target_paths_for_args, collect_rust_all_bin_target_paths, resolve_rust_named_bin_target_files, resolve_rust_named_test_target_files, discover_cargo_workspace_member_dirs, cargo_manifest_has_lib, cargo_manifest_has_package, cargo_manifest_has_workspace, cargo_manifest_has_any_testable_target, resolve_rust_named_bin_target_path, resolve_rust_named_test_target_path};
pub use reporting::{extract_structured_failures, fold_stack_trace_noise, extract_missing_prerequisites, clean_path_or_target, strip_line_or_fragment_suffix, normalize_target_for_root, normalize_targets_for_root, compact_passed_delta_stdout, extract_warnings_summary, format_junit_xml_report, format_sarif_report};
