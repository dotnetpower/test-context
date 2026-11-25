use crate::policy::MemoryPolicy;
use crate::workspace::{
    DATA_FILE_EXTENSIONS, FRONTEND_CONFIG_FILES, FRONTEND_SOURCE_EXTENSIONS,
    POLYGLOT_CONFIG_FILES, POLYGLOT_SOURCE_EXTENSIONS, PYTEST_CONFIG_FILES, RUST_CONFIG_FILES,
    Workspace,
};
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;


mod types;
mod snapshot;
mod classify;
mod hashing;
mod graph_build;
mod data_refs;
mod query;
mod bridges;
mod semantic_hash;
mod manifests;
mod resolver_rust;
mod resolver_frontend;
mod resolver_frontend_resolve;
mod resolver_python;
mod parser_python_lex;
mod parser_python;
mod parser_rust;
mod parser_frontend_lex;
mod parser_frontend;
mod parser_css;
mod parser_polyglot;
mod polyglot;

#[cfg(test)]
mod tests;

use snapshot::*;
use classify::*;
use query::*;
use bridges::*;
use semantic_hash::*;
use manifests::*;
use resolver_rust::*;
use resolver_frontend::*;
use resolver_python::*;
use parser_python_lex::*;
use parser_rust::*;
use parser_frontend_lex::*;
use parser_polyglot::*;
use polyglot::*;

pub use types::{FileStamp, file_stamp, ImportSpec, RustModDecl, ParsedPythonFile, FileNode, AffectedTest, FileGraphInfo, DependencyGraph};
pub use snapshot::save_persisted_ast_snapshot;
pub use classify::{split_target_file_and_node_in_files, path_matches_testpaths, dir_could_match_testpaths, normalize_lexical_rel, is_frontend_source_path, is_frontend_setup_file, is_frontend_test_path, is_cypress_test_path, is_polyglot_source_path, is_polyglot_test_path, is_frontend_barrel_file, is_stylesheet_path, is_storybook_story_path, is_playwright_e2e_path, file_language_family, frontend_pkg_dir, is_rust_common_test_helper, rust_integration_test_target_name, is_rust_integration_test_path, conftest_scope_dir, is_test_filename, matches_test_filename, testpaths_allow_dir_prefix, normalize_rel_str};
pub use semantic_hash::{sha256_hex, rust_api_semantic_sha256, semantic_sha256_for_source};
pub use resolver_rust::{rust_crate_dir};
pub use resolver_frontend::{clean_frontend_import_specifier};
pub use parser_python::{parse_python_source, parse_python_source_with_patterns};
pub use parser_rust::{owning_rust_crate_dirs, is_valid_env_var_name, parse_rust_source, parse_rust_source_for_path};
pub use parser_frontend::{parse_frontend_source};
pub use parser_css::{parse_css_source, parse_css_source_for_path};
pub use parser_polyglot::parse_polyglot_source;
