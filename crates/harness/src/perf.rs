//! Performance micro-benchmarks for the hot paths.
//!
//! Plain `#[test]`s with manual timing so they run on stable Rust (no nightly,
//! no criterion). Each prints a `PERF <name> ...` line that
//! `tests/perf-report.sh` parses.
//!
//! Run: cargo test -p harness --lib perf -- --nocapture --test-threads=1

use crate::session::{excerpt, StoredMessage};
use std::time::Instant;

fn report(name: &str, iters: u32, total: std::time::Duration) {
    let per_us = total.as_secs_f64() * 1e6 / iters as f64;
    println!(
        "PERF {name}\titers={iters}\ttotal_ms={:.3}\tper_op_us={per_us:.3}",
        total.as_secs_f64() * 1000.0
    );
}

/// A message shaped like a real chat turn: several text parts of CJK+ASCII.
fn synth_message() -> StoredMessage {
    let mut parts = Vec::new();
    for i in 0..4 {
        parts.push(serde_json::json!({ "Text": format!(
            "第 {i} 段消息：这是一段用于性能测试的中文文本，长度适中，\
             包含若干可搜索的关键词，比如 原神、curl、agent、性能测试。\
             The quick brown fox jumps over the lazy dog."
        )}));
    }
    StoredMessage {
        role: "assistant".into(),
        parts: serde_json::json!({ "Assistant": parts }),
        displays: vec![],
        turn: String::new(),
        created_at: 1,
    }
}

#[test]
fn perf_persist_atomic_write() {
    // Atomic write: every appended message rewrites the whole session JSON.
    let dir = std::env::temp_dir().join("parable-perf-persist");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for &count in &[1usize, 10, 50, 200] {
        let msgs: Vec<StoredMessage> = (0..count).map(|_| synth_message()).collect();
        let payload = serde_json::to_string_pretty(&msgs).unwrap();
        let path = dir.join(format!("s_{count}.json"));

        let iters = 200u32;
        let t0 = Instant::now();
        for _ in 0..iters {
            crate::atomic_write(&path, payload.as_bytes()).unwrap();
        }
        let dt = t0.elapsed();
        report(
            &format!("persist_{count}msg_{}kb", payload.len() / 1024),
            iters,
            dt,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn perf_persist_growth_curve() {
    // Growth curve: whole-file rewrite means cost grows linearly with history.
    let dir = std::env::temp_dir().join("parable-perf-growth");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");

    let mut msgs: Vec<StoredMessage> = Vec::new();
    for count in [10usize, 50, 100, 200, 400] {
        while msgs.len() < count {
            msgs.push(synth_message());
        }
        let payload = serde_json::to_string_pretty(&msgs).unwrap();
        let iters = 100u32;
        let t0 = Instant::now();
        for _ in 0..iters {
            crate::atomic_write(&path, payload.as_bytes()).unwrap();
        }
        let dt = t0.elapsed();
        report(
            &format!("persist_growth_{count}msg_{}kb", payload.len() / 1024),
            iters,
            dt,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn perf_search_excerpt_cjk() {
    // Excerpt extraction runs once per candidate message during a search.
    let text = "这是第 42 段消息：这是一段用于性能测试的中文文本，长度适中，包含若干可搜索的关键词，比如 原神、curl、agent、性能测试。The quick brown fox jumps over the lazy dog.";
    let iters = 50_000u32;
    let t0 = Instant::now();
    let mut found = 0usize;
    for _ in 0..iters {
        if excerpt(text, "原神").is_some() {
            found += 1;
        }
    }
    let dt = t0.elapsed();
    assert_eq!(found, iters as usize);
    report("search_excerpt_cjk", iters, dt);
}

#[test]
fn perf_token_estimate() {
    // Token estimation runs on every turn.
    let text = "你好，世界！这是一段用于性能测试的中文与 English mixed text. \
                안녕하세요, 你好, hello world, 1234567890 — mixed scripts for estimation.";
    let iters = 200_000u32;
    let t0 = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        acc += crate::token::estimate(text);
    }
    let dt = t0.elapsed();
    assert!(acc > 0);
    report("token_estimate", iters, dt);
}

#[test]
fn perf_serialize_session() {
    // Serialization cost paid on every persist.
    let msgs: Vec<StoredMessage> = (0..50).map(|_| synth_message()).collect();
    let iters = 2_000u32;
    let t0 = Instant::now();
    for _ in 0..iters {
        let s = serde_json::to_string_pretty(&msgs).unwrap();
        std::hint::black_box(s.len());
    }
    let dt = t0.elapsed();
    report("serialize_50msg", iters, dt);
}

#[test]
fn perf_neutral_parse() {
    // Rebuilding provider messages replays the whole history each turn.
    let msg = synth_message();
    let iters = 20_000u32;
    let t0 = Instant::now();
    for _ in 0..iters {
        let m: crate::provider::Msg = serde_json::from_value(msg.parts.clone()).unwrap();
        std::hint::black_box(m);
    }
    let dt = t0.elapsed();
    report("neutral_parse", iters, dt);
}

#[test]
fn perf_tool_specs() {
    // Building the tool schemas happens once per turn.
    let reg = crate::tools::Registry::new(std::path::PathBuf::from("."));
    let iters = 2_000u32;
    // cold-ish: first call populates the cache
    let t0 = Instant::now();
    for _ in 0..iters {
        let s = reg.specs();
        std::hint::black_box(s.len());
    }
    let dt = t0.elapsed();
    report("tool_specs_cached", iters, dt);
}

#[test]
fn perf_search_full_scan() {    // End-to-end search cost over a realistic corpus: 40 sessions x 50 messages.
    let corpus: Vec<(String, Vec<StoredMessage>)> = (0..40)
        .map(|s| {
            let msgs: Vec<StoredMessage> = (0..50)
                .map(|i| {
                    let mut m = synth_message();
                    m.role = if i % 2 == 0 { "user".into() } else { "assistant".into() };
                    m.parts = serde_json::json!({
                        "User": [{ "Text": format!("会话 {s} 的第 {i} 条消息：包含 原神 与 agent 等关键词，\
                                                     以及一段用于性能测试的普通文本内容。" ) }]
                    });
                    m
                })
                .collect();
            (format!("s_{s}"), msgs)
        })
        .collect();

    let iters = 50u32;
    let t0 = Instant::now();
    let mut hits = 0usize;
    for _ in 0..iters {
        for (_id, msgs) in &corpus {
            for m in msgs {
                let arr = m.parts.get("User").and_then(|v| v.as_array()).unwrap();
                for p in arr {
                    let text = p.get("Text").and_then(|x| x.as_str()).unwrap_or("");
                    if excerpt(text, "原神").is_some() {
                        hits += 1;
                    }
                }
            }
        }
    }
    let dt = t0.elapsed();
    assert!(hits > 0);
    report("search_scan_40x50msg", iters, dt);
}

#[test]
fn perf_append_message_cost() {
    // 每追加一条消息都要整份重写会话 —— 成本随历史线性增长（O(n²) IO）。
    // 这里量一下"一轮对话追加多条消息"的累计代价。
    let dir = std::env::temp_dir().join("parable-perf-append");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for &n_msg in &[20usize, 100] {
        let mut msgs: Vec<StoredMessage> = Vec::new();
        let iters = 20u32;
        let t0 = Instant::now();
        for _ in 0..iters {
            msgs.clear();
            for i in 0..n_msg {
                msgs.push(synth_message());
                // 模拟 append_message 的落盘代价：序列化整份会话 + 原子写。
                let payload = serde_json::to_string_pretty(&msgs).unwrap();
                crate::atomic_write(&dir.join("s.json"), payload.as_bytes()).unwrap();
                std::hint::black_box(i);
            }
        }
        let dt = t0.elapsed();
        report(
            &format!("append_{n_msg}msg_total"),
            iters * n_msg as u32,
            dt,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn perf_append_breakdown() {
    // 拆解 append 的成本：克隆整个会话 vs 序列化 vs 落盘。
    let msgs: Vec<StoredMessage> = (0..100).map(|_| synth_message()).collect();
    let iters = 500u32;

    let t0 = Instant::now();
    for _ in 0..iters {
        let c = msgs.clone();
        std::hint::black_box(c.len());
    }
    report("clone_100msg", iters, t0.elapsed());

    let t0 = Instant::now();
    for _ in 0..iters {
        let s = serde_json::to_string_pretty(&msgs).unwrap();
        std::hint::black_box(s.len());
    }
    report("serialize_100msg", iters, t0.elapsed());
}

#[test]
fn perf_serialize_compact_vs_pretty() {
    // to_string_pretty vs to_string：缩进占多少。
    let msgs: Vec<StoredMessage> = (0..100).map(|_| synth_message()).collect();
    let iters = 500u32;

    let t0 = Instant::now();
    for _ in 0..iters {
        let s = serde_json::to_string_pretty(&msgs).unwrap();
        std::hint::black_box(s.len());
    }
    report("serialize_pretty_100msg", iters, t0.elapsed());

    let t0 = Instant::now();
    for _ in 0..iters {
        let s = serde_json::to_string(&msgs).unwrap();
        std::hint::black_box(s.len());
    }
    report("serialize_compact_100msg", iters, t0.elapsed());
}

#[test]
fn perf_serialize_typed_vs_value() {
    // 存储用的是 Value（JSON 树），序列化要递归遍历；强类型 enum 直接写出。
    // 这是 append 成本 92% 的来源。
    use crate::provider::Msg;

    let n = 100usize;
    let value_msgs: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            serde_json::json!({
                "Assistant": [
                    { "Text": format!("第 {i} 段消息：这是一段用于性能测试的中文文本，长度适中，包含若干可搜索的关键词。") },
                    { "Thinking": { "text": "hmm", "signature": "sig" } },
                    { "ToolUse": { "id": "c1", "name": "read_file", "args": { "path": "x" } } },
                ]
            })
        })
        .collect();
    let typed: Vec<Msg> = (0..n)
        .map(|i| {
            Msg::Assistant(vec![crate::provider::AsstPart::Text(format!(
                "第 {i} 段消息：这是一段用于性能测试的中文文本，长度适中，包含若干可搜索的关键词。"
            ))])
        })
        .collect();

    let iters = 500u32;
    let t0 = Instant::now();
    for _ in 0..iters {
        let s = serde_json::to_string(&value_msgs).unwrap();
        std::hint::black_box(s.len());
    }
    report("serialize_value_100msg", iters, t0.elapsed());

    let t0 = Instant::now();
    for _ in 0..iters {
        let s = serde_json::to_string(&typed).unwrap();
        std::hint::black_box(s.len());
    }
    report("serialize_typed_100msg", iters, t0.elapsed());

    // Value -> Msg 的转换成本（现在每次落盘/读盘都要走一遍）
    let t0 = Instant::now();
    for _ in 0..iters {
        for v in &value_msgs {
            let m: Msg = serde_json::from_value(v.clone()).unwrap();
            std::hint::black_box(m);
        }
    }
    report("value_to_typed_100msg", iters, t0.elapsed());
}
