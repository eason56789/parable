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

pub use error::HarnessError;
pub type Result<T> = std::result::Result<T, HarnessError>;

/// 原子写盘：先写 `{文件名}.tmp`，再 rename 覆盖目标。
///
/// session / schedule / config 共用的落盘路径。Windows 上 `std::fs::rename`
/// 对已存在目标可能报错，这里退化为"先删后改名"再试一次；仍失败则清理
/// 临时文件并返回错误。纯同步函数，无外部依赖。
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_name);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows：目标已存在时 rename 可能失败——先删再改名重试。
            match std::fs::remove_file(path).and_then(|_| std::fs::rename(&tmp, path)) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    Err(e)
                }
            }
        }
    }
}
