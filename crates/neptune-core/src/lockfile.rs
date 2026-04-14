use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// v0.3 lockfile schema 版本号
pub const LOCK_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    /// Schema 版本，用于未来兼容性检查
    pub lock_version: u32,
    /// 顶层 neptune.toml 的 SHA-256，用于检测 manifest 变更
    pub manifest_sha256: String,
    /// 所有包（包括间接依赖），按名称字典序排列
    pub packages: Vec<LockedPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub source: LockedSource,
    /// 包内容的 SHA-256（path 依赖为目录哈希，git 依赖为 HEAD commit hash）
    pub content_sha256: String,
    /// 该包的直接依赖列表（name + version，用于 npt tree 展示）
    #[serde(default)]
    pub dependencies: Vec<LockedDep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedDep {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LockedSource {
    #[serde(rename = "registry")]
    Registry { url: String, package: String },
    #[serde(rename = "git")]
    Git { url: String, rev: String },
    #[serde(rename = "path")]
    Path { path: String },
}

impl Lockfile {
    pub fn read_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("读取 lockfile 失败: {}", path.display()))?;
        let lf: Lockfile = toml::from_str(&s)
            .with_context(|| format!("解析 lockfile TOML 失败: {}", path.display()))?;
        Ok(lf)
    }

    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<()> {
        // 写入前确保 packages 按名称排序，保证 lockfile 内容稳定
        let mut sorted = self.clone();
        sorted.packages.sort_by(|a, b| a.name.cmp(&b.name));
        let s = toml::to_string_pretty(&sorted).context("序列化 lockfile 失败")?;
        crate::util::atomic_write(path.as_ref(), s.as_bytes())
    }

    /// 检查 lockfile 是否与当前 manifest 一致
    pub fn is_up_to_date(&self, manifest_sha: &str) -> bool {
        self.manifest_sha256 == manifest_sha && self.lock_version == LOCK_VERSION
    }

    /// 验证 lockfile 的完整性
    pub fn validate(&self) -> Result<()> {
        if self.lock_version == 0 {
            return Err(anyhow::anyhow!("lockfile lock_version 不合法"));
        }
        if self.manifest_sha256.is_empty() {
            return Err(anyhow::anyhow!("lockfile manifest_sha256 为空"));
        }
        for pkg in &self.packages {
            if pkg.name.is_empty() {
                return Err(anyhow::anyhow!("lockfile 中存在 name 为空的包"));
            }
            if pkg.version.is_empty() {
                return Err(anyhow::anyhow!("lockfile 中包 {} 的 version 为空", pkg.name));
            }
            // 修复 #1：验证 content_sha256 对所有包都不应为空。
            // v0.3.1 起，Git 依赖在 resolve 阶段就会填充精确 commit hash，
            // 若为空说明是旧版 lockfile 或代码错误。
            if pkg.content_sha256.is_empty() {
                return Err(anyhow::anyhow!(
                    "lockfile 中包 {} 的 content_sha256 为空（请删除 neptune.lock 并重新运行 npt install）",
                    pkg.name
                ));
            }
        }
        Ok(())
    }
}
