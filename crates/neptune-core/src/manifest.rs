use std::{collections::BTreeMap, path::Path};

use anyhow::{anyhow, Context, Result};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub authors: Option<Vec<String>>,
    pub repository: Option<String>,

    pub entry: Entry,

    #[serde(default)]
    pub dependencies: BTreeMap<String, DepSpec>,

    #[serde(rename = "dev-dependencies", default)]
    pub dev_dependencies: BTreeMap<String, DepSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Entry {
    pub app: Option<String>,
    pub lib: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DepSpec {
    VersionReq(String),
    Detailed {
        version: Option<String>,
        git: Option<String>,
        rev: Option<String>,
        tag: Option<String>,
        branch: Option<String>,
        path: Option<String>,
        registry: Option<String>,
        optional: Option<bool>,
    },
}

impl DepSpec {
    pub fn as_version_req(&self) -> Option<VersionReq> {
        match self {
            DepSpec::VersionReq(s) => s.parse().ok(),
            DepSpec::Detailed {
                version: Some(v), ..
            } => v.parse().ok(),
            _ => None,
        }
    }
}

impl Manifest {
    pub fn read_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("读取 manifest 失败: {}", path.display()))?;
        let m: Manifest =
            toml::from_str(&s).with_context(|| format!("解析 TOML 失败: {}", path.display()))?;
        Ok(m)
    }

    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<()> {
        let s = toml::to_string_pretty(self).context("序列化 manifest 失败")?;
        crate::util::atomic_write(path.as_ref(), s.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        // name
        if self.name.trim().is_empty() {
            return Err(anyhow!("manifest.name 不能为空"));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(anyhow!(
                "manifest.name 只能包含小写字母/数字/连字符(-): {}",
                self.name
            ));
        }

        // version
        let _v: Version = self
            .version
            .parse()
            .map_err(|_| anyhow!("manifest.version 不是合法 semver: {}", self.version))?;

        // entry
        if self.entry.app.is_none() && self.entry.lib.is_none() {
            return Err(anyhow!("[entry] 至少需要 app 或 lib 之一"));
        }

        // deps
        for (name, spec) in self.dependencies.iter().chain(self.dev_dependencies.iter()) {
            if name.trim().is_empty() {
                return Err(anyhow!("依赖名不能为空"));
            }
            match spec {
                DepSpec::VersionReq(v) => {
                    v.parse::<VersionReq>()
                        .map_err(|_| anyhow!("依赖 {} 的版本范围不合法: {}", name, v))?;
                }
                DepSpec::Detailed {
                    version,
                    git,
                    rev,
                    tag,
                    branch,
                    path,
                    ..
                } => {
                    if let Some(v) = version {
                        v.parse::<VersionReq>()
                            .map_err(|_| anyhow!("依赖 {} 的版本范围不合法: {}", name, v))?;
                    }
                    if git.is_some() && path.is_some() {
                        return Err(anyhow!("依赖 {} 不能同时设置 git 与 path", name));
                    }
                    let mut c = 0;
                    if rev.is_some() {
                        c += 1;
                    }
                    if tag.is_some() {
                        c += 1;
                    }
                    if branch.is_some() {
                        c += 1;
                    }
                    if c > 1 {
                        return Err(anyhow!("依赖 {} 的 rev/tag/branch 只能设置一个", name));
                    }
                }
            }
        }

        Ok(())
    }
}
