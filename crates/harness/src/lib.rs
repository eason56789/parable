//! Parable harness core — agent loop, providers, tools, permissions, sessions, local server.

pub mod agent;
pub mod agents;
pub mod computer;
pub mod checkpoint;
pub mod desktop;
pub mod docs;
pub mod hooks;
pub mod mcp;
pub mod config;
pub mod cookies;
pub mod error;
pub mod event;
pub mod git;
pub mod memory;
pub mod models;
pub mod permission;
pub mod plugin;
pub mod provider;
pub mod schedule;
pub mod server;
pub mod session;
pub mod skills;
pub mod subagent;
pub mod team;
pub mod term;
pub mod tools;
pub mod token;
pub mod usage;
pub mod wsfs;

/// Performance micro-benchmarks. Only compiled under `cfg(test)`.
#[cfg(test)]
mod perf;

/// Test-only isolated `PARABLE_HOME` helper. Only compiled under `cfg(test)`.
#[cfg(test)]
pub(crate) mod testutil;

pub use error::HarnessError;
pub type Result<T> = std::result::Result<T, HarnessError>;

/// 原子写盘：先写唯一命名的 `{文件名}.tmp-<rand>`，fsync 后 rename 覆盖目标。
///
/// session / schedule / config / checkpoint 共用的落盘路径。
/// - tmp 名带随机后缀：固定 `.tmp` 名在两个并发写方交错时会互相覆盖
///   （A 写 tmp → B 写 tmp → A rename 成功 → B rename 失败），唯一名让
///   交错退化为"其中一个失败重试"，不会破坏对方的数据。
/// - rename 前 fsync：掉电时 NTFS 可能先持久化元数据后持久化数据，
///   目标会变成 0 字节残缺文件；先 sync_all 再 rename 缩小该窗口。
/// - Windows 上 `std::fs::rename` 对已存在目标可能报错：退化为
///   "目标先改名成 .bak → tmp 改名上位"，失败则把 .bak 还原回去——
///   旧实现是"先删目标再改名"，rename 一失败原文件就没了。
/// 纯同步函数，无外部依赖。
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let tmp = std::path::PathBuf::from(tmp_name);
    std::fs::write(&tmp, bytes)?;
    // 数据先落稳再换名：rename 只保证名字切换，不保证内容已刷盘。
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    let bak = {
        let mut b = path.as_os_str().to_os_string();
        b.push(".bak");
        std::path::PathBuf::from(b)
    };
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows：目标已存在时 rename 可能失败——目标先挪去 .bak，
            // 失败可还原；成功后再清理。
            match std::fs::rename(path, &bak).and_then(|_| std::fs::rename(&tmp, path)) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&bak);
                    Ok(())
                }
                Err(e) => {
                    // tmp 上位失败：把 .bak 还原回目标位，尽量不留残局。
                    let _ = std::fs::rename(&bak, path);
                    let _ = std::fs::remove_file(&tmp);
                    Err(e)
                }
            }
        }
    }
}

/// 子进程环境白名单（SEC-01）：`env_clear` 后只回填运行必需的系统变量，
/// 不把父进程持有的 provider key / token / secret / auth 带进子进程。
/// 配置里显式声明的 env 由调用方在此之后单独注入。
pub const CHILD_ENV_WHITELIST: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "TMPDIR",
    "SystemRoot",
    "ComSpec",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "LOCALAPPDATA",
    "APPDATA",
    "NODE_PATH",
    "PATHEXT",
    "NUMBER_OF_PROCESSORS",
];
