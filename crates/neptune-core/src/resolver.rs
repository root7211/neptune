//! v0.3.1 依赖解析器（Resolver）
//!
//! 相比 v0.3.0 的改动：
//!   - 引入 PackageId（name + source_fingerprint），去重时同名不同来源会被检测并报错（修复 #3）
//!   - 冲突检测拆分为两类：来源冲突（同名不同 path/git）+ semver 版本约束冲突（修复 #4）
//!   - topological_sort 重构：消除重复 in_degree 计算，使用邻接表使 pop 时复杂度降为 O(E)（修复 #5）
//!   - Git 依赖在 resolve 阶段就 clone 并 rev-parse HEAD，将精确 commit hash 写入 lockfile（修复 #1）

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use semver::{Version, VersionReq};

use crate::{
    lockfile::{LockedDep, LockedPackage, LockedSource},
    manifest::{DepSpec, Manifest},
    paths,
    util,
};

// ─────────────────────────────────────────────────────────────────────────────
// 核心数据结构
// ─────────────────────────────────────────────────────────────────────────────

/// 包的唯一标识：名称 + 来源指纹。
///
/// 来源指纹对 path 依赖为规范化绝对路径，对 git 依赖为 "url#rev"。
/// 同名但来源不同的包拥有不同的 PackageId，会触发来源冲突报错。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageId {
    pub name: String,
    pub source_fingerprint: String,
}

impl PackageId {
    fn for_path(name: &str, abs_path: &Path) -> Self {
        Self {
            name: name.to_string(),
            source_fingerprint: abs_path.to_string_lossy().to_string(),
        }
    }

    fn for_git(name: &str, url: &str, commit: &str) -> Self {
        Self {
            name: name.to_string(),
            source_fingerprint: format!("git+{}#{}", url, commit),
        }
    }
}

/// 解析过程中的中间节点，代表一个已解析的包
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub name: String,
    pub version: String,
    pub source: ResolvedSource,
    /// 该包的直接依赖（name -> spec）
    pub direct_deps: BTreeMap<String, DepSpec>,
    /// 该包的 PackageId（用于去重和冲突检测）
    pub id: PackageId,
}

/// 解析后的来源信息
#[derive(Debug, Clone)]
pub enum ResolvedSource {
    Path { abs_path: PathBuf },
    /// v0.3.1：rev 字段已是 resolve 阶段确定的精确 commit hash
    Git { url: String, rev: String },
}

/// 冲突信息
#[derive(Debug, Clone)]
pub struct Conflict {
    pub package_name: String,
    pub kind: ConflictKind,
    pub demands: Vec<ConflictDemand>,
}

#[derive(Debug, Clone)]
pub enum ConflictKind {
    /// 同名包来自不同来源（path/git url 不同）
    SourceMismatch,
    /// 同名包被要求满足互不兼容的 semver 版本约束
    VersionIncompatible,
}

#[derive(Debug, Clone)]
pub struct ConflictDemand {
    pub requester_chain: String,
    pub version_req: String,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ConflictKind::SourceMismatch => {
                writeln!(f, "来源冲突：包 \"{}\" 被多个依赖从不同来源引入：", self.package_name)?;
            }
            ConflictKind::VersionIncompatible => {
                writeln!(f, "版本冲突：包 \"{}\" 被多个依赖以不兼容的版本要求引入：", self.package_name)?;
            }
        }
        for d in &self.demands {
            writeln!(f, "  - {} 要求 {}", d.requester_chain, d.version_req)?;
        }
        Ok(())
    }
}

/// 解析结果
pub struct ResolveResult {
    /// 按拓扑排序后的所有包（依赖先于被依赖者）
    pub packages: Vec<ResolvedNode>,
    /// 检测到的冲突列表（非空时调用方应报错）
    pub conflicts: Vec<Conflict>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Resolver
// ─────────────────────────────────────────────────────────────────────────────

pub struct Resolver {
    root: PathBuf,
}

impl Resolver {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self { root: root.as_ref().to_path_buf() }
    }

    /// 从顶层 manifest 开始，BFS 递归解析所有依赖，返回解析结果。
    pub fn resolve(&self, manifest: &Manifest) -> Result<ResolveResult> {
        // resolved: PackageId -> ResolvedNode
        let mut resolved: HashMap<PackageId, ResolvedNode> = HashMap::new();
        // name_to_id: 包名 -> 首次解析时的 PackageId（用于检测同名不同来源）
        let mut name_to_id: HashMap<String, PackageId> = HashMap::new();
        // demands: 包名 -> Vec<(引入链, req_str)>（用于 semver 冲突检测）
        let mut demands: HashMap<String, Vec<(String, String)>> = HashMap::new();

        // 队列：(包名, DepSpec, 引入者链路, 解析基准目录)
        let mut queue: VecDeque<(String, DepSpec, String, PathBuf)> = VecDeque::new();

        for (name, spec) in &manifest.dependencies {
            queue.push_back((name.clone(), spec.clone(), format!("root -> {}", name), self.root.clone()));
        }

        let mut conflicts: Vec<Conflict> = Vec::new();

        while let Some((pkg_name, dep_spec, chain, base_dir)) = queue.pop_front() {
            let req_str = dep_spec_to_req_string(&dep_spec);
            demands.entry(pkg_name.clone()).or_default().push((chain.clone(), req_str.clone()));

            // 解析来源，得到 PackageId（对 git 依赖会在此阶段 clone 并锁定 commit）
            let pkg_id = match self.dep_spec_to_package_id(&pkg_name, &dep_spec, &base_dir) {
                Ok(id) => id,
                Err(e) => return Err(e.context(format!("解析依赖 {} 失败（来自 {}）", pkg_name, chain))),
            };

            // 检测同名不同来源冲突
            if let Some(existing_id) = name_to_id.get(&pkg_name) {
                if existing_id != &pkg_id {
                    conflicts.push(Conflict {
                        package_name: pkg_name.clone(),
                        kind: ConflictKind::SourceMismatch,
                        demands: vec![
                            ConflictDemand {
                                requester_chain: "（已记录来源）".to_string(),
                                version_req: existing_id.source_fingerprint.clone(),
                            },
                            ConflictDemand {
                                requester_chain: chain.clone(),
                                version_req: pkg_id.source_fingerprint.clone(),
                            },
                        ],
                    });
                }
                // 无论是否冲突，同名包已处理过，跳过
                continue;
            }

            // 首次见到这个包名，记录 id 并解析
            name_to_id.insert(pkg_name.clone(), pkg_id.clone());

            let node = self.resolve_one(&pkg_name, &dep_spec, &base_dir)
                .with_context(|| format!("解析依赖 {} 失败（来自 {}）", pkg_name, chain))?;

            // 将子依赖入队，基准目录为该包的实际路径
            let node_base = match &node.source {
                ResolvedSource::Path { abs_path } => abs_path.clone(),
                ResolvedSource::Git { url, rev } => {
                    // Git 依赖的子依赖：使用 clone 后的缓存目录作为基准
                    let cache_dir = paths::project_dir(&self.root).join("cache").join("git");
                    cache_dir.join(format!("{}-{}", sanitize_for_path(url), sanitize_for_path(rev)))
                }
            };

            for (child_name, child_spec) in &node.direct_deps {
                let child_chain = format!("{} -> {}", chain, child_name);
                queue.push_back((child_name.clone(), child_spec.clone(), child_chain, node_base.clone()));
            }

            resolved.insert(pkg_id, node);
        }

        // semver 版本约束冲突检测（仅对有 semver req 的包）
        for (pkg_name, pkg_demands) in &demands {
            if pkg_demands.len() <= 1 {
                continue;
            }
            let resolved_version = name_to_id
                .get(pkg_name)
                .and_then(|id| resolved.get(id))
                .map(|n| n.version.as_str())
                .unwrap_or("0.0.0");

            let mut has_semver_conflict = false;
            let mut conflict_demands = Vec::new();

            for (chain, req_str) in pkg_demands {
                if let Ok(req) = req_str.parse::<VersionReq>() {
                    if let Ok(ver) = resolved_version.parse::<Version>() {
                        if !req.matches(&ver) {
                            has_semver_conflict = true;
                        }
                    }
                }
                conflict_demands.push(ConflictDemand {
                    requester_chain: chain.clone(),
                    version_req: req_str.clone(),
                });
            }

            if has_semver_conflict {
                let already_reported = conflicts.iter().any(|c| c.package_name == *pkg_name);
                if !already_reported {
                    conflicts.push(Conflict {
                        package_name: pkg_name.clone(),
                        kind: ConflictKind::VersionIncompatible,
                        demands: conflict_demands,
                    });
                }
            }
        }

        // 拓扑排序（O(V+E)）
        let packages = topological_sort(resolved)?;

        Ok(ResolveResult { packages, conflicts })
    }

    /// 根据 DepSpec 计算 PackageId。
    /// 对 git 依赖：在此阶段 clone 并 rev-parse HEAD，将精确 commit hash 作为指纹。
    fn dep_spec_to_package_id(&self, name: &str, spec: &DepSpec, base_dir: &Path) -> Result<PackageId> {
        match spec {
            DepSpec::VersionReq(_) => Err(anyhow!(
                "v0.3 原型暂不支持 registry 依赖（{}），请使用 path 或 git",
                name
            )),
            DepSpec::Detailed { path: Some(p), .. } => {
                let abs = base_dir.join(p).canonicalize()
                    .with_context(|| format!("path 依赖 {} 不存在: {}", name, p))?;
                Ok(PackageId::for_path(name, &abs))
            }
            DepSpec::Detailed { git: Some(url), rev, tag, branch, .. } => {
                let lock_rev = rev.as_deref()
                    .or(tag.as_deref())
                    .or(branch.as_deref())
                    .unwrap_or("HEAD");
                // clone/fetch 并锁定 commit hash
                let commit = self.git_ensure_and_resolve(url, lock_rev)?;
                Ok(PackageId::for_git(name, url, &commit))
            }
            _ => Err(anyhow!("依赖 {} 必须设置 path 或 git", name)),
        }
    }

    /// clone 或复用缓存，返回精确 commit hash
    fn git_ensure_and_resolve(&self, url: &str, rev: &str) -> Result<String> {
        let cache_dir = paths::project_dir(&self.root).join("cache").join("git");
        util::ensure_dir(&cache_dir)?;
        let repo_dir = cache_dir.join(format!("{}-{}", sanitize_for_path(url), sanitize_for_path(rev)));

        if !repo_dir.exists() {
            git_clone_and_checkout(url, rev, &repo_dir)?;
        } else {
            git_checkout_rev(&repo_dir, rev)?;
        }

        git_rev_parse_head(&repo_dir)
    }

    /// 解析单个依赖，返回 ResolvedNode（不递归）
    fn resolve_one(&self, name: &str, spec: &DepSpec, base_dir: &Path) -> Result<ResolvedNode> {
        match spec {
            DepSpec::VersionReq(v) => Err(anyhow!(
                "v0.3 原型暂不支持 registry 依赖（{} = \"{}\"）；registry 支持将在 v0.4 实现",
                name, v
            )),
            DepSpec::Detailed { git, rev, tag, branch, path, version, .. } => {
                if let Some(p) = path {
                    self.resolve_path_dep(name, p, version.as_deref(), base_dir)
                } else if let Some(url) = git {
                    self.resolve_git_dep(name, url, rev.as_deref(), tag.as_deref(), branch.as_deref(), version.as_deref())
                } else {
                    Err(anyhow!("依赖 {} 必须设置 path 或 git", name))
                }
            }
        }
    }

    fn resolve_path_dep(
        &self,
        name: &str,
        rel_path: &str,
        declared_version: Option<&str>,
        base_dir: &Path,
    ) -> Result<ResolvedNode> {
        let abs = base_dir.join(rel_path).canonicalize()
            .with_context(|| format!("path 依赖 {} 不存在或无法访问: {}", name, rel_path))?;

        let (version, direct_deps) = if abs.join(paths::MANIFEST_FILE).exists() {
            let dep_manifest = Manifest::read_from(abs.join(paths::MANIFEST_FILE))
                .with_context(|| format!("读取依赖 {} 的 manifest 失败", name))?;
            (dep_manifest.version.clone(), dep_manifest.dependencies.clone())
        } else {
            (declared_version.unwrap_or("0.0.0").to_string(), BTreeMap::new())
        };

        let id = PackageId::for_path(name, &abs);
        Ok(ResolvedNode { name: name.to_string(), version, source: ResolvedSource::Path { abs_path: abs }, direct_deps, id })
    }

    fn resolve_git_dep(
        &self,
        name: &str,
        url: &str,
        rev: Option<&str>,
        tag: Option<&str>,
        branch: Option<&str>,
        declared_version: Option<&str>,
    ) -> Result<ResolvedNode> {
        let lock_rev = rev.map(|s| s.to_string())
            .or_else(|| tag.map(|s| s.to_string()))
            .or_else(|| branch.map(|s| s.to_string()))
            .unwrap_or_else(|| "HEAD".to_string());

        // 在 resolve 阶段就锁定 commit hash（dep_spec_to_package_id 已经 clone，这里复用缓存）
        let commit_hash = self.git_ensure_and_resolve(url, &lock_rev)?;

        let cache_dir = paths::project_dir(&self.root).join("cache").join("git");
        let repo_dir = cache_dir.join(format!("{}-{}", sanitize_for_path(url), sanitize_for_path(&lock_rev)));

        // 读取 git 依赖包自身的 manifest（如果存在），获取真实版本和子依赖
        let (version, direct_deps) = if repo_dir.join(paths::MANIFEST_FILE).exists() {
            let dep_manifest = Manifest::read_from(repo_dir.join(paths::MANIFEST_FILE))
                .with_context(|| format!("读取 git 依赖 {} 的 manifest 失败", name))?;
            (dep_manifest.version.clone(), dep_manifest.dependencies.clone())
        } else {
            (declared_version.unwrap_or("0.0.0").to_string(), BTreeMap::new())
        };

        let id = PackageId::for_git(name, url, &commit_hash);
        Ok(ResolvedNode {
            name: name.to_string(),
            version,
            // rev 已是精确 commit hash
            source: ResolvedSource::Git { url: url.to_string(), rev: commit_hash },
            direct_deps,
            id,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 公共辅助函数
// ─────────────────────────────────────────────────────────────────────────────

/// 将 ResolvedNode 转换为 LockedPackage（content_sha256 由调用方填充）
pub fn node_to_locked_package(node: &ResolvedNode, content_sha256: String) -> LockedPackage {
    let source = match &node.source {
        ResolvedSource::Path { abs_path } => LockedSource::Path {
            path: abs_path.to_string_lossy().to_string(),
        },
        ResolvedSource::Git { url, rev } => LockedSource::Git {
            url: url.clone(),
            rev: rev.clone(), // v0.3.1：rev 已是精确 commit hash
        },
    };

    let dependencies: Vec<LockedDep> = node.direct_deps.keys()
        .map(|dep_name| LockedDep { name: dep_name.clone(), version: String::new() })
        .collect();

    LockedPackage { name: node.name.clone(), version: node.version.clone(), source, content_sha256, dependencies }
}

/// 计算 path 依赖目录的内容哈希
pub fn compute_path_content_hash(abs_path: &Path) -> Result<String> {
    if abs_path.join(paths::MANIFEST_FILE).exists() {
        util::sha256_of_dir_filtered(abs_path, &[".neptune", "neptune.lock", "nelua_modules"])
    } else {
        Ok(util::sha256_of_bytes(abs_path.to_string_lossy().as_bytes()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 拓扑排序（Kahn 算法，O(V+E)）
// ─────────────────────────────────────────────────────────────────────────────

/// 对解析结果进行拓扑排序，确保依赖先于被依赖者输出。
///
/// v0.3.1 修复（#5）：
///   - 消除了 v0.3.0 中重复构建 in_degree 的冗余代码
///   - 使用预先构建的 dependents 邻接表，pop 时只遍历直接依赖者（O(E) 而非 O(n²)）
fn topological_sort(mut nodes: HashMap<PackageId, ResolvedNode>) -> Result<Vec<ResolvedNode>> {
    // 建立 name -> id 映射，用于从 direct_deps（按名称）查找 PackageId
    let name_to_id: HashMap<String, PackageId> = nodes.iter()
        .map(|(id, node)| (node.name.clone(), id.clone()))
        .collect();

    // 一次遍历同时构建 in_degree 和 dependents 邻接表
    let mut in_degree: HashMap<PackageId, usize> = nodes.keys().map(|k| (k.clone(), 0)).collect();
    let mut dependents: HashMap<PackageId, Vec<PackageId>> = HashMap::new();

    for (id, node) in &nodes {
        for dep_name in node.direct_deps.keys() {
            if let Some(dep_id) = name_to_id.get(dep_name) {
                *in_degree.entry(id.clone()).or_insert(0) += 1;
                dependents.entry(dep_id.clone()).or_default().push(id.clone());
            }
        }
    }

    // 初始队列：所有入度为 0 的包，按名称排序保证稳定输出
    let mut queue: VecDeque<PackageId> = {
        let mut zero: Vec<PackageId> = in_degree.iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(id, _)| id.clone())
            .collect();
        zero.sort_by(|a, b| a.name.cmp(&b.name));
        zero.into()
    };

    let mut result = Vec::new();

    while let Some(id) = queue.pop_front() {
        let node = nodes.remove(&id).unwrap();

        // 只遍历依赖 id 的包（总计 O(E)），减少其入度
        if let Some(deps_on_me) = dependents.get(&id) {
            let mut next_ready: Vec<PackageId> = Vec::new();
            for dependent_id in deps_on_me {
                if let Some(deg) = in_degree.get_mut(dependent_id) {
                    if *deg > 0 {
                        *deg -= 1;
                        if *deg == 0 {
                            next_ready.push(dependent_id.clone());
                        }
                    }
                }
            }
            next_ready.sort_by(|a, b| a.name.cmp(&b.name));
            queue.extend(next_ready);
        }

        result.push(node);
    }

    if !nodes.is_empty() {
        let cycle_names: Vec<_> = nodes.values().map(|n| n.name.as_str()).collect();
        return Err(anyhow!("检测到循环依赖，涉及包：{}", cycle_names.join(", ")));
    }

    Ok(result)
}

// ─────────────────────────────────────────────────────────────────────────────
// Git 辅助函数
// ─────────────────────────────────────────────────────────────────────────────

fn git_clone_and_checkout(url: &str, rev: &str, dir: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["clone", url])
        .arg(dir)
        .status()
        .context("执行 git clone 失败（请确认已安装 git 且网络可用）")?;
    if !status.success() {
        return Err(anyhow!("git clone 失败: {}", url));
    }
    git_checkout_rev(dir, rev)
}

fn git_checkout_rev(dir: &Path, rev: &str) -> Result<()> {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(["checkout", "--quiet", rev])
        .status()
        .context("执行 git checkout 失败")?;

    if !status.success() {
        let _ = std::process::Command::new("git")
            .current_dir(dir)
            .args(["fetch", "--all", "--tags"])
            .status();

        let status2 = std::process::Command::new("git")
            .current_dir(dir)
            .args(["checkout", "--quiet", rev])
            .status()
            .context("执行 git checkout（fetch 后重试）失败")?;

        if !status2.success() {
            return Err(anyhow!("git checkout 失败：rev/tag/branch \"{}\" 不存在", rev));
        }
    }
    Ok(())
}

fn git_rev_parse_head(dir: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("执行 git rev-parse HEAD 失败")?;
    if !out.status.success() {
        return Err(anyhow!("git rev-parse HEAD 失败"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sanitize_for_path(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// 内部工具
// ─────────────────────────────────────────────────────────────────────────────

fn dep_spec_to_req_string(spec: &DepSpec) -> String {
    match spec {
        DepSpec::VersionReq(v) => v.clone(),
        DepSpec::Detailed { version: Some(v), .. } => v.clone(),
        DepSpec::Detailed { git: Some(url), rev, tag, branch, .. } => {
            let pin = rev.as_deref().or(tag.as_deref()).or(branch.as_deref()).unwrap_or("HEAD");
            format!("git+{}#{}", url, pin)
        }
        DepSpec::Detailed { path: Some(p), .. } => format!("path:{}", p),
        _ => "*".to_string(),
    }
}
