use std::{fs, io::Write, path::Path};

use anyhow::{Context, Result};

/// 原子写入：使用随机临时文件名，写入后 fsync，再 rename 到目标路径。
/// 修复了 v0.2 中固定 .tmp 后缀导致的并发竞争问题，并补充了 fsync。
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("创建目录失败: {}", parent.display()))?;

    // 使用 tempfile 在同一目录下创建随机命名的临时文件，避免并发冲突
    let mut tmp = tempfile::Builder::new()
        .prefix(".neptune-tmp-")
        .tempfile_in(parent)
        .with_context(|| format!("创建临时文件失败: {}", parent.display()))?;

    tmp.write_all(data)
        .with_context(|| "写入临时文件失败".to_string())?;

    // fsync 确保数据落盘，防止崩溃后文件损坏
    tmp.as_file().sync_all().context("fsync 临时文件失败")?;

    // persist() 将临时文件原子地移动到目标路径
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("原子替换失败 -> {}: {}", path.display(), e.error))?;

    // 修复 #6：对 parent 目录调用 fsync，确保 rename 操作本身也落盘。
    // 在断电/内核崩溃场景下，若不 fsync 目录，rename 可能丢失，
    // 导致目标文件消失（旧文件已被替换，新文件尚未持久化到目录项）。
    // 注意：在 Windows 上 fsync 目录会失败，此处静默忽略该错误。
    let dir_file = fs::File::open(parent)
        .with_context(|| format!("打开 parent 目录失败（用于 fsync）: {}", parent.display()))?;
    let _ = dir_file.sync_all(); // Windows 上忽略错误

    Ok(())
}

/// 创建目录（包括所有父目录）
pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("创建目录失败: {}", path.display()))
}

pub fn sha256_of_bytes(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

pub fn sha256_of_file(path: &Path) -> Result<String> {
    let data = std::fs::read(path)
        .with_context(|| format!("读取文件失败（用于 sha256）: {}", path.display()))?;
    Ok(sha256_of_bytes(&data))
}

/// 计算目录内所有文件内容的稳定哈希（按路径排序后拼接）
/// 用于 path 依赖的内容指纹，比仅哈希 manifest 更准确
pub fn sha256_of_dir(dir: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut entries = collect_files(dir)?;
    entries.sort();
    for (rel_path, abs_path) in &entries {
        hasher.update(rel_path.as_bytes());
        hasher.update(b"\0");
        let data = std::fs::read(abs_path)
            .with_context(|| format!("读取文件失败（目录 sha256）: {}", abs_path.display()))?;
        hasher.update(&data);
        hasher.update(b"\0");
    }
    Ok(hex::encode(hasher.finalize()))
}

fn collect_files(dir: &Path) -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut result = Vec::new();
    for abs in walkdir_sorted(dir)? {
        if abs.is_file() {
            let rel = abs
                .strip_prefix(dir)
                .unwrap_or(&abs)
                .to_string_lossy()
                .to_string();
            result.push((rel, abs));
        }
    }
    Ok(result)
}

fn walkdir_sorted(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut result = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current.is_dir() {
            let mut children: Vec<_> = std::fs::read_dir(&current)
                .with_context(|| format!("读取目录失败: {}", current.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            children.sort();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        } else {
            result.push(current);
        }
    }
    Ok(result)
}

/// 计算目录内所有文件内容的稳定哈希，排除指定的子目录/文件名
pub fn sha256_of_dir_filtered(dir: &Path, excludes: &[&str]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut entries = collect_files_filtered(dir, excludes)?;
    entries.sort();
    for (rel_path, abs_path) in &entries {
        hasher.update(rel_path.as_bytes());
        hasher.update(b"\0");
        let data = std::fs::read(abs_path)
            .with_context(|| format!("读取文件失败（目录 sha256）: {}", abs_path.display()))?;
        hasher.update(&data);
        hasher.update(b"\0");
    }
    Ok(hex::encode(hasher.finalize()))
}

fn collect_files_filtered(
    dir: &Path,
    excludes: &[&str],
) -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut result = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current.is_dir() {
            // 检查是否在排除列表中
            if let Some(name) = current.file_name().and_then(|n| n.to_str()) {
                if excludes.contains(&name) {
                    continue;
                }
            }
            let mut children: Vec<_> = std::fs::read_dir(&current)
                .with_context(|| format!("读取目录失败: {}", current.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            children.sort();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        } else {
            // 检查文件名是否在排除列表中
            if let Some(name) = current.file_name().and_then(|n| n.to_str()) {
                if excludes.contains(&name) {
                    continue;
                }
            }
            let rel = current
                .strip_prefix(dir)
                .unwrap_or(&current)
                .to_string_lossy()
                .to_string();
            result.push((rel, current));
        }
    }
    Ok(result)
}
