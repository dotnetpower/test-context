use serde_json::{Value, json};

pub const MAX_MESSAGE: usize = 2 * 1024 * 1024;

pub fn tools() -> Value {
    json!({
        "tools": [
            {
                "name": "test_run",
                "description": "MCP fallback for sessions where the matching Native tool is unavailable. Use this before shell pytest, cargo test, vitest, jest, or node --test in a registered workspace. Resolves target test files (or expands modified Python, Rust, or Frontend source/stylesheet files via the AST reverse dependency graph), checks live SHA-256 Merkle dependency hashes (dep_hash), skips tests that already passed in the background or prior runs, joins in-flight background runs, and executes only the uncached delta. Returns instant results (< 5ms) when all targeted tests are already cached green.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Optional absolute registered workspace/worktree root. Always pass explicitly when known (especially in subagents, Windows/WSL Agent Sessions, and multi-root setups); omit to resolve from the current workspace."
                        },
                        "targets": {
                            "type": "array",
                            "maxItems": 128,
                            "items": { "type": "string" },
                            "description": "Optional relative test file paths, test directories, test nodeids (path::test_name), or modified Python/Rust/Frontend source file paths. Omit to target all workspace tests."
                        },
                        "k": {
                            "type": "string",
                            "description": "Optional test name/expression filter (-k / -t)."
                        },
                        "mark": {
                            "type": "string",
                            "description": "Optional pytest -m marker filter."
                        },
                        "rerun_failed": {
                            "type": "boolean",
                            "default": true,
                            "description": "Re-run tests whose cached result for the current dep_hash was failed."
                        },
                        "no_cache": {
                            "type": "boolean",
                            "default": false,
                            "description": "Bypass cached pass results and force execution."
                        },
                        "ruff_fix": {
                            "type": "boolean",
                            "default": false,
                            "description": "Auto-fix safe Ruff/ESLint lint and import issues (ruff check --fix / eslint --fix) on targeted files before running tests."
                        },
                        "pytest_args": {
                            "type": "array",
                            "maxItems": 32,
                            "items": { "type": "string" },
                            "description": "Optional additional test runner CLI flags."
                        }
                    }
                },
                "annotations": {
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "openWorldHint": false
                }
            },
            {
                "name": "test_affected",
                "description": "MCP fallback for sessions where the matching Native tool is unavailable. Find which Python (pytest), Rust (cargo test), or Frontend (Vitest/Jest/Node/Storybook) test files are impacted by changed source files or git working-tree edits via the AST reverse dependency graph, along with each test file's dependency chain, live background cache state (cached_passed, cached_failed, running_in_background, queued_in_background, uncached), and pre-checked Ruff/ESLint diagnostics.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Optional absolute registered workspace root."
                        },
                        "files": {
                            "type": "array",
                            "maxItems": 128,
                            "items": { "type": "string" },
                            "description": "Optional relative paths of modified Python, Rust, Frontend, stylesheet, or config files. If omitted, uses git status dirty files when git_diff=true."
                        },
                        "git_diff": {
                            "type": "boolean",
                            "default": true,
                            "description": "Use git working-tree dirty files when files is empty."
                        }
                    }
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": false
                }
            },
            {
                "name": "test_graph",
                "description": "Inspect the AST dependency graph for a specific Python, Rust, or Frontend (TS/JS/JSX/TSX/Vue/Svelte/CSS/SCSS) source or test file: direct imports, reverse importers, transitive dependencies, conftest.py / tests/common / setupTests.ts chains, affected test files, test symbols, fixtures, and Merkle dep_hash.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Optional absolute registered workspace root."
                        },
                        "file": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Relative path to the Python, Rust, or Frontend file within the workspace."
                        }
                    },
                    "required": ["file"]
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": false
                }
            },
            {
                "name": "test_status",
                "description": "Show test-context daemon health and workspace residency immediately. Set load=true to load an inactive graph and include detailed AST, cache, queue, and saved-time counts.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Optional absolute registered workspace root."
                        },
                        "load": {
                            "type": "boolean",
                            "default": false,
                            "description": "Load an inactive workspace graph to include detailed file and cache counts. False returns registered/unloaded state immediately."
                        }
                    }
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": false
                }
            },
            {
                "name": "test_sync",
                "description": "Synchronize the Python AST dependency graph with saved files on disk and queue any uncached or modified tests for background execution. Set wait=true to wait for background execution to finish.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Optional absolute registered workspace root."
                        },
                        "wait": {
                            "type": "boolean",
                            "default": false,
                            "description": "Wait for queued background test runs to finish before returning."
                        }
                    }
                },
                "annotations": {
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "openWorldHint": false
                }
            },
            {
                "name": "test_invalidate",
                "description": "Invalidate cached test results for specific test/source files (or the entire workspace when files is omitted) and schedule background re-execution.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Optional absolute registered workspace root."
                        },
                        "files": {
                            "type": "array",
                            "maxItems": 128,
                            "items": { "type": "string" },
                            "description": "Optional relative test or source file paths to invalidate. Omit to invalidate all cached results in the workspace."
                        }
                    }
                },
                "annotations": {
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "openWorldHint": false
                }
            },
            {
                "name": "test_workspaces",
                "description": "List explicitly registered test-context workspace roots, scopes, globs, and detected pytest commands.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                },
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": false
                }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tools_are_described_as_native_unavailable_fallbacks() {
        let tools = tools();
        let entries = tools["tools"].as_array().expect("MCP tools array");
        for name in ["test_run", "test_affected"] {
            let description = entries
                .iter()
                .find(|tool| tool["name"].as_str() == Some(name))
                .and_then(|tool| tool["description"].as_str())
                .expect("MCP tool description");
            assert!(
                description.starts_with(
                    "MCP fallback for sessions where the matching Native tool is unavailable."
                ),
                "{name} did not preserve Native-first routing: {description}"
            );
        }
    }
}
