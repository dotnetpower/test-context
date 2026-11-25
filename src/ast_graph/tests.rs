use super::*;

#[test]
fn parses_imports_fixtures_and_test_symbols() {
    let src = r#"
import os, sys as system
from pkg.sub import (
    alpha,
    beta as b,
)
from ..relative_mod import helper

# from fake.comment import ignored
DOC = """
import fake.in_docstring
def test_fake():
    pass
"""

@pytest.fixture(scope="module")
def sample_db(tmp_path):
    from lazy.mod import connect
    return connect(tmp_path)

async def test_create_user(sample_db, monkeypatch: object = None):
    mod = importlib.import_module("dynamic.plugin")
    assert sample_db is not None
"#;
    let parsed = parse_python_source(src);
    assert!(
        parsed
            .test_symbols
            .contains(&"test_create_user".to_string())
    );
    assert!(!parsed.test_symbols.contains(&"test_fake".to_string()));
    assert_eq!(parsed.fixtures_defined, vec!["sample_db".to_string()]);
    assert!(parsed.fixtures_used.contains(&"sample_db".to_string()));
    assert!(parsed.fixtures_used.contains(&"tmp_path".to_string()));
    assert!(parsed.imports.contains(&ImportSpec {
        level: 0,
        module: "pkg.sub".to_string(),
        symbols: vec!["alpha".to_string(), "beta".to_string()],
    }));
    assert!(parsed.imports.contains(&ImportSpec {
        level: 2,
        module: "relative_mod".to_string(),
        symbols: vec!["helper".to_string()],
    }));
    assert!(parsed.imports.contains(&ImportSpec {
        level: 0,
        module: "lazy.mod".to_string(),
        symbols: vec!["connect".to_string()],
    }));
    assert!(parsed.imports.contains(&ImportSpec {
        level: 0,
        module: "dynamic.plugin".to_string(),
        symbols: Vec::new(),
    }));
}

#[test]
fn parses_named_fixtures_usefixtures_varargs_nested_fstrings_and_data_refs() {
    let src = r#"
pytestmark = pytest.mark.usefixtures("global_fx")

@pytest.fixture(scope="session", name="aliased_db")
def _internal_db_builder(*args, seed_fx, **kwargs):
    msg = f"nested {inner("import fake_inside_fstring")} done"
    schema = "config/schema.json"
    return msg, schema

@pytest.mark.usefixtures("mark_fx1", "mark_fx2")
def test_with_varargs(first_fx, *args, kw_fx: int = 1, **kwargs):
    pass
"#;
    let parsed = parse_python_source(src);
    assert_eq!(parsed.fixtures_defined, vec!["aliased_db".to_string()]);
    assert!(parsed.fixtures_used.contains(&"global_fx".to_string()));
    assert!(parsed.fixtures_used.contains(&"mark_fx1".to_string()));
    assert!(parsed.fixtures_used.contains(&"mark_fx2".to_string()));
    assert!(parsed.fixtures_used.contains(&"seed_fx".to_string()));
    assert!(parsed.fixtures_used.contains(&"first_fx".to_string()));

    let unpacked_args = parse_python_source("def test_a(): __import__('pkg', *args)\n");
    assert!(
        unpacked_args
            .defined_symbols
            .contains(&"py_dynamic_import".to_string())
    );
    let unpacked_kwargs = parse_python_source("def test_b(): __import__('pkg', **kwargs)\n");
    assert!(
        unpacked_kwargs
            .defined_symbols
            .contains(&"py_dynamic_import".to_string())
    );
    let aliased_dunder = parse_python_source(
        "from builtins import __import__ as load\nm = load('pkg', fromlist=['feature'])\n",
    );
    assert!(
        aliased_dunder
            .imports
            .iter()
            .any(|imp| imp.module == "pkg" && imp.symbols.contains(&"feature".to_string()))
    );
    let str_fromlist = parse_python_source("m = __import__('pkg', fromlist='ab')\n");
    assert!(
        str_fromlist
            .imports
            .iter()
            .any(|imp| imp.module == "pkg"
                && imp.symbols.contains(&"a".to_string())
                && imp.symbols.contains(&"b".to_string()))
    );
    let grouped_str_fromlist = parse_python_source("m = __import__('pkg', fromlist=('ab'))\n");
    assert!(
        grouped_str_fromlist
            .imports
            .iter()
            .any(|imp| imp.module == "pkg"
                && imp.symbols.contains(&"a".to_string())
                && imp.symbols.contains(&"b".to_string()))
    );
    let tuple_fromlist = parse_python_source("m = __import__('pkg', fromlist=('ab',))\n");
    assert!(
        tuple_fromlist
            .imports
            .iter()
            .any(|imp| imp.module == "pkg" && imp.symbols == vec!["ab".to_string()])
    );
    let bound_shutil = parse_python_source("import shutil\ndef test_c():\n    cp = shutil.copy\n    cp('a', 'b')\n");
    assert!(bound_shutil.defined_symbols.contains(&"py_dynamic_io".to_string()));
    let dyn_shutil = parse_python_source("def test_d(): __import__('shutil').which('git')\n");
    assert!(dyn_shutil.defined_symbols.contains(&"py_dynamic_io".to_string()));
    let getattr_shutil = parse_python_source("import shutil\ndef test_e(): getattr(shutil, 'which')('tool')\n");
    assert!(getattr_shutil.defined_symbols.contains(&"py_dynamic_io".to_string()));
    let map_shutil = parse_python_source("import shutil\ndef test_f(): list(map(shutil.which, ['git']))\n");
    assert!(map_shutil.defined_symbols.contains(&"py_dynamic_io".to_string()));
    let rust_copy = parse_rust_source("use std::fs::copy;\n#[test]\nfn t() { let _ = copy(\"a\", \"b\"); }\n");
    assert!(rust_copy.defined_symbols.contains(&"rust_dynamic_io".to_string()));
    let rust_hl = parse_rust_source("use std::fs::hard_link;\n#[test]\nfn t() { let _ = hard_link(\"a\", \"b\"); }\n");
    assert!(rust_hl.defined_symbols.contains(&"rust_dynamic_io".to_string()));
    assert!(parsed.fixtures_used.contains(&"kw_fx".to_string()));
    assert!(!parsed.fixtures_used.contains(&"args".to_string()));
    assert!(!parsed.fixtures_used.contains(&"kwargs".to_string()));
    assert!(parsed.data_refs.contains(&"config/schema.json".to_string()));
    assert!(parsed.module_markers.contains(&"usefixtures".to_string()));
}

#[test]
fn isolates_monorepo_service_compartments_and_supports_hash_only_updates() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes("services/svc_a/src/app/config.py", b"VALUE = 1\n");
    graph.insert_file_bytes("services/svc_b/src/app/config.py", b"VALUE = 2\n");
    graph.insert_file_bytes(
        "services/svc_a/tests/test_a.py",
        b"from app.config import VALUE\ndef test_a(): assert VALUE == 1\n",
    );
    graph.insert_file_bytes(
        "services/svc_b/tests/test_b.py",
        b"from app.config import VALUE\ndef test_b(): assert VALUE == 2\n",
    );
    graph.recompute_edges();

    // Changing svc_a's config must only affect svc_a's test, never svc_b's test!
    let affected_a = graph.affected_tests(&["services/svc_a/src/app/config.py".to_string()]);
    assert_eq!(affected_a.len(), 1);
    assert_eq!(affected_a[0].test_file, "services/svc_a/tests/test_a.py");

    let hash_a_before = graph.dep_hash_for_test["services/svc_a/tests/test_a.py"].clone();
    let hash_b_before = graph.dep_hash_for_test["services/svc_b/tests/test_b.py"].clone();

    // Body-only change returns structure_changed == false and updates only affected dep_hash
    let structure_changed =
        graph.insert_file_bytes("services/svc_a/src/app/config.py", b"VALUE = 100\n");
    assert!(!structure_changed);
    graph.recompute_hashes_for_changed(&["services/svc_a/src/app/config.py".to_string()]);

    assert_ne!(
        graph.dep_hash_for_test["services/svc_a/tests/test_a.py"],
        hash_a_before
    );
    assert_eq!(
        graph.dep_hash_for_test["services/svc_b/tests/test_b.py"],
        hash_b_before
    );
}

#[test]
fn resolves_aks_store_demo_polyglot_hyphenated_subservice_imports() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes("src/ai-service/routers/__init__.py", b"");
    graph.insert_file_bytes(
        "src/ai-service/routers/description_generator.py",
        b"def generate_description(): return 'desc'\n",
    );
    graph.insert_file_bytes(
        "src/ai-service/routers/image_generator.py",
        b"def generate_image(): return 'img'\n",
    );
    graph.insert_file_bytes(
        "src/ai-service/main.py",
        b"from routers.description_generator import generate_description\nfrom routers.image_generator import generate_image\n",
    );
    graph.insert_file_bytes(
        "src/ai-service/tests/test_description.py",
        b"from routers.description_generator import generate_description\ndef test_desc(): assert generate_description() == 'desc'\n",
    );
    graph.insert_file_bytes(
        "src/ai-service/tests/test_image.py",
        b"from routers.image_generator import generate_image\ndef test_img(): assert generate_image() == 'img'\n",
    );
    graph.insert_file_bytes(
        "src/ai-service/tests/test_main.py",
        b"from main import generate_description\ndef test_app(): pass\n",
    );
    graph.recompute_edges();

    let affected =
        graph.affected_tests(&["src/ai-service/routers/description_generator.py".to_string()]);
    let test_files: Vec<String> = affected.into_iter().map(|a| a.test_file).collect();
    assert_eq!(
        test_files,
        vec![
            "src/ai-service/tests/test_description.py".to_string(),
            "src/ai-service/tests/test_main.py".to_string(),
        ]
    );
    let modules = graph
        .affected_modules(&["src/ai-service/routers/description_generator.py".to_string()]);
    assert!(modules.contains(&"src/ai-service/main.py".to_string()));
}

#[test]
fn honors_custom_pytest_python_files_patterns() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("pytest.ini"),
        "[pytest]\npython_files = check_*.py\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("check_feature.py"),
        "def test_custom():\n    assert True\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("test_default.py"),
        "def test_default():\n    assert True\n",
    )
    .unwrap();
    let workspace = Workspace::new(temp.path(), Vec::new(), Vec::new()).unwrap();
    let graph = DependencyGraph::build(&workspace, &MemoryPolicy::default()).unwrap();
    assert!(graph.dep_hash_for_test.contains_key("check_feature.py"));
    assert!(!graph.dep_hash_for_test.contains_key("test_default.py"));
}

#[test]
fn honors_multiline_python_files_and_testpaths() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\n\
python_files = [\n\
  \"check_*.py\",\n\
  \"*_spec.py\",\n\
]\n\
testpaths = [\n\
  \"tests/unit\",\n\
]\n",
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("tests/unit")).unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("tests/unit/check_feature.py"),
        "def test_feature():\n    assert True\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("tests/unit/other_spec.py"),
        "def test_other():\n    assert True\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/check_helper.py"),
        "def test_not_collected():\n    assert True\n",
    )
    .unwrap();
    let workspace = Workspace::new(temp.path(), Vec::new(), Vec::new()).unwrap();
    let graph = DependencyGraph::build(&workspace, &MemoryPolicy::default()).unwrap();
    assert!(
        graph
            .dep_hash_for_test
            .contains_key("tests/unit/check_feature.py")
    );
    assert!(
        graph
            .dep_hash_for_test
            .contains_key("tests/unit/other_spec.py")
    );
    assert!(!graph.dep_hash_for_test.contains_key("src/check_helper.py"));
}

#[test]
fn parses_rust_mods_use_trees_tests_markers_includes_and_env_vars() {
    let src = r###"
#![allow(dead_code)]
#![cfg_attr(test, parallel_safe)]

pub mod engine;
#[path = "custom_storage.rs"]
mod storage;

use crate::engine::{Runner, Config as EngineConfig, sub::{alpha, beta}};
use super::super::shared::helper;
use std::env;

/*
  /* nested comment with fake #[test] fn test_ignored_comment() {} */
*/
const RAW: &str = r#"
#[test]
fn test_inside_raw_string() {}
"#;

pub struct Service<'a> {
    name: &'a str,
}

#[tokio::test]
#[serial_test::serial]
async fn test_async_engine() {
    let sql = include_str!("../testdata/query.sql");
    let snap = "snapshots/engine.snap";
    let key = env!("API_SECRET_KEY");
    let dyn_key = env::var("RUNTIME_TOKEN").unwrap();
    assert!(!sql.is_empty() && !snap.is_empty() && !key.is_empty() && !dyn_key.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires external cluster"]
    fn test_unit_case() {
        let ch = '\'';
        assert_eq!(ch, '\'');
    }
}
"###;
    let parsed = parse_rust_source(src);
    assert_eq!(
        parsed.test_symbols,
        vec!["test_async_engine".to_string(), "test_unit_case".to_string()]
    );
    assert!(parsed.rust_mods.contains(&RustModDecl {
        name: "engine".to_string(),
        custom_path: None,
        is_inline: false,
        is_cfg_test: false,
    }));
    assert!(parsed.rust_mods.contains(&RustModDecl {
        name: "storage".to_string(),
        custom_path: Some("custom_storage.rs".to_string()),
        is_inline: false,
        is_cfg_test: false,
    }));
    assert!(parsed.rust_mods.contains(&RustModDecl {
        name: "tests".to_string(),
        custom_path: None,
        is_inline: true,
        is_cfg_test: true,
    }));
    assert!(parsed.imports.contains(&ImportSpec {
        level: 0,
        module: "crate.engine".to_string(),
        symbols: vec!["Config".to_string(), "Runner".to_string()],
    }));
    assert!(parsed.imports.contains(&ImportSpec {
        level: 0,
        module: "crate.engine.sub".to_string(),
        symbols: vec!["alpha".to_string(), "beta".to_string()],
    }));
    assert!(parsed.imports.contains(&ImportSpec {
        level: 2,
        module: "shared".to_string(),
        symbols: vec!["helper".to_string()],
    }));
    assert!(parsed.data_refs.contains(&"../testdata/query.sql".to_string()));
    assert!(parsed.data_refs.contains(&"snapshots/engine.snap".to_string()));
    assert!(parsed.markers.contains(&"serial".to_string()));
    assert!(parsed.markers.contains(&"ignore".to_string()));
    assert!(parsed.module_markers.contains(&"parallel_safe".to_string()));
    assert_eq!(
        parsed.env_vars_referenced,
        vec!["API_SECRET_KEY".to_string(), "RUNTIME_TOKEN".to_string()]
    );
    assert!(parsed.defined_symbols.contains(&"Service".to_string()));
}

#[test]
fn builds_rust_dependency_graph_with_unit_integration_common_helper_and_include_str() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "Cargo.toml",
        b"[package]\nname = \"sample-rust\"\nversion = \"0.1.0\"\n",
    );
    graph.insert_file_bytes(
        "src/lib.rs",
        b"pub mod math;\npub mod service;\npub mod isolated;\n",
    );
    graph.insert_file_bytes(
        "src/math.rs",
        b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_add_unit() { assert_eq!(add(1, 2), 3); }\n}\n",
    );
    graph.insert_file_bytes(
        "src/service.rs",
        b"use crate::math::add;\npub fn double(x: i32) -> i32 { add(x, x) }\n",
    );
    graph.insert_file_bytes("testdata/banner.txt", b"hello\n");
    graph.insert_file_bytes(
        "src/isolated.rs",
        b"pub fn banner() -> &'static str { include_str!(\"../testdata/banner.txt\") }\n",
    );
    graph.insert_file_bytes("tests/common/mod.rs", b"pub fn setup() -> i32 { 21 }\n");
    graph.insert_file_bytes(
        "tests/service_test.rs",
        b"mod common;\nuse sample_rust::service::double;\n#[test]\nfn test_service_double() { assert_eq!(double(common::setup()), 42); }\n",
    );
    graph.insert_file_bytes(
        "tests/isolated_test.rs",
        b"use sample_rust::isolated::banner;\n#[test]\nfn test_isolated_banner() { assert_eq!(banner().trim(), \"hello\"); }\n",
    );
    graph.recompute_edges();

    // 1. Editing `src/math.rs` affects `src/math.rs` (inline unit test) and `tests/service_test.rs`, NOT `tests/isolated_test.rs`!
    let affected_math: Vec<String> = graph
        .affected_tests(&["src/math.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_math,
        vec![
            "src/math.rs".to_string(),
            "tests/service_test.rs".to_string()
        ]
    );

    // 2. Editing `testdata/banner.txt` referenced via `include_str!("../testdata/banner.txt")` affects only `tests/isolated_test.rs`!
    let affected_banner: Vec<String> = graph
        .affected_tests(&["testdata/banner.txt".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_banner, vec!["tests/isolated_test.rs".to_string()]);

    // 3. Editing `tests/common/mod.rs` affects all integration tests under `tests/`!
    let affected_common: Vec<String> = graph
        .affected_tests(&["tests/common/mod.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_common,
        vec![
            "tests/isolated_test.rs".to_string(),
            "tests/service_test.rs".to_string()
        ]
    );
}

#[test]
fn excludes_type_checking_only_imports_and_tracks_referenced_env_vars_in_dep_hash() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes("pkg/__init__.py", b"");
    graph.insert_file_bytes("pkg/heavy_types.py", b"class HeavyModel:\n    pass\n");
    graph.insert_file_bytes("pkg/runtime_dep.py", b"def calc() -> int:\n    return 7\n");
    graph.insert_file_bytes(
        "pkg/service.py",
        b"from typing import TYPE_CHECKING\nimport os\nif TYPE_CHECKING:\n    from pkg.heavy_types import HeavyModel\nfrom pkg.runtime_dep import calc\n\ndef run() -> int:\n    flag = os.getenv(\"TCTX_TEST_ROUND4_FLAG\")\n    return calc() if not flag else 0\n",
    );
    graph.insert_file_bytes(
        "tests/test_service.py",
        b"from pkg.service import run\ndef test_run():\n    assert run() == 7\n",
    );
    graph.recompute_edges();

    // 1. `pkg/heavy_types.py` is imported only inside `if TYPE_CHECKING:`, so editing it must NOT affect `tests/test_service.py`!
    let affected_tc = graph.affected_tests(&["pkg/heavy_types.py".to_string()]);
    assert!(affected_tc.is_empty());
    assert_eq!(
        graph.files["pkg/service.py"].parsed.type_checking_imports.len(),
        1
    );

    // 2. `pkg/runtime_dep.py` is a runtime import, so editing it affects `tests/test_service.py`
    let affected_rt = graph.affected_tests(&["pkg/runtime_dep.py".to_string()]);
    assert_eq!(affected_rt.len(), 1);
    assert_eq!(affected_rt[0].test_file, "tests/test_service.py");

    // 3. Changing referenced env var `TCTX_TEST_ROUND4_FLAG` updates `dep_hash` for `tests/test_service.py`
    let hash_before = graph.dep_hash_for_test["tests/test_service.py"].clone();
    unsafe {
        std::env::set_var("TCTX_TEST_ROUND4_FLAG", "enabled_v2");
    }
    graph.refresh_referenced_env_hashes();
    let hash_after = graph.dep_hash_for_test["tests/test_service.py"].clone();
    unsafe {
        std::env::remove_var("TCTX_TEST_ROUND4_FLAG");
    }
    assert_ne!(hash_before, hash_after);
}

#[test]
fn links_pyo3_maturin_rust_extension_changes_to_python_pytest_files() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "pyproject.toml",
        b"[project]\nname = \"hybrid-pkg\"\n[tool.maturin]\nmodule-name = \"hybrid_pkg._native\"\n",
    );
    graph.insert_file_bytes(
        "Cargo.toml",
        b"[package]\nname = \"hybrid-pkg\"\n[lib]\nname = \"_native\"\ncrate-type = [\"cdylib\", \"rlib\"]\n",
    );
    graph.insert_file_bytes(
        "src/fast_kernel.rs",
        b"pub fn dot_product(a: &[f64], b: &[f64]) -> f64 { a.iter().zip(b).map(|(x, y)| x * y).sum() }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_dot_rust() { assert_eq!(dot_product(&[1.0, 2.0], &[3.0, 4.0]), 11.0); }\n}\n",
    );
    graph.insert_file_bytes(
        "src/lib.rs",
        b"pub mod fast_kernel;\nuse pyo3::prelude::*;\n#[pymodule]\nfn _native(m: &Bound<'_, PyModule>) -> PyResult<()> { Ok(()) }\n",
    );
    graph.insert_file_bytes(
        "python/hybrid_pkg/__init__.py",
        b"from ._native import dot_product\n",
    );
    graph.insert_file_bytes(
        "python/hybrid_pkg/pure_py.py",
        b"def greet() -> str:\n    return 'pure'\n",
    );
    graph.insert_file_bytes(
        "tests/test_hybrid.py",
        b"from hybrid_pkg import dot_product\ndef test_dot_py():\n    assert dot_product([1.0], [2.0]) == 2.0\n",
    );
    graph.insert_file_bytes(
        "tests/test_pure.py",
        b"from hybrid_pkg.pure_py import greet\ndef test_greet():\n    assert greet() == 'pure'\n",
    );
    graph.recompute_edges();

    // Editing `src/fast_kernel.rs` must affect BOTH the Rust unit test (`src/fast_kernel.rs`)
    // AND the Python pytest file (`tests/test_hybrid.py`), while leaving `tests/test_pure.py` untouched!
    let affected: Vec<String> = graph
        .affected_tests(&["src/fast_kernel.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected,
        vec![
            "src/fast_kernel.rs".to_string(),
            "tests/test_hybrid.py".to_string(),
        ]
    );

    // Symbol-based target resolution & fuzzy suggestion check
    assert_eq!(
        graph.resolve_target_test_files(&["test_dot_rust".to_string()]),
        vec!["src/fast_kernel.rs".to_string()]
    );
    let suggestions = graph.suggest_closest_targets(&["tests/test_hybrd.py".to_string()]);
    assert!(suggestions.contains(&"tests/test_hybrid.py".to_string()));
}

#[test]
fn parses_and_resolves_react_vite_tsx_barrel_alias_css_and_type_only_imports() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "tsconfig.json",
        b"{\n  \"compilerOptions\": {\n    \"paths\": {\n      \"@/*\": [\"./src/*\"]\n    }\n  }\n}\n",
    );
    graph.insert_file_bytes("src/test/setup.ts", b"import '@testing-library/jest-dom';\n");
    graph.insert_file_bytes(
        "src/types/models.ts",
        b"export interface ButtonProps { label: string; }\n",
    );
    graph.insert_file_bytes("src/assets/logo.svg", b"<svg></svg>\n");
    graph.insert_file_bytes(
        "src/components/Button.module.css",
        b".btn { color: blue; }\n",
    );
    graph.insert_file_bytes(
        "src/components/Button.tsx",
        b"import type { ButtonProps } from '@/types/models';\nimport styles from './Button.module.css';\nimport logoSvg from '@/assets/logo.svg?raw';\nexport function Button(props: ButtonProps) {\n  const api = import.meta.env.VITE_FRONTEND_API_URL;\n  return `${styles.btn}:${logoSvg}:${api}:${props.label}`;\n}\n",
    );
    graph.insert_file_bytes(
        "src/components/Modal.tsx",
        b"export function Modal() { return 'modal'; }\n",
    );
    graph.insert_file_bytes(
        "src/components/index.ts",
        b"export { Button } from './Button';\nexport { Modal } from './Modal';\nexport type { ButtonProps } from '@/types/models';\n",
    );
    graph.insert_file_bytes(
        "src/__tests__/__snapshots__/Button.test.tsx.snap",
        b"exports[`Button renders 1`] = `\"btn\"`;\n",
    );
    graph.insert_file_bytes(
        "src/__tests__/Button.test.tsx",
        b"import { Button } from '@/components';\ndescribe('Button suite', () => {\n  it.concurrent('renders primary button', () => {\n    expect(Button({ label: 'Click' })).toBeTruthy();\n  });\n});\n",
    );
    graph.insert_file_bytes(
        "src/__tests__/Modal.test.tsx",
        b"import { Modal } from '@/components';\ntest('renders modal dialog', () => {\n  expect(Modal()).toBe('modal');\n});\n",
    );
    graph.recompute_edges();

    // 1. Barrel tree-shaking: editing `Button.tsx` affects `Button.test.tsx`, NOT `Modal.test.tsx`!
    let affected_btn: Vec<String> = graph
        .affected_tests(&["src/components/Button.tsx".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_btn, vec!["src/__tests__/Button.test.tsx".to_string()]);

    // 2. CSS Module & Vite `?raw` SVG asset changes propagate to `Button.test.tsx`!
    let affected_css: Vec<String> = graph
        .affected_tests(&["src/components/Button.module.css".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_css, vec!["src/__tests__/Button.test.tsx".to_string()]);
    let affected_svg: Vec<String> = graph
        .affected_tests(&["src/assets/logo.svg".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_svg, vec!["src/__tests__/Button.test.tsx".to_string()]);

    // 3. `import type` / `export type` changes (`src/types/models.ts`) do NOT invalidate runtime tests!
    let affected_types = graph.affected_tests(&["src/types/models.ts".to_string()]);
    assert!(affected_types.is_empty());

    // 4. Snapshot file change affects `Button.test.tsx`!
    let affected_snap: Vec<String> = graph
        .affected_tests(&["src/__tests__/__snapshots__/Button.test.tsx.snap".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_snap,
        vec!["src/__tests__/Button.test.tsx".to_string()]
    );

    // 5. Global frontend setup file (`src/test/setup.ts`) affects all frontend tests!
    let affected_setup: Vec<String> = graph
        .affected_tests(&["src/test/setup.ts".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_setup,
        vec![
            "src/__tests__/Button.test.tsx".to_string(),
            "src/__tests__/Modal.test.tsx".to_string(),
        ]
    );

    // 6. Changing `VITE_FRONTEND_API_URL` referenced via `import.meta.env.VITE_FRONTEND_API_URL` updates Merkle `dep_hash`!
    let hash_before = graph.dep_hash_for_test["src/__tests__/Button.test.tsx"].clone();
    unsafe {
        std::env::set_var("VITE_FRONTEND_API_URL", "https://api.example.com");
    }
    graph.refresh_referenced_env_hashes();
    let hash_after = graph.dep_hash_for_test["src/__tests__/Button.test.tsx"].clone();
    unsafe {
        std::env::remove_var("VITE_FRONTEND_API_URL");
    }
    assert_ne!(hash_before, hash_after);

    let info = graph.file_graph_info("src/components/Button.tsx").unwrap();
    assert_eq!(info.language, "react_tsx");
}

#[test]
fn resolves_typescript_esm_js_extension_imports_in_own_vscode_extension() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = Workspace::new(repo_root, Vec::new(), Vec::new()).unwrap();
    let graph = DependencyGraph::build(&workspace, &MemoryPolicy::default()).unwrap();
    let affected: Vec<String> = graph
        .affected_tests(&["vscode-extension/src/resultFormatter.ts".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert!(
        affected.contains(&"vscode-extension/test/nativeTools.test.ts".to_string()),
        "expected nativeTools.test.ts in affected: {affected:?}"
    );
}

#[test]
fn resolves_scss_partials_subpath_imports_package_exports_glob_worker_and_storybook() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "package.json",
        br##"{
  "name": "@acme/design-system",
  "imports": {
"#internal/*": "./src/internal/*"
  },
  "exports": {
".": "./src/index.ts",
"./card": {
  "import": "./src/components/Card.tsx"
}
  }
}"##,
    );
    graph.insert_file_bytes(
        "tsconfig.json",
        b"{\n  \"extends\": \"./tsconfig.app.json\"\n}\n",
    );
    graph.insert_file_bytes(
        "tsconfig.app.json",
        b"{\n  \"compilerOptions\": {\n    \"paths\": {\n      \"@ui/*\": [\"./src/components/*\"]\n    }\n  }\n}\n",
    );
    graph.insert_file_bytes("src/styles/_tokens.scss", b"$primary: #0055ff;\n");
    graph.insert_file_bytes(
        "src/components/Card.module.scss",
        b"@use '../styles/tokens';\n.card { color: tokens.$primary; }\n",
    );
    graph.insert_file_bytes(
        "src/internal/format.ts",
        b"export function formatTitle(s: string) { return s.toUpperCase(); }\n",
    );
    graph.insert_file_bytes(
        "src/workers/render.worker.ts",
        b"export const WORKER_READY = true;\n",
    );
    graph.insert_file_bytes("src/plugins/chart.ts", b"export const chartPlugin = 1;\n");
    graph.insert_file_bytes(
        "src/components/Card.tsx",
        b"import styles from './Card.module.scss';\nimport { formatTitle } from '#internal/format';\nconst plugins = import.meta.glob('../plugins/*.ts');\nconst worker = new Worker(new URL('../workers/render.worker.ts', import.meta.url));\nexport function Card(title: string) {\n  return `${styles.card}:${formatTitle(title)}:${Object.keys(plugins).length}:${worker}`;\n}\n",
    );
    graph.insert_file_bytes(
        "src/components/Card.stories.tsx",
        b"import { Card } from '@ui/Card';\nexport default { component: Card };\nexport const Primary = {\n  play: async () => {\n    Card('Hello');\n  },\n};\n",
    );
    graph.insert_file_bytes(
        "e2e/card.e2e.ts",
        b"import { test, expect } from '@playwright/test';\nimport { Card } from '@acme/design-system/card';\ntest('renders card in browser', async () => {\n  expect(Card('E2E')).toBeTruthy();\n});\n",
    );
    graph.recompute_edges();

    // 1. Multi-hop SCSS partial chain: `_tokens.scss` -> `Card.module.scss` -> `Card.tsx` -> `Card.stories.tsx` & `e2e/card.e2e.ts`!
    let affected_scss: Vec<String> = graph
        .affected_tests(&["src/styles/_tokens.scss".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_scss,
        vec![
            "e2e/card.e2e.ts".to_string(),
            "src/components/Card.stories.tsx".to_string(),
        ]
    );

    // 2. `#internal/format` subpath import propagates to both tests!
    let affected_subpath: Vec<String> = graph
        .affected_tests(&["src/internal/format.ts".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_subpath.len(), 2);

    // 3. `import.meta.glob('../plugins/*.ts')` and `new URL('../workers/render.worker.ts', import.meta.url)` propagate!
    let affected_glob = graph.affected_tests(&["src/plugins/chart.ts".to_string()]);
    assert_eq!(affected_glob.len(), 2);
    let affected_worker =
        graph.affected_tests(&["src/workers/render.worker.ts".to_string()]);
    assert_eq!(affected_worker.len(), 2);

    // 4. Playwright E2E test has `playwright` and `e2e` markers automatically attached
    let e2e_node = &graph.files["e2e/card.e2e.ts"];
    assert!(e2e_node.parsed.markers.contains(&"playwright".to_string()));
    assert!(e2e_node.parsed.module_markers.contains(&"e2e".to_string()));
}

#[test]
fn semantic_hash_ignores_comments_and_granulizes_conftest_and_rust_pub_use_barrels() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "src/pkg/calc.py",
        b"def compute(x: int) -> int:\n    return x * 2\n",
    );
    graph.insert_file_bytes(
        "tests/conftest.py",
        b"import pytest\n\n@pytest.fixture\ndef db_conn():\n    return 'db'\n",
    );
    graph.insert_file_bytes(
        "tests/test_calc.py",
        b"from pkg.calc import compute\ndef test_compute():\n    assert compute(3) == 6\n",
    );
    graph.insert_file_bytes(
        "tests/test_db.py",
        b"def test_with_db(db_conn):\n    assert db_conn == 'db'\n",
    );
    graph.recompute_edges();

    let calc_hash_before = graph.dep_hash_for_test["tests/test_calc.py"].clone();
    // 1. Adding ordinary comments & blank lines to `src/pkg/calc.py` must NOT change `dep_hash`!
    graph.insert_file_bytes(
        "src/pkg/calc.py",
        b"# Header comment\n\ndef compute(x: int) -> int:  # inline comment\n    # body note\n    return x * 2\n\n",
    );
    graph.recompute_hashes_for_changed(&["src/pkg/calc.py".to_string()]);
    assert_eq!(
        graph.dep_hash_for_test["tests/test_calc.py"],
        calc_hash_before,
        "comment-only and blank-line edits must preserve semantic dep_hash"
    );

    // 2. Editing `tests/conftest.py` (which only defines non-autouse `db_conn`) affects `test_db.py` but NOT `test_calc.py`!
    let affected_cf: Vec<String> = graph
        .affected_tests(&["tests/conftest.py".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_cf, vec!["tests/test_db.py".to_string()]);

    // 3. Rust `pub use` barrel module (`src/models/mod.rs`) tree-shakes sibling modules!
    let mut rs_graph = DependencyGraph::new();
    rs_graph.insert_file_bytes(
        "src/models/user.rs",
        b"pub struct User;\n",
    );
    rs_graph.insert_file_bytes(
        "src/models/order.rs",
        b"pub struct Order;\n",
    );
    rs_graph.insert_file_bytes(
        "src/models/mod.rs",
        b"pub mod user;\npub mod order;\npub use self::user::User;\npub use self::order::Order;\n",
    );
    rs_graph.insert_file_bytes(
        "tests/user_test.rs",
        b"use crate::models::User;\n#[test]\nfn test_user() { let _ = User; }\n",
    );
    rs_graph.recompute_edges();
    let affected_user: Vec<String> = rs_graph
        .affected_tests(&["src/models/user.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_user, vec!["tests/user_test.rs".to_string()]);
    let affected_order = rs_graph.affected_tests(&["src/models/order.rs".to_string()]);
    assert!(
        affected_order.is_empty(),
        "unrelated sibling module in Rust barrel mod.rs must not invalidate user_test.rs"
    );
}

#[test]
fn resolves_nextjs_app_router_wasm_api_contracts_and_polyglot_cpp_csharp_proto() {
    let mut graph = DependencyGraph::new();
    // 1. Next.js App Router (`app/layout.tsx` -> `app/dashboard/page.tsx`)
    graph.insert_file_bytes(
        "app/layout.tsx",
        b"export default function RootLayout({ children }: any) { return children; }\n",
    );
    graph.insert_file_bytes(
        "app/dashboard/page.tsx",
        b"export default function DashboardPage() { return 'dash'; }\n",
    );
    graph.insert_file_bytes(
        "app/dashboard/page.test.tsx",
        b"import DashboardPage from './page';\ntest('renders dashboard', () => { DashboardPage(); });\n",
    );
    // 2. Full-Stack API route (`/api/v1/orders`) + Wasm (`#[wasm_bindgen]`) + React Context (`ThemeContext`)
    graph.insert_file_bytes(
        "src/backend/routes.py",
        b"@app.get('/api/v1/orders')\ndef list_orders(): return []\n",
    );
    graph.insert_file_bytes(
        "src/wasm/kernel.rs",
        b"#[wasm_bindgen]\npub fn fast_hash(input: &str) -> u32 { 42 }\n",
    );
    graph.insert_file_bytes(
        "src/context/Theme.tsx",
        b"import { createContext } from 'react';\nexport const ThemeContext = createContext('dark');\n",
    );
    graph.insert_file_bytes(
        "src/frontend/Orders.test.tsx",
        b"import { fast_hash } from '../wasm/pkg';\nimport { ThemeContext } from '../context/Theme';\ntest('fetches orders', async () => {\n  const theme = useContext(ThemeContext);\n  const res = await fetch('/api/v1/orders');\n  return fast_hash(theme);\n});\n",
    );
    // 3. C/C++ (relative, root-resolved, and `include/` -> `src/` headers) + C# (namespace `using`) + Protobuf
    graph.insert_file_bytes(
        "proto/order.proto",
        b"syntax = \"proto3\";\nmessage OrderRequest { string id = 1; }\n",
    );
    graph.insert_file_bytes("native/math.h", b"int add_vals(int a, int b);\n");
    graph.insert_file_bytes(
        "native/math.c",
        b"#include \"math.h\"\nint add_vals(int a, int b) { return a + b; }\n",
    );
    graph.insert_file_bytes("include/core.h", b"int core_mul(int a, int b);\n");
    graph.insert_file_bytes(
        "src/core.c",
        b"#include \"core.h\"\nint core_mul(int a, int b) { return a * b; }\n",
    );
    graph.insert_file_bytes(
        "native/math_test.cpp",
        b"#include \"native/math.h\"\n#include \"core.h\"\nTEST(MathTest, Adds) { add_vals(1, 2); core_mul(2, 3); }\n",
    );
    graph.insert_file_bytes(
        "dotnet/Pricing.cs",
        b"namespace Acme.Billing;\npublic class Pricing { public int Total() => 42; }\n",
    );
    graph.insert_file_bytes(
        "dotnet/PricingTests.cs",
        b"using Acme.Billing;\nnamespace Acme.Billing.Tests;\npublic class PricingTests {\n  [Fact]\n  public void ComputesTotal() {}\n}\n",
    );
    graph.insert_file_bytes(
        "Directory.Build.props",
        b"<Project><PropertyGroup><Nullable>enable</Nullable></PropertyGroup></Project>\n",
    );
    graph.insert_file_bytes("CMakeLists.txt", b"project(native)\n");
    graph.insert_file_bytes(
        "src/backend/test_proto_client.py",
        b"import order_pb2\ndef test_proto(): assert order_pb2 is not None\n",
    );
    graph.insert_file_bytes("src/backend/untested_orphan.py", b"def orphan_helper(): return 0\n");
    graph.recompute_edges();

    // Next.js `app/layout.tsx` change propagates to `app/dashboard/page.test.tsx`!
    let affected_layout: Vec<String> = graph
        .affected_tests(&["app/layout.tsx".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_layout,
        vec!["app/dashboard/page.test.tsx".to_string()]
    );

    // Backend route `/api/v1/orders` and Wasm `fast_hash` propagate to `src/frontend/Orders.test.tsx`!
    let affected_api: Vec<String> = graph
        .affected_tests(&["src/backend/routes.py".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_api,
        vec!["src/frontend/Orders.test.tsx".to_string()]
    );
    let affected_wasm: Vec<String> = graph
        .affected_tests(&["src/wasm/kernel.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_wasm,
        vec!["src/frontend/Orders.test.tsx".to_string()]
    );

    // C root-resolved `native/math.h` -> `native/math.c` AND `include/core.h` -> `src/core.c` propagate to `native/math_test.cpp`!
    let affected_c: Vec<String> = graph
        .affected_tests(&["native/math.c".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_c, vec!["native/math_test.cpp".to_string()]);
    let affected_c_mirrored: Vec<String> = graph
        .affected_tests(&["src/core.c".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_c_mirrored, vec!["native/math_test.cpp".to_string()]);

    // C# namespace `using Acme.Billing;` links `dotnet/Pricing.cs` -> `dotnet/PricingTests.cs`!
    let affected_cs: Vec<String> = graph
        .affected_tests(&["dotnet/Pricing.cs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_cs, vec!["dotnet/PricingTests.cs".to_string()]);

    // .NET / CMake / Go config isolation: editing `Directory.Build.props`, `CMakeLists.txt`, or `go.mod`
    // must NOT invalidate unrelated Python/Rust/Frontend tests!
    let affected_dotnet_cfg: Vec<String> = graph
        .affected_tests(&["Directory.Build.props".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_dotnet_cfg, vec!["dotnet/PricingTests.cs".to_string()]);
    let affected_cmake_cfg: Vec<String> = graph
        .affected_tests(&["CMakeLists.txt".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_cmake_cfg, vec!["native/math_test.cpp".to_string()]);
    let affected_go_mod: Vec<String> = graph
        .affected_tests(&["go.mod".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert!(affected_go_mod.is_empty(), "go.mod must not affect non-Go tests: {affected_go_mod:?}");

    // Protobuf `proto/order.proto` propagates to `src/backend/test_proto_client.py`!
    let affected_proto: Vec<String> = graph
        .affected_tests(&["proto/order.proto".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_proto,
        vec!["src/backend/test_proto_client.py".to_string()]
    );

    // Untested modified file detection
    let untested = graph.untested_modified_files(&[
        "src/backend/untested_orphan.py".to_string(),
        "native/math.c".to_string(),
    ]);
    assert_eq!(untested, vec!["src/backend/untested_orphan.py".to_string()]);
}

#[test]
fn discovers_nonstandard_c_inc_headers_under_include_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
    std::fs::create_dir_all(dir.path().join("native/include")).unwrap();
    std::fs::create_dir_all(dir.path().join("native/tests")).unwrap();
    std::fs::create_dir_all(dir.path().join("generated")).unwrap();
    std::fs::create_dir_all(dir.path().join("dotnet/tests")).unwrap();
    std::fs::write(dir.path().join("generated/gen.h"), "#define GEN 1\n").unwrap();
    std::fs::write(
        dir.path().join("native/include/defs.inc"),
        "#define MAGIC_VAL 7\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("native/tests/calc_test.cpp"),
        "#include <stdio.h>\n#include <defs.inc>\n#include \"generated/gen.h\"\nTEST(CalcTest, Magic) { int x = MAGIC_VAL; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("App.sln"),
        "Microsoft Visual Studio Solution File, Format Version 12.00\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dotnet/GlobalUsings.cs"),
        "global using Acme.Core;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dotnet/Engine.cs"),
        "namespace Acme.Old;\npublic class Engine {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dotnet/tests/EngineTests.cs"),
        "namespace Acme.Tests;\npublic class EngineTests {\n  [Fact]\n  public void Runs() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("test_py_ok.py"),
        "def test_ok():\n    assert True\n",
    )
    .unwrap();

    let ws = Workspace::new(dir.path(), vec![], vec![]).unwrap();
    let mut graph = DependencyGraph::build(&ws, &MemoryPolicy::default()).unwrap();
    assert!(
        !graph.discovery_budget_exceeded,
        "C/C++ system/ignored includes must not disable caching globally for Python/Rust/Frontend"
    );
    assert!(
        graph.files.contains_key("native/include/defs.inc"),
        "expected angle-bracket nonstandard .inc header to be discovered via data_refs"
    );
    assert!(
        graph.files.get("App.sln").is_some_and(|n| n.is_config),
        "expected .sln solution file to be indexed as config"
    );
    let affected_c: Vec<String> = graph
        .affected_tests(&["native/include/defs.inc".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_c, vec!["native/tests/calc_test.cpp".to_string()]);

    let affected_sln: Vec<String> = graph
        .affected_tests(&["App.sln".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_sln,
        vec!["dotnet/tests/EngineTests.cs".to_string()],
        ".sln config must affect only C# tests, not Python/C++ tests"
    );

    // Live incremental C# namespace update: moving `Engine.cs` into `Acme.Core` must rebuild
    // consumer edges through `GlobalUsings.cs` (`global using Acme.Core;`)!
    graph.insert_file_bytes(
        "dotnet/Engine.cs",
        b"namespace Acme.Core;\npublic class Engine {}\n",
    );
    graph.recompute_structure_incremental(&["dotnet/Engine.cs".to_string()]);
    let affected_cs_live: Vec<String> = graph
        .affected_tests(&["dotnet/Engine.cs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_cs_live,
        vec!["dotnet/tests/EngineTests.cs".to_string()]
    );
}

#[test]
fn parses_multiline_python_env_lookups_and_scopes_dotenv_to_python() {
    let src = concat!(
        "import os\n",
        "k1 = os.environ.get(\n",
        "    \"AZURE_OPENAI_API_KEY\"\n",
        ")\n",
        "k2 = os.getenv(\n",
        "    \"LOCAL_LLM_MODEL\",\n",
        "    \"default\",\n",
        ")\n",
        "k3 = os.environ[\n",
        "    \"APP_MODE\"\n",
        "]\n",
    );
    let parsed = parse_python_source(src);
    assert!(parsed.env_vars_referenced.contains(&"AZURE_OPENAI_API_KEY".to_string()));
    assert!(parsed.env_vars_referenced.contains(&"LOCAL_LLM_MODEL".to_string()));
    assert!(parsed.env_vars_referenced.contains(&"APP_MODE".to_string()));
    assert!(
        !parsed.defined_symbols.iter().any(|s| s == "py_dynamic_env"),
        "multiline literal env lookups must not emit py_dynamic_env: {:?}",
        parsed.defined_symbols
    );

    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes("svc/.env", b"APP_MODE=test\n");
    graph.insert_file_bytes("svc/test_env.py", b"def test_ok():\n    assert True\n");
    graph.insert_file_bytes("svc/lib.rs", b"#[test]\nfn ok() {}\n");
    graph.recompute_edges();
    let affected: Vec<String> = graph
        .affected_tests(&["svc/.env".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected, vec!["svc/test_env.py".to_string()]);
}

#[test]
fn normalizes_and_parses_escaped_multibyte_utf8_without_panicking() {
    let py_src = concat!(
        "import re\n",
        "CURRENCY_RE = re.compile(r\"[\\$€£\\¥\\₩\\🦀]+\")\n",
        "TRIPLE = \"\"\"prefix \\¥ \\가 \\🦀 suffix\"\"\"\n",
        "def test_currency():\n",
        "    assert CURRENCY_RE.match(\"¥100\")\n",
    );
    let py_hash_a = semantic_sha256_for_source("tests/test_currency.py", py_src.as_bytes());
    let py_hash_b = semantic_sha256_for_source(
        "tests/test_currency.py",
        format!("{py_src}# trailing comment with ¥ and 한글\n").as_bytes(),
    );
    assert_eq!(py_hash_a, py_hash_b);
    let parsed_py = parse_python_source(py_src);
    assert!(parsed_py.test_symbols.contains(&"test_currency".to_string()));

    let ts_src = concat!(
        "import { parsePrice } from './price';\n",
        "/* multi-byte block comment ¥ 가 🦀 */\n",
        "const pat = \"\\¥\\₩\\🦀\";\n",
        "const tpl = `\\¥\\₩\\🦀`;\n",
        "test('parses yen', () => { expect(parsePrice(pat, tpl)).toBe(100); });\n",
    );
    let ts_hash_a = semantic_sha256_for_source("src/price.test.ts", ts_src.as_bytes());
    let ts_hash_b = semantic_sha256_for_source(
        "src/price.test.ts",
        format!("{ts_src}// comment ¥\n").as_bytes(),
    );
    assert_eq!(ts_hash_a, ts_hash_b);
    let parsed_ts = parse_frontend_source(ts_src);
    assert!(parsed_ts.test_symbols.contains(&"parses yen".to_string()));

    let rs_src = concat!(
        "use crate::currency::parse_symbol;\n",
        "#[test]\n",
        "fn parses_yen_char() {\n",
        "    let ch = '¥';\n",
        "    let escaped = \"\\¥\";\n",
        "    assert_eq!(parse_symbol(ch, escaped), 1);\n",
        "}\n",
    );
    let rs_hash = semantic_sha256_for_source("src/lib.rs", rs_src.as_bytes());
    assert!(!rs_hash.is_empty());
    let parsed_rs = parse_rust_source(rs_src);
    assert!(parsed_rs.test_symbols.contains(&"parses_yen_char".to_string()));
}

#[test]
fn rust_lib_root_shared_types_do_not_couple_unrelated_sibling_modules() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "Cargo.toml",
        b"[package]\nname = \"sample\"\nversion = \"0.1.0\"\n",
    );
    graph.insert_file_bytes(
        "src/lib.rs",
        b"pub mod alpha;\npub mod beta;\npub type Result<T> = std::result::Result<T, String>;\n",
    );
    graph.insert_file_bytes(
        "src/alpha.rs",
        b"use crate::Result;\npub fn run_a() -> Result<i32> { Ok(1) }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_a() { assert_eq!(run_a().unwrap(), 1); }\n}\n",
    );
    graph.insert_file_bytes(
        "src/beta.rs",
        b"use crate::Result;\npub fn run_b() -> Result<i32> { Ok(2) }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_b() { assert_eq!(run_b().unwrap(), 2); }\n}\n",
    );
    graph.recompute_edges();

    let affected_alpha: Vec<String> = graph
        .affected_tests(&["src/alpha.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_alpha, vec!["src/alpha.rs".to_string()]);

    let affected_lib: Vec<String> = graph
        .affected_tests(&["src/lib.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_lib,
        vec!["src/alpha.rs".to_string(), "src/beta.rs".to_string()]
    );
}

#[test]
fn monorepo_subcrate_cargo_toml_and_target_resolution_stay_isolated() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "crates/alpha/Cargo.toml",
        b"[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n",
    );
    graph.insert_file_bytes(
        "crates/alpha/src/helper.rs",
        b"pub fn base_val() -> i32 { 7 }\n",
    );
    graph.insert_file_bytes(
        "crates/alpha/src/lib.rs",
        b"pub mod helper;\nuse crate::helper::base_val;\npub fn alpha_val() -> i32 { base_val() }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_alpha() { assert_eq!(alpha_val(), 7); }\n}\n",
    );
    graph.insert_file_bytes(
        "crates/beta/Cargo.toml",
        b"[package]\nname = \"beta\"\nversion = \"0.1.0\"\n",
    );
    graph.insert_file_bytes(
        "crates/beta/src/lib.rs",
        b"use alpha::alpha_val;\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn test_beta() { assert_eq!(alpha_val(), 7); }\n}\n",
    );
    graph.recompute_edges();

    let alpha_hash_before = graph.dep_hash_for_test["crates/alpha/src/lib.rs"].clone();
    let beta_hash_before = graph.dep_hash_for_test["crates/beta/src/lib.rs"].clone();

    // Editing crates/beta/Cargo.toml must NOT invalidate or mark crates/alpha affected!
    graph.insert_file_bytes(
        "crates/beta/Cargo.toml",
        b"[package]\nname = \"beta\"\nversion = \"0.2.0\"\n",
    );
    graph.recompute_hashes_for_changed(&["crates/beta/Cargo.toml".to_string()]);
    assert_eq!(
        graph.dep_hash_for_test["crates/alpha/src/lib.rs"],
        alpha_hash_before
    );
    assert_ne!(
        graph.dep_hash_for_test["crates/beta/src/lib.rs"],
        beta_hash_before
    );
    let affected_by_beta_toml: Vec<String> = graph
        .affected_tests(&["crates/beta/Cargo.toml".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_by_beta_toml,
        vec!["crates/beta/src/lib.rs".to_string()]
    );

    // Resolving a source file inside crates/alpha stays inside crates/alpha when same-crate tests exist,
    // while `affected_tests` still reports full cross-crate impact.
    assert_eq!(
        graph.resolve_target_test_files(&["crates/alpha/src/helper.rs".to_string()]),
        vec!["crates/alpha/src/lib.rs".to_string()]
    );
    let full_affected: Vec<String> = graph
        .affected_tests(&["crates/alpha/src/helper.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        full_affected,
        vec![
            "crates/alpha/src/lib.rs".to_string(),
            "crates/beta/src/lib.rs".to_string()
        ]
    );
}

#[test]
fn rust_inline_cfg_test_edits_and_runtime_json_strings_do_not_invalidate_downstream() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "Cargo.toml",
        b"[package]\nname = \"engine\"\nversion = \"0.1.0\"\n",
    );
    graph.insert_file_bytes(
        "src/lib.rs",
        b"pub mod core;\npub mod consumer;\n",
    );
    graph.insert_file_bytes(
        "src/core.rs",
        concat!(
            "pub fn compute() -> i32 {\n",
            "    let _state_path = \"data/autonomy/live_state.json\";\n",
            "    let _fixture = include_str!(\"../testdata/seed.json\");\n",
            "    42\n",
            "}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    use super::*;\n",
            "    #[test]\n",
            "    fn test_core() { assert_eq!(compute(), 42); }\n",
            "}\n",
        )
        .as_bytes(),
    );
    graph.insert_file_bytes(
        "src/consumer.rs",
        concat!(
            "use crate::core::compute;\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    use super::*;\n",
            "    #[test]\n",
            "    fn test_consumer() { assert_eq!(compute(), 42); }\n",
            "}\n",
        )
        .as_bytes(),
    );
    graph.recompute_edges();

    // 1. Production string literal `"data/autonomy/live_state.json"` is ignored while `include_str!` is captured!
    let core_refs = &graph.files["src/core.rs"].parsed.data_refs;
    assert!(!core_refs.iter().any(|r| r.contains("live_state.json")));
    assert!(core_refs.iter().any(|r| r.contains("testdata/seed.json")));

    let core_hash_before = graph.dep_hash_for_test["src/core.rs"].clone();
    let consumer_hash_before = graph.dep_hash_for_test["src/consumer.rs"].clone();

    // 2. Editing only inside `#[cfg(test)] mod tests` updates `src/core.rs`'s own dep_hash
    //    WITHOUT invalidating `src/consumer.rs` or marking `src/consumer.rs` affected!
    let structure_changed = graph.insert_file_bytes(
        "src/core.rs",
        concat!(
            "pub fn compute() -> i32 {\n",
            "    let _state_path = \"data/autonomy/live_state.json\";\n",
            "    let _fixture = include_str!(\"../testdata/seed.json\");\n",
            "    42\n",
            "}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    use super::*;\n",
            "    #[test]\n",
            "    fn test_core() { assert_eq!(compute() + 0, 42); }\n",
            "}\n",
        )
        .as_bytes(),
    );
    assert!(!structure_changed);
    graph.recompute_hashes_for_changed(&["src/core.rs".to_string()]);
    assert_ne!(graph.dep_hash_for_test["src/core.rs"], core_hash_before);
    assert_eq!(
        graph.dep_hash_for_test["src/consumer.rs"],
        consumer_hash_before,
        "downstream importer dep_hash must remain unchanged when only #[cfg(test)] body changed"
    );
    // Stateless `affected_tests` remains complete for git-diff queries while `dep_hash` keeps `src/consumer.rs` cached!
    let affected_inline: Vec<String> = graph
        .affected_tests(&["src/core.rs".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(
        affected_inline,
        vec!["src/consumer.rs".to_string(), "src/core.rs".to_string()]
    );

    // 3. Editing non-test public implementation DOES invalidate `src/consumer.rs`!
    graph.insert_file_bytes(
        "src/core.rs",
        concat!(
            "pub fn compute() -> i32 {\n",
            "    let _state_path = \"data/autonomy/live_state.json\";\n",
            "    let _fixture = include_str!(\"../testdata/seed.json\");\n",
            "    43 - 1\n",
            "}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    use super::*;\n",
            "    #[test]\n",
            "    fn test_core() { assert_eq!(compute() + 0, 42); }\n",
            "}\n",
        )
        .as_bytes(),
    );
    graph.recompute_hashes_for_changed(&["src/core.rs".to_string()]);
    assert_ne!(
        graph.dep_hash_for_test["src/consumer.rs"],
        consumer_hash_before
    );
}

#[test]
fn ts_prefix_wildcard_and_vite_declaration_order_alias_precedence() {
    // 1. TypeScript wildcard precedence ranks by PREFIX length only (`foo/*` beats `f*o/bar`
    //    on `foo/xo/bar`), even when both targets exist on disk!
    let mut ts_graph = DependencyGraph::new();
    ts_graph.insert_file_bytes(
        "tsconfig.json",
        br#"{
          "compilerOptions": {
            "paths": {
              "f*o/bar": ["src/wrong/*.ts"],
              "foo/*": ["src/right/*.ts"],
              "@fb/*": ["generated/*", "src/fallback/*"]
            }
          }
        }"#,
    );
    ts_graph.insert_file_bytes("src/right/xo/bar.ts", b"export const v = 1;\n");
    ts_graph.insert_file_bytes("src/wrong/oo/x.ts", b"export const v = 2;\n");
    ts_graph.insert_file_bytes("src/fallback/item.ts", b"export const x = 3;\n");
    ts_graph.insert_file_bytes(
        "src/a.test.ts",
        b"import { v } from 'foo/xo/bar';\ntest('ok', () => { expect(v).toBe(1); });\n",
    );
    ts_graph.insert_file_bytes(
        "src/fb.test.ts",
        b"import { x } from '@fb/item';\ntest('fb', () => { expect(x).toBe(3); });\n",
    );
    ts_graph.recompute_edges();
    let affected_right: Vec<String> = ts_graph
        .affected_tests(&["src/right/xo/bar.ts".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_right, vec!["src/a.test.ts".to_string()]);
    let affected_wrong: Vec<String> = ts_graph
        .affected_tests(&["src/wrong/oo/x.ts".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert!(affected_wrong.is_empty());
    assert!(!ts_graph.untrackable_tests.contains("src/a.test.ts"));
    // Ordered fallback `["generated/*", "src/fallback/*"]`: first target `generated/item.ts`
    // is unindexed/gitignored, so `src/fb.test.ts` must fail closed (untrackable)!
    assert!(ts_graph.untrackable_tests.contains("src/fb.test.ts"));

    // 2. Vite aliases apply in EXACT declaration order (`@lib` before `@lib/secret` resolves
    //    `@lib/secret` through `@lib` -> `src/first/secret.ts`, NOT `src/second/secret.ts`!).
    let mut vite_graph = DependencyGraph::new();
    vite_graph.insert_file_bytes(
        "vite.config.ts",
        br#"import path from 'path';
export default {
  resolve: {
alias: {
  '@lib': path.resolve(__dirname, './src/first'),
  '@lib/secret': path.resolve(__dirname, './src/second/secret.ts'),
},
  },
};
"#,
    );
    vite_graph.insert_file_bytes("src/first/secret.ts", b"export const s = 1;\n");
    vite_graph.insert_file_bytes("src/second/secret.ts", b"export const s = 2;\n");
    vite_graph.insert_file_bytes(
        "src/v.test.ts",
        b"import { s } from '@lib/secret';\ntest('v', () => { expect(s).toBe(1); });\n",
    );
    vite_graph.recompute_edges();
    let affected_first: Vec<String> = vite_graph
        .affected_tests(&["src/first/secret.ts".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert_eq!(affected_first, vec!["src/v.test.ts".to_string()]);
    let affected_second: Vec<String> = vite_graph
        .affected_tests(&["src/second/secret.ts".to_string()])
        .into_iter()
        .map(|a| a.test_file)
        .collect();
    assert!(affected_second.is_empty());
}

#[test]
fn pure_rust_dep_hash_tracks_cargo_lock_and_ignores_python_env_or_plain_pyproject() {
    let mut graph = DependencyGraph::new();
    graph.insert_file_bytes(
        "pyproject.toml",
        b"[project]\nname = \"py-app\"\nversion = \"0.1.0\"\n",
    );
    graph.insert_file_bytes(
        "Cargo.toml",
        b"[package]\nname = \"rs-core\"\nversion = \"0.1.0\"\n",
    );
    graph.insert_file_bytes("Cargo.lock", b"version = 3\n[[package]]\nname = \"serde\"\n");
    graph.insert_file_bytes(
        "src/lib.rs",
        b"#[cfg(test)]\nmod tests {\n    #[test]\n    fn test_rs() { assert_eq!(2 + 2, 4); }\n}\n",
    );
    graph.insert_file_bytes(
        "tests/test_py.py",
        b"def test_py():\n    assert 1 + 1 == 2\n",
    );
    graph.environment_hash = "py-env-v1".to_string();
    graph.non_python_environment_hash = "rust-env-v1".to_string();
    graph.recompute_edges();

    let rs_before = graph.dep_hash_for_test["src/lib.rs"].clone();
    let py_before = graph.dep_hash_for_test["tests/test_py.py"].clone();

    // Python environment rotation invalidates Python tests but preserves pure Rust dep_hash.
    graph.apply_environment_hashes("py-env-v2".to_string(), "rust-env-v1".to_string());
    assert_eq!(graph.dep_hash_for_test["src/lib.rs"], rs_before);
    assert_ne!(graph.dep_hash_for_test["tests/test_py.py"], py_before);

    // Plain (non-maturin) pyproject.toml change affects Python tests only, not pure Rust.
    let py_after_env = graph.dep_hash_for_test["tests/test_py.py"].clone();
    graph.insert_file_bytes(
        "pyproject.toml",
        b"[project]\nname = \"py-app\"\nversion = \"0.2.0\"\n",
    );
    graph.recompute_hashes_for_changed(&["pyproject.toml".to_string()]);
    assert_eq!(graph.dep_hash_for_test["src/lib.rs"], rs_before);
    assert_ne!(graph.dep_hash_for_test["tests/test_py.py"], py_after_env);

    // Cargo.lock change invalidates Rust tests without invalidating pure Python tests.
    let py_after_pyproject = graph.dep_hash_for_test["tests/test_py.py"].clone();
    graph.insert_file_bytes(
        "Cargo.lock",
        b"version = 3\n[[package]]\nname = \"serde\"\nversion = \"1.0.219\"\n",
    );
    graph.recompute_hashes_for_changed(&["Cargo.lock".to_string()]);
    assert_ne!(graph.dep_hash_for_test["src/lib.rs"], rs_before);
    assert_eq!(
        graph.dep_hash_for_test["tests/test_py.py"],
        py_after_pyproject
    );
}
