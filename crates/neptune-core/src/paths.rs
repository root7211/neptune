use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "neptune.toml";
pub const LOCK_FILE: &str = "neptune.lock";
pub const PROJECT_DIR: &str = ".neptune";
pub const MODULES_DIR: &str = "nelua_modules";
pub const PATH_BOOTSTRAP_FILE: &str = "neptune_path.lua";

pub fn project_dir(root: &Path) -> PathBuf {
    root.join(PROJECT_DIR)
}

pub fn pkgs_dir(root: &Path) -> PathBuf {
    project_dir(root).join("pkgs")
}

pub fn bin_dir(root: &Path) -> PathBuf {
    project_dir(root).join("bin")
}

pub fn modules_dir(root: &Path) -> PathBuf {
    root.join(MODULES_DIR)
}

pub fn path_bootstrap_file(root: &Path) -> PathBuf {
    root.join(PATH_BOOTSTRAP_FILE)
}

/// Git 依赖缓存目录：.neptune/cache/git
pub fn git_cache_dir(root: &Path) -> PathBuf {
    project_dir(root).join("cache").join("git")
}
