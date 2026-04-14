use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use neptune_core::{
    lockfile::{LockedPackage, LockedSource, Lockfile, LOCK_VERSION},
    manifest::Manifest,
    paths,
    resolver::{compute_path_content_hash, node_to_locked_package, Resolver, ResolvedSource},
};

#[derive(Parser, Debug)]
#[command(
    name = "npt",
    version,
    about = "Neptune - Nelua package manager (v0.3.1)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 初始化新的 Neptune 项目（创建 neptune.toml）
    Init {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        lib: Option<String>,
    },

    /// 安装依赖（v0.3：支持递归解析 path/git 依赖，生成完整 lockfile）
    Install {
        /// 冻结模式：lockfile 必须存在且与 manifest 一致，否则报错
        #[arg(long)]
        frozen: bool,
        /// 强制重新解析，忽略已有 lockfile
        #[arg(long)]
        force: bool,
    },

    /// 运行项目：注入 LUA_PATH 后执行指定命令
    Run {
        /// 要执行的命令（默认：nelua）
        #[arg(long, default_value = "nelua")]
        cmd: String,
        /// 传递给命令的额外参数
        #[arg(last = true)]
        args: Vec<String>,
    },

    /// 打印完整依赖树（基于 neptune.lock，包含间接依赖）
    Tree,

    /// 检查项目环境（nelua、git 等工具是否可用）
    Doctor,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Commands::Init { name, app, lib } => cmd_init(name, app, lib),
        Commands::Install { frozen, force } => cmd_install(frozen, force),
        Commands::Run { cmd, args } => cmd_run(cmd, args),
        Commands::Tree => cmd_tree(),
        Commands::Doctor => cmd_doctor(),
    }
}

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().context("获取当前目录失败")
}

// ─────────────────────────────────────────────────────────────────────────────
// npt init
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_init(name: Option<String>, app: Option<String>, lib: Option<String>) -> Result<()> {
    let root = cwd()?;
    let mf_path = root.join(paths::MANIFEST_FILE);
    if mf_path.exists() {
        return Err(anyhow!("{} 已存在，如需重新初始化请手动删除", paths::MANIFEST_FILE));
    }

    let pname = name.unwrap_or_else(|| {
        root.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("neptune-project")
            .to_string()
    });

    let entry_app = app.or_else(|| Some("src/main.nelua".to_string()));
    let entry = neptune_core::manifest::Entry {
        app: entry_app,
        lib,
    };

    let m = Manifest {
        name: pname,
        version: "0.1.0".to_string(),
        description: Some("A Neptune project".to_string()),
        license: Some("MIT".to_string()),
        authors: None,
        repository: None,
        entry,
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
    };
    m.validate()?;
    m.write_to(&mf_path)?;

    neptune_io::fs::ensure_dir(&root.join("src"))?;

    println!("✓ 已创建 {}", mf_path.display());
    println!("  下一步：编辑 neptune.toml 添加依赖，然后运行 npt install");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// npt install
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_install(frozen: bool, force: bool) -> Result<()> {
    let root = cwd()?;
    let mf_path = root.join(paths::MANIFEST_FILE);
    if !mf_path.exists() {
        return Err(anyhow!(
            "未找到 {}，请先运行 npt init",
            paths::MANIFEST_FILE
        ));
    }

    let m = Manifest::read_from(&mf_path)?;
    m.validate()?;

    let lock_path = root.join(paths::LOCK_FILE);
    let manifest_sha = neptune_core::util::sha256_of_file(&mf_path)?;

    // 对 .neptune 目录加排他锁，防止并发安装
    let _lock = neptune_io::fs::lock_dir(&paths::project_dir(&root))?;

    let lf = if !force && lock_path.exists() {
        let existing = Lockfile::read_from(&lock_path)?;
        existing.validate()?;

        if frozen {
            if !existing.is_up_to_date(&manifest_sha) {
                return Err(anyhow!(
                    "--frozen 模式下检测到 manifest 或 lockfile schema 已变化。\n\
                     请在非 frozen 模式下运行 npt install 更新 lockfile，\n\
                     然后将新的 neptune.lock 提交到版本控制。"
                ));
            }
            println!("✓ lockfile 已是最新，跳过解析（--frozen 模式）");
            existing
        } else if existing.is_up_to_date(&manifest_sha) {
            println!("✓ lockfile 已是最新，跳过重新解析");
            existing
        } else {
            println!("→ 检测到 manifest 变化，重新解析依赖...");
            resolve_and_write_lockfile(&root, &m, &manifest_sha, &lock_path)?
        }
    } else if frozen {
        return Err(anyhow!(
            "--frozen 模式下未找到 {}，请先在非 frozen 模式下运行 npt install",
            paths::LOCK_FILE
        ));
    } else {
        resolve_and_write_lockfile(&root, &m, &manifest_sha, &lock_path)?
    };

    // 物化所有包到 .neptune/pkgs
    let pkgs_root = paths::pkgs_dir(&root);
    neptune_io::fs::ensure_dir(&pkgs_root)?;

    let mut materialized = 0;
    let mut skipped = 0;
    for pkg in &lf.packages {
        if materialize_pkg(pkg, &root, &pkgs_root)? {
            materialized += 1;
        } else {
            skipped += 1;
        }
    }

    // 生成稳定的模块映射（基于 lockfile，而非字典序猜测）
    generate_modules_mapping(&root, &pkgs_root, &lf)?;
    generate_neptune_path_lua(&root)?;

    println!(
        "✓ 安装完成：{} 个依赖（新安装 {}，已缓存 {}）",
        lf.packages.len(),
        materialized,
        skipped
    );
    Ok(())
}

/// 使用 Resolver 解析依赖图，生成并写入 lockfile
fn resolve_and_write_lockfile(
    root: &Path,
    m: &Manifest,
    manifest_sha: &str,
    lock_path: &Path,
) -> Result<Lockfile> {
    let resolver = Resolver::new(root);
    let result = resolver.resolve(m)?;

    // 如果有冲突，打印详细信息并报错
    if !result.conflicts.is_empty() {
        let mut msg = String::from("发现依赖冲突，无法继续安装：\n\n");
        for conflict in &result.conflicts {
            msg.push_str(&conflict.to_string());
            msg.push('\n');
        }
        msg.push_str("请解决上述冲突后重新运行 npt install。");
        return Err(anyhow!("{}", msg));
    }

    let mut packages: Vec<LockedPackage> = Vec::new();
    for node in &result.packages {
        let content_sha = match &node.source {
            ResolvedSource::Path { abs_path } => {
                compute_path_content_hash(abs_path)
                    .with_context(|| format!("计算 {} 的内容哈希失败", node.name))?
            }
            ResolvedSource::Git { url: _, rev } => {
                // v0.3.1 修复 #1：rev 已是 resolve 阶段锁定的精确 commit hash，
                // 直接用作 content_sha256，不再写空字符串。
                format!("git-commit:{}", rev)
            }
        };

        let mut locked_pkg = node_to_locked_package(node, content_sha);
        // 更新子依赖的版本信息
        for dep in &mut locked_pkg.dependencies {
            if let Some(resolved_dep) = result.packages.iter().find(|n| n.name == dep.name) {
                dep.version = resolved_dep.version.clone();
            }
        }
        packages.push(locked_pkg);
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name));

    let lf = Lockfile {
        lock_version: LOCK_VERSION,
        manifest_sha256: manifest_sha.to_string(),
        packages,
    };
    lf.write_to(lock_path)?;
    println!("✓ 已生成 neptune.lock（包含 {} 个包，含间接依赖）", lf.packages.len());
    Ok(lf)
}

// ─────────────────────────────────────────────────────────────────────────────
// 包物化（materialize）
// ─────────────────────────────────────────────────────────────────────────────

/// 将包物化到 .neptune/pkgs/<name>/<id>
/// 返回 true 表示新安装，false 表示已缓存跳过
fn materialize_pkg(pkg: &LockedPackage, root: &Path, pkgs_root: &Path) -> Result<bool> {
    let (id, src_dir, content_sha) = match &pkg.source {
        LockedSource::Path { path } => {
            let src = PathBuf::from(path);
            let id = format!("path-{}", short_hash(&pkg.content_sha256));
            (id, src, pkg.content_sha256.clone())
        }
        LockedSource::Git { url, rev } => {
            // v0.3.1 修复 #2：rev 已是 resolve 阶段锁定的精确 commit hash，
            // 直接用它生成 pkg_id，与 generate_modules_mapping 保持一致，
            // 不再需要在 materialize 阶段重新 rev-parse。
            let cache_dir = paths::git_cache_dir(root);
            neptune_io::fs::ensure_dir(&cache_dir)?;
            let repo_dir = cache_dir.join(format!("{}-{}", sanitize(url), sanitize(rev)));

            if !repo_dir.exists() {
                println!("  → 克隆 {} ({})", url, &rev[..std::cmp::min(12, rev.len())]);
                // 此时 rev 已是完整 commit hash，直接 clone + checkout
                git_clone_and_checkout(url, rev, &repo_dir)?;
            }
            // 已缓存：无需重新 checkout（rev 是不可变的 commit hash）

            let id = format!("git-{}", &sanitize(rev)[..std::cmp::min(8, rev.len())]);
            let content_sha = format!("git-commit:{}", rev);
            (id, repo_dir, content_sha)
        }
        LockedSource::Registry { .. } => {
            return Err(anyhow!("v0.3 原型暂不支持 registry 依赖，将在 v0.4 实现"));
        }
    };

    let dst = pkgs_root.join(&pkg.name).join(&id);
    if dst.exists() {
        return Ok(false);
    }

    println!("  → 安装 {}@{}", pkg.name, pkg.version);
    neptune_io::fs::copy_dir_recursive(&src_dir, &dst)?;

    let meta = dst.join(".neptune-meta.json");
    let json = serde_json::json!({
        "name": pkg.name,
        "version": pkg.version,
        "content_sha256": content_sha,
        "npt_version": "0.3.1",
    });
    neptune_core::util::atomic_write(&meta, serde_json::to_vec_pretty(&json)?.as_slice())?;

    Ok(true)
}

// ─────────────────────────────────────────────────────────────────────────────
// 模块映射生成
// ─────────────────────────────────────────────────────────────────────────────

/// 根据 lockfile 生成稳定的 nelua_modules/ 映射
fn generate_modules_mapping(root: &Path, pkgs_root: &Path, lf: &Lockfile) -> Result<()> {
    let modules_dir = paths::modules_dir(root);
    neptune_io::fs::ensure_dir(&modules_dir)?;

    for pkg in &lf.packages {
        let pkg_id = match &pkg.source {
            LockedSource::Path { .. } => format!("path-{}", short_hash(&pkg.content_sha256)),
            LockedSource::Git { rev, .. } => {
                // v0.3.1 修复 #2：使用 lockfile 中锁定的 commit hash 生成 pkg_id，
                // 与 materialize_pkg 的逻辑完全一致，不再猜测目录。
                format!("git-{}", &sanitize(rev)[..std::cmp::min(8, rev.len())])
            }
            LockedSource::Registry { .. } => continue,
        };

        let installed_dir = pkgs_root.join(&pkg.name).join(&pkg_id);
        if !installed_dir.exists() {
            eprintln!(
                "警告：包 {} 的安装目录不存在（{}），跳过模块映射",
                pkg.name,
                installed_dir.display()
            );
            continue;
        }

        let dst = modules_dir.join(&pkg.name);
        let linked = neptune_io::fs::symlink_dir(&installed_dir, &dst)?;
        if !linked {
            neptune_io::fs::remove_if_exists(&dst)?;
            neptune_io::fs::copy_dir_recursive(&installed_dir, &dst)?;
        }
    }

    Ok(())
}

/// 生成 neptune_path.lua 引导文件
fn generate_neptune_path_lua(root: &Path) -> Result<()> {
    let path = paths::path_bootstrap_file(root);
    let content = "-- Auto-generated by Neptune (npt) v0.3. DO NOT EDIT.\n\
-- Usage: add `require('neptune_path')` at your program entry point.\n\
-- This file sets up Lua module search paths for all installed dependencies.\n\
\n\
local root = './nelua_modules'\n\
\n\
-- Nelua/Lua module search paths\n\
package.path = root..'/?.lua;'..root..'/?/init.lua;'..package.path\n\
\n\
-- Native module search (for C extensions, future-proof)\n\
package.cpath = root..'/?.so;'..root..'/?.dll;'..package.cpath\n\
\n\
return true\n";
    neptune_core::util::atomic_write(&path, content.as_bytes())?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// npt run
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_run(cmd: String, args: Vec<String>) -> Result<()> {
    let root = cwd()?;

    let boot = paths::path_bootstrap_file(&root);
    if !boot.exists() {
        return Err(anyhow!(
            "未找到 {}，请先运行 npt install",
            paths::PATH_BOOTSTRAP_FILE
        ));
    }

    let modules = paths::modules_dir(&root);
    let lua_path = format!(
        "{}/?.lua;{}/?/init.lua;;",
        modules.display(),
        modules.display()
    );
    let lua_cpath = format!("{}/?.so;{}/?.dll;;", modules.display(), modules.display());

    let status = std::process::Command::new(&cmd)
        .current_dir(&root)
        .env("LUA_PATH", &lua_path)
        .env("LUA_CPATH", &lua_cpath)
        .args(&args)
        .status()
        .with_context(|| format!("执行命令失败: {}", cmd))?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(anyhow!("命令 {} 以非零退出码退出: {}", cmd, code));
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// npt tree
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_tree() -> Result<()> {
    let root = cwd()?;
    let lock_path = root.join(paths::LOCK_FILE);
    if !lock_path.exists() {
        return Err(anyhow!("未找到 {}，请先运行 npt install", paths::LOCK_FILE));
    }
    let lf = Lockfile::read_from(&lock_path)?;

    println!("依赖树（共 {} 个包）：", lf.packages.len());
    println!();

    let all_dep_names: std::collections::HashSet<_> = lf
        .packages
        .iter()
        .flat_map(|p| p.dependencies.iter().map(|d| d.name.as_str()))
        .collect();

    let top_level: Vec<_> = lf
        .packages
        .iter()
        .filter(|p| !all_dep_names.contains(p.name.as_str()))
        .collect();

    let indirect: Vec<_> = lf
        .packages
        .iter()
        .filter(|p| all_dep_names.contains(p.name.as_str()))
        .collect();

    if !top_level.is_empty() {
        println!("直接依赖：");
        for pkg in &top_level {
            print_pkg_tree(pkg, &lf.packages, 1);
        }
    }

    if !indirect.is_empty() {
        println!();
        println!("间接依赖（{}个）：", indirect.len());
        for pkg in &indirect {
            let source_info = match &pkg.source {
                LockedSource::Path { path } => format!("path:{}", path),
                LockedSource::Git { url, rev } => format!("git+{}#{}", url, &rev[..std::cmp::min(8, rev.len())]),
                LockedSource::Registry { url, .. } => format!("registry:{}", url),
            };
            println!("  {}@{} [{}]", pkg.name, pkg.version, source_info);
        }
    }

    Ok(())
}

fn print_pkg_tree(pkg: &LockedPackage, all_pkgs: &[LockedPackage], depth: usize) {
    let indent = "  ".repeat(depth);
    let source_info = match &pkg.source {
        LockedSource::Path { path } => format!("path:{}", path),
        LockedSource::Git { url, rev } => format!("git+{}#{}", url, &rev[..std::cmp::min(8, rev.len())]),
        LockedSource::Registry { url, .. } => format!("registry:{}", url),
    };
    println!("{}{}@{} [{}]", indent, pkg.name, pkg.version, source_info);
    for dep in &pkg.dependencies {
        if let Some(dep_pkg) = all_pkgs.iter().find(|p| p.name == dep.name) {
            print_pkg_tree(dep_pkg, all_pkgs, depth + 1);
        } else {
            println!("{}  {}@{} [未安装]", indent, dep.name, dep.version);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// npt doctor
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_doctor() -> Result<()> {
    println!("Neptune 环境检查 (npt doctor)");
    println!("─────────────────────────────");

    let checks: &[(&str, &str, &[&str])] = &[
        ("nelua", "Nelua 编译器", &["--version"]),
        ("git", "Git 版本控制", &["--version"]),
        ("cc", "C 编译器（用于 Nelua 编译）", &["--version"]),
        ("pkg-config", "pkg-config（用于 C 库探测，v0.6 需要）", &["--version"]),
    ];

    let mut all_ok = true;
    for (cmd, desc, args) in checks {
        match std::process::Command::new(cmd).args(*args).output() {
            Ok(out) if out.status.success() => {
                let version = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("(版本未知)")
                    .to_string();
                println!("  ✓ {} - {}", desc, version.trim());
            }
            _ => {
                println!("  ✗ {} - 未找到（{}）", desc, cmd);
                all_ok = false;
            }
        }
    }

    println!();
    if all_ok {
        println!("✓ 所有检查通过！");
    } else {
        println!("⚠ 部分工具未找到，请根据提示安装缺失的依赖。");
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Git 辅助函数（materialize 阶段使用）
// ─────────────────────────────────────────────────────────────────────────────

/// 克隆仓库并 checkout 到指定 commit hash。
/// v0.3.1 中，materialize 阶段传入的 rev 已是精确 commit hash，
/// 不再需要 fetch 重试逻辑。
fn git_clone_and_checkout(url: &str, rev: &str, dir: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["clone", url])
        .arg(dir)
        .status()
        .context("执行 git clone 失败（请确认已安装 git 且网络可用）")?;
    if !status.success() {
        return Err(anyhow!("git clone 失败: {}", url));
    }
    // checkout 到精确 commit hash
    let status2 = std::process::Command::new("git")
        .current_dir(dir)
        .args(["checkout", "--quiet", rev])
        .status()
        .context("执行 git checkout 失败")?;
    if !status2.success() {
        return Err(anyhow!("git checkout 失败: commit \"{}\" 不存在", rev));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 工具函数
// ─────────────────────────────────────────────────────────────────────────────

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn short_hash(s: &str) -> String {
    if s.len() >= 8 {
        s[..8].to_string()
    } else {
        s.to_string()
    }
}
