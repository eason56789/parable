//! 测试专用：进程级隔离的 `PARABLE_HOME`。
//!
//! `PARABLE_HOME` 是进程级环境变量：并发测试各自 `set_var` 会互相串目录，
//! 未注入隔离根的用例还会把会话写进用户真实的 `~/.parable`。这里集中一把
//! 进程级互斥锁——每个用例在串行段内拿到独立临时目录并把变量指过去，
//! `Drop` 时恢复原值、删除临时目录。其他模块不要再各写一套 env_lock。

#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(test)]
fn env_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// 一个用例独占的临时数据根：构造后 `PARABLE_HOME` 指向它，离开作用域即还原。
#[cfg(test)]
pub struct TestHome {
    root: PathBuf,
    prev: Option<std::ffi::OsString>,
    _guard: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl TestHome {
    /// 占用进程级 env 锁，新建独立临时目录并把 `PARABLE_HOME` 指过去。
    /// 必须等到锁被拿到才返回，避免并发测试串目录。
    pub fn new(tag: &str) -> Self {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "parable-test-home-{tag}-{}-{}",
            std::process::id(),
            crate::event::now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let prev = std::env::var_os("PARABLE_HOME");
        std::env::set_var("PARABLE_HOME", &root);
        Self {
            root,
            prev,
            _guard: guard,
        }
    }

    /// 隔离根目录（`PARABLE_HOME` 指向的位置）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 隔离根下的会话目录（`~/.parable/sessions` 的对应位置）。
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }
}

#[cfg(test)]
impl Drop for TestHome {
    fn drop(&mut self) {
        // 先还原进程级变量，再删临时目录；避免别的用例把 home 解析到已删目录。
        match self.prev.take() {
            Some(v) => std::env::set_var("PARABLE_HOME", v),
            None => std::env::remove_var("PARABLE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
