//! 文件检查点：每次写文件前把旧内容快照进 ~/.parable/checkpoints/<session>/，
//! 支持「回退该轮的文件改动」（Claude Code /rewind、OpenCode snapshots 同型）。
//!
//! 语义：同一 turn 内同一路径只保留【第一次】记录（即本轮改动前的状态），
//! 回退 = 把该 turn 记录的所有文件恢复为快照内容；本轮新建的文件删除。
//!
//! 存储布局（根目录由构造传入，生产用 `Config::home_dir().join("checkpoints")`）：
//!
//! ```text
//! <root>/<session_id>/<turn_id>/
//!     manifest.json        // {"turn":..,"ts":..,"files":[{"path":..,"existed":true,"snap":"<hash>.bin"}]}
//!     snaps/<hash>.bin     // 旧内容原样字节（文件名 = 内容 SHA-256 hex，同内容天然去重）
//!     pre-restore/<i>_<ts>.bin   // 回退前对当前内容的再备份，防误回退二次丢失
//! ```

use crate::error::HarnessError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize)]
pub struct CheckpointInfo {
    pub turn: String,
    pub ts: u64,
    pub files: Vec<String>,
}

/// 单文件相对本轮改动前快照的当前状态（前端改动摘要条的行）。
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    /// 绝对路径。
    pub path: String,
    /// created / modified / deleted / unchanged / gone（新建后又被删）。
    pub status: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckpointDetail {
    pub turn: String,
    pub ts: u64,
    pub files: Vec<FileChange>,
}

/// 检查点存储。
///
/// `record` / `restore` / `prune` 之间用一把互斥锁串行化：manifest 是
/// 读-改-写结构，两个并发 record 可能互相覆盖对方的条目。`list` 只读，
/// 走原子写保证的一致快照，不持锁。
pub struct CheckpointStore {
    root: PathBuf,
    io: Mutex<()>,
}

/// manifest.json 的磁盘格式。`snap` 只在 `existed == true` 时非空。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Manifest {
    turn: String,
    ts: u64,
    files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    /// 绝对路径字符串。
    path: String,
    /// false = 本轮新建（此前不存在），回退时应删除。
    existed: bool,
    /// 快照文件名 "<sha256-hex>.bin"；existed=false 时为空串。
    #[serde(default)]
    snap: String,
}

/// session/turn 用作目录名：拒绝会跑出根目录或为空的 id。
fn safe_id(id: &str) -> Result<(), HarnessError> {
    if id.is_empty() || id == "." || id == ".." || id.contains('/') || id.contains('\\') {
        return Err(HarnessError::Tool(format!(
            "checkpoint: invalid id {id:?}"
        )));
    }
    Ok(())
}

/// 路径记录为绝对路径字符串：相对输入按当前工作目录补全。
/// 不做 canonicalize——Windows 上会带出 `\\?\` 前缀，和调用方手里的路径对不上。
fn absolute_string(path: &Path) -> String {
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path).to_string_lossy().into_owned(),
            // 拿不到 cwd 就按原样记录，dedup 仍按字符串相等工作。
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 内容 SHA-256 hex -> 快照文件名。
fn snap_name(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{}.bin", hex::encode(h.finalize()))
}

impl CheckpointStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            io: Mutex::new(()),
        }
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.root.join(session)
    }

    fn turn_dir(&self, session: &str, turn: &str) -> PathBuf {
        self.root.join(session).join(turn)
    }

    fn manifest_path(dir: &Path) -> PathBuf {
        dir.join("manifest.json")
    }

    /// 读一个 turn 的 manifest；缺失或损坏返回 None（list 跳过、restore 报错）。
    fn read_manifest(dir: &Path) -> Option<Manifest> {
        let text = std::fs::read_to_string(Self::manifest_path(dir)).ok()?;
        match serde_json::from_str::<Manifest>(&text) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!("checkpoint: corrupt manifest in {}: {e}", dir.display());
                None
            }
        }
    }

    /// 原子更新 manifest（临时文件 + rename，照 session.rs 的落盘方式）。
    fn write_manifest(dir: &Path, m: &Manifest) -> Result<(), HarnessError> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(m)
            .map_err(|e| HarnessError::Tool(format!("checkpoint: serialize manifest: {e}")))?;
        crate::atomic_write(&Self::manifest_path(dir), json.as_bytes())?;
        Ok(())
    }

    /// 写操作前记录：`old` 为 None 表示文件此前不存在（本轮新建）。
    /// 同 turn 同路径去重（保留最早状态）。
    pub async fn record(
        &self,
        session: &str,
        turn: &str,
        path: &std::path::Path,
        old: Option<&[u8]>,
    ) -> Result<(), HarnessError> {
        safe_id(session)?;
        safe_id(turn)?;
        let dir = self.turn_dir(session, turn);
        let path_str = absolute_string(path);

        let _guard = self.io.lock().await;

        let mut m = Self::read_manifest(&dir).unwrap_or_default();
        let fresh = m.files.is_empty() && m.turn.is_empty() && m.ts == 0;
        if fresh {
            m.turn = turn.to_string();
            // 首条记录的时间：list 的倒序、prune 的「最近 keep 个」都以它为准。
            m.ts = now_ms();
        }

        // 同 turn 同路径去重：保留第一次记录（本轮最早状态），内容相同与否都不再追加。
        if m.files.iter().any(|f| f.path == path_str) {
            return Ok(());
        }

        let entry = match old {
            Some(bytes) => {
                let name = snap_name(bytes);
                let snaps = dir.join("snaps");
                std::fs::create_dir_all(&snaps)?;
                let snap_path = snaps.join(&name);
                // 内容寻址命名：同内容文件已存在时跳过写盘（SHA-256 相同即同字节）。
                if !snap_path.is_file() {
                    crate::atomic_write(&snap_path, bytes)?;
                }
                FileEntry {
                    path: path_str,
                    existed: true,
                    snap: name,
                }
            }
            None => FileEntry {
                path: path_str,
                existed: false,
                snap: String::new(),
            },
        };
        m.files.push(entry);
        Self::write_manifest(&dir, &m)
    }

    /// 某会话的全部检查点（按时间倒序）。manifest 缺失/损坏的目录跳过。
    pub async fn list(&self, session: &str) -> Vec<CheckpointInfo> {
        let mut out = Vec::new();
        let session_dir = self.session_dir(session);
        if let Ok(rd) = std::fs::read_dir(&session_dir) {
            for e in rd.flatten() {
                let dir = e.path();
                if !dir.is_dir() {
                    continue;
                }
                let Some(m) = Self::read_manifest(&dir) else {
                    continue;
                };
                out.push(CheckpointInfo {
                    turn: m.turn,
                    ts: m.ts,
                    files: m.files.into_iter().map(|f| f.path).collect(),
                });
            }
        }
        out.sort_by(|a, b| b.ts.cmp(&a.ts));
        out
    }

    /// 带逐文件改动详情的检查点列表（按时间倒序）。
    ///
    /// 每个文件对比「快照内容（本轮改动前）」与「当前盘上内容」，给出状态与
    /// 行级增删统计——前端改动摘要条（N 个文件已更改 +A -B）直接消费这份
    /// 数据。已回退过的 turn：盘上内容等于快照，状态收敛为 unchanged，前端
    /// 据此隐藏摘要条，撤销按钮无需额外状态查询。
    pub async fn list_detail(&self, session: &str) -> Vec<CheckpointDetail> {
        let mut out = Vec::new();
        let session_dir = self.session_dir(session);
        let Some(rd) = std::fs::read_dir(&session_dir).ok() else {
            return out;
        };
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(m) = Self::read_manifest(&dir) else {
                continue;
            };
            let files = m
                .files
                .iter()
                .map(|f| Self::change_stat(&dir, f))
                .collect();
            out.push(CheckpointDetail {
                turn: m.turn,
                ts: m.ts,
                files,
            });
        }
        out.sort_by(|a, b| b.ts.cmp(&a.ts));
        out
    }

    /// 单文件：快照 vs 当前盘上内容 → 状态 + 增删行数。
    fn change_stat(turn_dir: &Path, f: &FileEntry) -> FileChange {
        let snap = || -> Option<String> {
            let bytes = std::fs::read(turn_dir.join("snaps").join(&f.snap)).ok()?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        };
        let cur = std::fs::read(&f.path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .ok();
        let (status, added, removed) = match (f.existed, cur) {
            (true, Some(new)) => {
                let old = snap().unwrap_or_default();
                let (a, r, _) = crate::tools::fs::diff_lines(&old, &new);
                if a + r == 0 {
                    ("unchanged", a, r)
                } else {
                    ("modified", a, r)
                }
            }
            (true, None) => {
                // 本轮改过、现在没了：视为整文件删除
                let old = snap().unwrap_or_default();
                let r = old.lines().count().max(if old.is_empty() { 0 } else { 1 });
                ("deleted", 0, r)
            }
            (false, Some(new)) => {
                let (a, _, _) = crate::tools::fs::diff_lines("", &new);
                ("created", a, 0)
            }
            (false, None) => ("gone", 0, 0),
        };
        FileChange {
            path: f.path.clone(),
            status: status.into(),
            added,
            removed,
        }
    }

    /// 单文件的审查 diff：快照 vs 当前盘上内容，行级（已折叠长段未变更区）。
    /// 返回 (状态, 增, 删, 行)。turn/路径没有对应检查点时返回 None。
    pub async fn diff_for(
        &self,
        session: &str,
        turn: &str,
        path: &str,
    ) -> Option<(String, usize, usize, Vec<crate::tools::DiffLine>)> {
        safe_id(session).ok()?;
        safe_id(turn).ok()?;
        let dir = self.turn_dir(session, turn);
        let m = Self::read_manifest(&dir)?;
        let f = m.files.iter().find(|f| f.path == path)?;
        let stat = Self::change_stat(&dir, f);
        let cur = std::fs::read(&f.path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .ok();
        let old = || -> String {
            std::fs::read(dir.join("snaps").join(&f.snap))
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        let lines = match (&stat.status[..], cur) {
            ("modified", Some(new)) => crate::tools::fs::diff_lines(&old(), &new).2,
            ("created", Some(new)) => crate::tools::fs::diff_lines("", &new).2,
            ("deleted", _) => crate::tools::fs::diff_lines(&old(), "").2,
            _ => Vec::new(),
        };
        Some((stat.status, stat.added, stat.removed, lines))
    }

    /// 回退某 turn 的文件改动；返回恢复/删除的文件绝对路径列表（新建文件被删除）。
    ///
    /// 恢复前先把每个目标文件的当前内容备份进 `<turn>/pre-restore/<i>_<ts>.bin`：
    /// 误回退（或对同一个 turn 回退两次）时当前内容还能从这里找回。备份名带
    /// 时间戳，重复回退不会覆盖上一次的备份。
    pub async fn restore(
        &self,
        session: &str,
        turn: &str,
    ) -> Result<Vec<String>, HarnessError> {
        safe_id(session)?;
        safe_id(turn)?;
        let dir = self.turn_dir(session, turn);

        let _guard = self.io.lock().await;

        let m = Self::read_manifest(&dir).ok_or_else(|| {
            HarnessError::Tool(format!(
                "checkpoint: no manifest for turn {turn} in session {session}"
            ))
        })?;

        // 先备份当前内容，再动手恢复：任何一步失败都不至于丢用户数据。
        let ts = now_ms();
        let pre = dir.join("pre-restore");
        let mut backups: Vec<PathBuf> = Vec::new();
        for (i, f) in m.files.iter().enumerate() {
            let cur = Path::new(&f.path);
            if cur.is_file() {
                std::fs::create_dir_all(&pre)?;
                let bytes = std::fs::read(cur)?;
                let bak = pre.join(format!("{i}_{ts}.bin"));
                crate::atomic_write(&bak, &bytes)?;
                backups.push(bak);
            }
        }

        let mut touched = Vec::new();
        for f in &m.files {
            let cur = Path::new(&f.path);
            if f.existed {
                let bytes = std::fs::read(dir.join("snaps").join(&f.snap)).map_err(|e| {
                    HarnessError::Tool(format!(
                        "checkpoint: snapshot {} missing or unreadable: {e}",
                        f.snap
                    ))
                })?;
                // 文件可能被整目录删除过：先把父目录补回来再原子写。
                if let Some(parent) = cur.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::atomic_write(cur, &bytes)?;
                touched.push(f.path.clone());
            } else if cur.exists() {
                // 新建文件回退 = 删除。如果现在变成目录（极端情况），跳过并记
                // 警告，不让单条异常打断整轮恢复。
                match std::fs::remove_file(cur) {
                    Ok(()) => touched.push(f.path.clone()),
                    Err(e) => {
                        tracing::warn!(
                            "checkpoint: cannot remove {} during restore: {e}",
                            cur.display()
                        );
                    }
                }
            }
        }
        // 备份成功即留在盘上，由 prune 随 turn 目录一起清理。
        drop(backups);
        Ok(touched)
    }

    /// 只保留最近 `keep` 个 turn 的检查点，其余（含损坏目录）删除。
    /// `keep == 0` 清空该会话。
    pub async fn prune(&self, session: &str, keep: usize) -> Result<(), HarnessError> {
        safe_id(session)?;
        let session_dir = self.session_dir(session);
        if !session_dir.is_dir() {
            return Ok(());
        }

        let _guard = self.io.lock().await;

        let mut turns: Vec<(u64, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&session_dir) {
            for e in rd.flatten() {
                let dir = e.path();
                if !dir.is_dir() {
                    continue;
                }
                let ts = Self::read_manifest(&dir).map(|m| m.ts).unwrap_or(u64::MAX);
                turns.push((ts, dir));
            }
        }
        // 按 ts 新→旧排；损坏目录（u64::MAX）排最后，优先被清。
        turns.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, dir) in turns.into_iter().skip(keep) {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = td.path().join(tag);
        (td, root)
    }

    /// now_ms 精度是毫秒，两个 turn 撞在同一毫秒会让 list 的排序不稳。
    fn tick() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    #[tokio::test]
    async fn record_list_restore_round_trip() {
        let (_td, root) = tmp_root("cp");
        let store = CheckpointStore::new(root.clone());
        let work = _td.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let f = work.join("a.txt");

        std::fs::write(&f, b"v1").unwrap();
        store
            .record("s1", "t1", &f, Some(b"v1"))
            .await
            .expect("record t1");
        tick();
        std::fs::write(&f, b"v2").unwrap();
        store
            .record("s1", "t2", &f, Some(b"v2"))
            .await
            .expect("record t2");

        let infos = store.list("s1").await;
        assert_eq!(infos.len(), 2, "two turns recorded");
        assert_eq!(infos[0].turn, "t2", "newest first");
        assert_eq!(infos[1].turn, "t1");
        assert_eq!(infos[0].files, vec![f.to_string_lossy().to_string()]);

        // 回退 t2 = 撤销该轮改动 -> 内容回到 t2 之前的状态 v2。
        let restored = store.restore("s1", "t2").await.expect("restore t2");
        assert_eq!(restored, vec![f.to_string_lossy().to_string()]);
        assert_eq!(std::fs::read(&f).unwrap(), b"v2");

        // 再回退 t1 -> 回到最初 v1；t1 的 pre-restore 里应留有本次回退前的 v2。
        store.restore("s1", "t1").await.expect("restore t1");
        assert_eq!(std::fs::read(&f).unwrap(), b"v1");
        let pre = root.join("s1").join("t1").join("pre-restore");
        assert!(pre.is_dir() && std::fs::read_dir(&pre).unwrap().count() >= 1);
    }

    #[tokio::test]
    async fn same_turn_same_path_keeps_first_record() {
        let (_td, root) = tmp_root("cp");
        let store = CheckpointStore::new(root.clone());
        let f = _td.path().join("dedup.txt");
        std::fs::write(&f, b"original").unwrap();

        store
            .record("s", "t", &f, Some(b"original"))
            .await
            .expect("first record");
        std::fs::write(&f, b"changed-once").unwrap();
        store
            .record("s", "t", &f, Some(b"changed-once"))
            .await
            .expect("second record is a no-op");
        std::fs::write(&f, b"changed-twice").unwrap();
        store
            .record("s", "t", &f, Some(b"changed-twice"))
            .await
            .expect("third record is a no-op");

        // manifest 只有一条，快照只有最早那份内容。
        let m: Manifest = serde_json::from_str(
            &std::fs::read_to_string(root.join("s").join("t").join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].path, f.to_string_lossy().to_string());
        let snaps = std::fs::read_dir(root.join("s").join("t").join("snaps")).unwrap();
        assert_eq!(snaps.count(), 1);

        store.restore("s", "t").await.expect("restore");
        assert_eq!(std::fs::read(&f).unwrap(), b"original", "回到本轮最早状态");
    }

    #[tokio::test]
    async fn new_file_rollback_deletes_it() {
        let (_td, root) = tmp_root("cp");
        let store = CheckpointStore::new(root.clone());
        let f = _td.path().join("created-this-turn.txt");

        // 本轮新建：记录时文件还不存在。
        store.record("s", "t", &f, None).await.expect("record new");
        std::fs::write(&f, b"born this turn").unwrap();

        let restored = store.restore("s", "t").await.expect("restore");
        assert_eq!(restored, vec![f.to_string_lossy().to_string()]);
        assert!(!f.exists(), "新建文件应被删除");

        // 幂等：文件已经不在了，再回退一次不报错、列表为空。
        let again = store.restore("s", "t").await.expect("restore again");
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn prune_keeps_most_recent_turns() {
        let (_td, root) = tmp_root("cp");
        let store = CheckpointStore::new(root.clone());
        let f = _td.path().join("p.txt");
        std::fs::write(&f, b"x").unwrap();

        for t in ["t1", "t2", "t3"] {
            store.record("s", t, &f, Some(b"x")).await.unwrap();
            tick();
        }
        assert_eq!(store.list("s").await.len(), 3);

        store.prune("s", 1).await.expect("prune");
        let left = store.list("s").await;
        assert_eq!(left.len(), 1, "only the newest turn survives");
        assert_eq!(left[0].turn, "t3");
        assert!(!root.join("s").join("t1").exists());
        assert!(!root.join("s").join("t2").exists());
        assert!(root.join("s").join("t3").exists());

        // keep=0 清空；不存在的会话是 no-op。
        store.prune("s", 0).await.expect("prune all");
        assert!(store.list("s").await.is_empty());
        store.prune("ghost", 4).await.expect("no-op for unknown session");
    }

    #[tokio::test]
    async fn corrupt_manifest_dirs_are_skipped_by_list_and_pruned_first() {
        let (_td, root) = tmp_root("cp");
        let store = CheckpointStore::new(root.clone());
        let f = _td.path().join("c.txt");
        std::fs::write(&f, b"x").unwrap();

        store.record("s", "good", &f, Some(b"x")).await.unwrap();
        let bad = root.join("s").join("broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("manifest.json"), b"{ not json").unwrap();

        let infos = store.list("s").await;
        assert_eq!(infos.len(), 1, "corrupt dir must be skipped");
        assert_eq!(infos[0].turn, "good");

        // 损坏目录排在最后，keep=0 时一并清掉。
        store.prune("s", 0).await.unwrap();
        assert!(!bad.exists());
        assert!(store.list("s").await.is_empty());
    }

    #[tokio::test]
    async fn identical_content_is_not_written_twice() {
        let (_td, root) = tmp_root("cp");
        let store = CheckpointStore::new(root.clone());
        let f = _td.path().join("same.txt");
        std::fs::write(&f, b"stable").unwrap();

        store.record("s", "t1", &f, Some(b"stable")).await.unwrap();
        tick();
        // 另一个 turn 又记到同样的内容：快照文件复用（内容寻址天然去重）。
        store.record("s", "t2", &f, Some(b"stable")).await.unwrap();

        let snaps = root.join("s").join("t1").join("snaps");
        assert_eq!(std::fs::read_dir(&snaps).unwrap().count(), 1);
        // 两个 manifest 指向同一个快照文件。
        let n1 = std::fs::read_to_string(root.join("s").join("t1").join("manifest.json")).unwrap();
        let n2 = std::fs::read_to_string(root.join("s").join("t2").join("manifest.json")).unwrap();
        let m1: Manifest = serde_json::from_str(&n1).unwrap();
        let m2: Manifest = serde_json::from_str(&n2).unwrap();
        assert_eq!(m1.files[0].snap, m2.files[0].snap);
    }

    #[tokio::test]
    async fn restore_survives_deleted_parent_dirs_and_reports_missing_turn() {
        let (_td, root) = tmp_root("cp");
        let store = CheckpointStore::new(root.clone());
        let sub = _td.path().join("deep").join("nest");
        std::fs::create_dir_all(&sub).unwrap();
        let f = sub.join("f.txt");
        std::fs::write(&f, b"keep me").unwrap();
        store.record("s", "t", &f, Some(b"keep me")).await.unwrap();

        // 模拟回滚目标：整个目录树被删掉。
        std::fs::remove_dir_all(_td.path().join("deep")).unwrap();

        let restored = store.restore("s", "t").await.expect("restore rebuilds dirs");
        assert_eq!(restored, vec![f.to_string_lossy().to_string()]);
        assert_eq!(std::fs::read(&f).unwrap(), b"keep me");

        // 没有记录过的 turn：明确报错而不是空列表。
        let err = store.restore("s", "nope").await.expect_err("must error");
        assert!(err.to_string().contains("nope"));
    }
}
