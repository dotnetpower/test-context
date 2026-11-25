use serde_json::{Value, json};

pub const MAX_MESSAGE: usize = 2 * 1024 * 1024;

pub fn tools() -> Value {
    json!({
        "tools": [
            {
                "name": "test_run",
                "description": "MCP fallback for sessions where the matching Native tool is unavailable. Run incremental tests (pytest, cargo test, vitest, jest, node --test, go test) via AST graph & dep_hash cache; unchanged passes return in <5ms, only uncached deltas run.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "root": {
                            "type": "string",
                            "description": "Absolute registered workspace/worktree root (pass explicitly when known)."
                        },
                        "targets": {
                            "type": "array",
                            "maxItems": 128,
                            "items": { "type": "string" },
                            "description": "Relative test files, directories, nodeids (path::test), or modified source files. Omit to run workspace tests (when >64 test files and no k/mark filter, scopes to git-dirty or recent-commit affected tests and falls back to all tests if none; pass [\".\"] for full suite)."
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
                            "description": "Re-run tests whose cached result for the current dep_hash failed."
                        },
                        "no_cache": {
                            "type": "boolean",
                            "default": false,
                            "description": "Bypass cached passes and force execution."
                        },
                        "ruff_fix": {
                            "type": "boolean",
                            "default": false,
                            "description": "Auto-fix safe lint issues on targeted files before running tests."
                        },
                        "pytest_args": {
                            "type": "array",
                            "maxItems": 32,
                            "items": { "type": "string" },
                            "description": "Optional additional runner CLI flags."
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
                "description": "MCP fallback for sessions where the matching Native tool is unavailable. List tests affected by modified files or git diff, with dependency chains, cache states, and cached lint diagnostics.",
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
                            "description": "Modified relative paths; omit to use git dirty files when git_diff=true."
                        },
                        "git_diff": {
                            "type": "boolean",
                            "default": true,
                            "description": "Use git dirty files when files is empty."
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
                "description": "Inspect AST dependency graph for one file: imports, reverse importers, transitive deps, setup/conftest chain, affected tests, symbols, and dep_hash.",
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
                            "description": "Relative file path within the workspace."
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
                "description": "Show daemon health and workspace residency immediately; set load=true to load an inactive graph.",
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
                            "description": "Load inactive graph to include detailed counts."
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
                "description": "Sync AST dependency graph with disk and queue uncached tests for background execution.",
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
                            "description": "Wait for queued background runs to finish."
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
                "description": "Invalidate cached test results for given files (or whole workspace) and schedule background re-runs.",
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
                            "description": "Relative paths to invalidate; omit for whole workspace."
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
                "description": "List registered test-context workspace roots, scopes, globs, and test commands.",
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
