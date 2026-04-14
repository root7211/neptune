use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use fs2::FileExt;

pub struct DirLock {
    _file: fs::File,
    pub lock_path: PathBuf,
}

/// 对指定目录加排他锁（使用 .npt-lock 文件）
pub fn lock_dir(dir: &Path) -> Result<DirLock> {
    ensure_dir(dir)?;
    let lock_path = dir.join(".npt-lock");
    let f = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("打开锁文件失败: {}", lock_path.display()))?;
    f.lock_exclusive()
        .with_context(|| format!("获取目录锁失败: {}", dir.display()))?;
    Ok(DirLock {
        _file: f,
        lock_path,
    })
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("创建目录失败: {}", path.display()))
}

pub fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("读取元数据失败: {}", path.display())),
        Ok(meta) => {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                std::fs::remove_dir_all(path)
                    .with_context(|| format!("删除目录失败: {}", path.display()))?;
            } else {
                std::fs::remove_file(path)
                    .with_context(|| format!("删除文件/符号链接失败: {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// 创建目录符号链接（Unix）或 Junction Point（Windows）
/// 返回 true 表示创建了链接，false 表示需要回退到复制
pub fn symlink_dir(src: &Path, dst: &Path) -> Result<bool> {
    if let Some(p) = dst.parent() {
        ensure_dir(p)?;
    }
    remove_if_exists(dst)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs as unix_fs;
        unix_fs::symlink(src, dst).with_context(|| {
            format!("创建 symlink 失败: {} -> {}", dst.display(), src.display())
        })?;
        return Ok(true);
    }

    #[cfg(windows)]
    {
        // 优先尝试目录符号链接
        if std::os::windows::fs::symlink_dir(src, dst).is_ok() {
            return Ok(true);
        }
        // 回退到 Junction Point（不需要特殊权限）
        if create_junction(src, dst).is_ok() {
            return Ok(true);
        }
        return Ok(false);
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = src;
        let _ = dst;
        Ok(false)
    }
}

/// 递归复制目录（只复制普通文件和目录，跳过符号链接避免循环）
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    ensure_dir(dst)?;
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("读取目录失败: {}", src.display()))?
    {
        let entry = entry?;
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ft.is_file() {
            if let Some(parent) = to.parent() {
                ensure_dir(parent)?;
            }
            std::fs::copy(&from, &to).with_context(|| {
                format!("复制文件失败: {} -> {}", from.display(), to.display())
            })?;
        }
        // 跳过符号链接，避免循环引用
    }
    Ok(())
}

/// 根据 lockfile 中记录的包 ID 精确定位已安装的包目录
/// 修复了 v0.2 中使用字典序第一个子目录的脆弱逻辑
pub fn find_pkg_dir(pkgs_root: &Path, pkg_name: &str, pkg_id: &str) -> Option<PathBuf> {
    let target = pkgs_root.join(pkg_name).join(pkg_id);
    if target.exists() {
        Some(target)
    } else {
        None
    }
}

/// 列出指定目录下的所有子目录（按名称排序）
pub fn list_child_dirs(parent: &Path) -> Result<Vec<PathBuf>> {
    if !parent.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(parent)
        .with_context(|| format!("读取目录失败: {}", parent.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    entries.sort();
    Ok(entries)
}

/// 保留向后兼容的 first_child_dir
pub fn first_child_dir(parent: &Path) -> Result<Option<PathBuf>> {
    Ok(list_child_dirs(parent)?.into_iter().next())
}

#[cfg(windows)]
fn create_junction(src: &Path, dst: &Path) -> Result<()> {
    let status = std::process::Command::new("cmd")
        .args([
            "/C", "mklink", "/J",
            &dst.to_string_lossy(),
            &src.to_string_lossy(),
        ])
        .status()
        .context("执行 mklink /J 失败")?;
    if !status.success() {
        return Err(anyhow::anyhow!("mklink /J 失败"));
    }
    Ok(())
}
