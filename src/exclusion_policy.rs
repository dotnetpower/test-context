use std::path::Path;

pub const UNIVERSAL_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    "target",
    "dist",
    "build",
    "out",
    "coverage",
    "htmlcov",
    "__pycache__",
    "__pypackages__",
    "venv",
    "env",
    ".venv",
    ".tox",
    ".nox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    ".direnv",
    ".eggs",
    "site-packages",
];

pub fn is_excluded_dir_name(name: &str) -> bool {
    if name.starts_with('.') && name != "." {
        return true;
    }
    if name.ends_with(".egg-info") || name.starts_with("bazel-") || name.starts_with("cmake-build-")
    {
        return true;
    }
    UNIVERSAL_EXCLUDED_DIRS.contains(&name)
}

pub fn is_excluded_rel_path(rel_path: &Path) -> bool {
    for component in rel_path.components() {
        if let std::path::Component::Normal(os) = component
            && let Some(name) = os.to_str()
            && is_excluded_dir_name(name)
        {
            return true;
        }
    }
    false
}
