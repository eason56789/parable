//! `web_fetch`: pull a URL and reduce it to readable text.
//!
//! Deliberately dependency-free: scripts, styles, and markup are stripped with a
//! small hand-rolled parser rather than a full HTML engine.

use super::*;
use serde_json::Value;

pub async fn fetch(a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
    let url = a["url"].as_str().unwrap_or("").to_string();
    if url.is_empty() {
        return Err(HarnessError::Tool("`url` is required".into()));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(HarnessError::Tool(format!(
            "only http/https URLs are supported, got {url}"
        )));
    }
    let max_bytes = a["max_bytes"].as_u64().unwrap_or(20_000) as usize;

    ctx.authorize("web_fetch", &url, Risk::Network).await?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(45))
        .user_agent("Parable-Harness/0.1")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(HarnessError::Tool(format!("HTTP {status} fetching {url}")));
    }
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await?;

    let text = if content_type.contains("json") {
        serde_json::from_str::<Value>(&body)
            .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.clone()))
            .unwrap_or(body.clone())
    } else if content_type.contains("html") || body.trim_start().starts_with('<') {
        html_to_text(&body)
    } else {
        body.clone()
    };

    let text = if text.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…\n[truncated]", &text[..end])
    } else {
        text
    };

    Ok(ToolOutcome {
        ok: true,
        title: format!("Fetched {url} · {} chars", text.len()),
        display: DisplayKind::Plain(text.clone()),
        for_model: format!("Fetched {url} ({} bytes of text):\n\n{}", body.len(), text),
    })
}

/// Strip tags, drop script/style/noscript/svg, collapse whitespace.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let mut in_skip = false;
    let mut skip_tag = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    let mut chars = html.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' => {
                in_tag = false;
                let t = tag.trim().to_ascii_lowercase();
                if in_skip {
                    if t.starts_with(&format!("/{skip_tag}")) {
                        in_skip = false;
                    }
                } else if t.starts_with("script")
                    || t.starts_with("style")
                    || t.starts_with("noscript")
                    || t.starts_with("svg")
                    || t.starts_with("head")
                {
                    in_skip = true;
                    skip_tag = t.split_whitespace().next().unwrap_or("").trim_start_matches('/').to_string();
                } else if matches!(
                    t.split_whitespace().next().unwrap_or(""),
                    "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                        | "section" | "article" | "header" | "footer" | "blockquote"
                ) {
                    out.push('\n');
                }
                tag.clear();
            }
            _ => {
                if in_tag {
                    tag.push(c);
                } else if !in_skip {
                    out.push(c);
                }
            }
        }
    }

    // Decode the handful of entities that matter for readability.
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&mdash;", "—")
        .replace("&hellip;", "…");

    // Collapse runs of blank lines and trim trailing spaces per line.
    let mut result = String::new();
    let mut blank = 0usize;
    for line in out.lines() {
        let t = line.trim_end();
        if t.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        result.push_str(t);
        result.push('\n');
    }
    result.trim().to_string()
}

/// Multi-engine web search.
///
/// Queries several public search endpoints, converts each results page to text,
/// and returns them concatenated with a per-platform header — the same shape as
/// a hand-run multi-platform search. Engines that block the request (Baidu's
/// anti-bot interstitial, for instance) are reported honestly as blocked rather
/// than dropped or faked.
pub async fn multi_search(a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
    let query = a["query"].as_str().unwrap_or("").trim().to_string();
    if query.is_empty() {
        return Err(HarnessError::Tool("`query` is required".into()));
    }
    let per_engine = a["max_bytes"].as_u64().unwrap_or(3500) as usize;

    ctx.authorize("web_search", &query, Risk::Network).await?;

    // 用户在设置里挑的搜索来源裁剪查询集：
    // anysearch → 只查 AnySearch；multi → 跳过 AnySearch 只跑引擎；其余 → 两者都查。
    let pref = ctx.search_pref.as_str();
    let anysearch_first = pref != "multi";
    let engines_on = pref != "anysearch";

    // (label, search url)
    let engines: Vec<(&str, String)> = if engines_on {
        vec![
            ("必应 (Bing)", format!("https://www.bing.com/search?q={}", urlencode(&query))),
            ("搜狗 (Sogou)", format!("https://www.sogou.com/web?query={}", urlencode(&query))),
            ("360搜索", format!("https://www.so.com/s?q={}", urlencode(&query))),
            ("DuckDuckGo", format!("https://html.duckduckgo.com/html/?q={}", urlencode(&query))),
            ("百度 (Baidu)", format!("https://www.baidu.com/s?wd={}", urlencode(&query))),
        ]
    } else {
        Vec::new()
    };

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(12))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let mut out = format!(
        "=== {} === 关键词: {query} 搜索平台: {} 个\n",
        if engines_on { "多平台搜索结果" } else { "AnySearch 搜索结果" },
        engines.len() + usize::from(anysearch_first)
    );
    let mut ok_count = 0usize;

    // AnySearch 聚合源先行：POST JSON API，免 key（匿名按 IP 限流）。
    // 失败不影响后面的平台抓取，按原样如实报告。
    if anysearch_first {
        out.push_str("\n【AnySearch】\n");
        match anysearch_search(&client, &query).await {
            Ok(text) if !text.trim().is_empty() => {
                ok_count += 1;
                out.push_str(&text);
                out.push('\n');
            }
            Ok(_) => out.push_str("(无结果)\n"),
            Err(e) => out.push_str(&format!("(请求失败: {e})\n")),
        }
    }

    for (label, url) in &engines {
        out.push_str(&format!("\n【{label}】\nURL: {url}\n"));
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let ctype = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                match resp.text().await {
                    Ok(body) => {
                        let text = if ctype.contains("html") || body.trim_start().starts_with('<') {
                            html_to_text(&body)
                        } else {
                            body
                        };
                        let blocked = is_blocked(&text);
                        let clipped = clip(&text, per_engine);
                        if blocked {
                            out.push_str("(该平台返回了反爬验证页，未取得结果)\n");
                        } else if clipped.trim().is_empty() {
                            out.push_str("(无结果)\n");
                        } else {
                            ok_count += 1;
                            out.push_str(&clipped);
                            out.push('\n');
                        }
                    }
                    Err(e) => out.push_str(&format!("(读取失败: {e})\n")),
                }
                let _ = status;
            }
            Err(e) => out.push_str(&format!("(请求失败: {e})\n")),
        }
    }

    out.push_str(&format!(
        "\n=== 结束 === 成功获取 {} / {} 个平台\n",
        ok_count,
        engines.len() + 1
    ));

    Ok(ToolOutcome {
        ok: ok_count > 0,
        title: format!("Searched “{query}” · {ok_count}/{} platforms", engines.len() + 1),
        display: DisplayKind::Plain(out.clone()),
        for_model: out,
    })
}

/// DeepSeek Search API (`POST /v1/tools/search`) — DeepSeek's in-house search
/// engine, returning ranked results with relevance scores and model-ready
/// summaries. Needs an API key set in Settings (`cfg.search_key`); without
/// one the tool refuses instead of silently degrading.
pub async fn deepseek_search(a: &Value, ctx: &ToolCtx) -> Result<ToolOutcome, HarnessError> {
    if ctx.search_key.trim().is_empty() {
        return Err(HarnessError::Tool(
            "deepseek_search 未配置：请先在设置中填入 DeepSeek 搜索 API key".into(),
        ));
    }
    let query = a["query"].as_str().unwrap_or("").trim().to_string();
    if query.is_empty() || query.chars().count() > 128 {
        return Err(HarnessError::Tool("`query` 必填，1-128 字符".into()));
    }
    let count = a["count"].as_u64().unwrap_or(8).clamp(1, 50);
    let need_content = a["need_content"].as_bool().unwrap_or(false);
    let ori_question = a["ori_question"].as_str().unwrap_or("").trim().to_string();

    ctx.authorize("deepseek_search", &query, Risk::Network).await?;

    let mut body = serde_json::json!({
        "query": query,
        "count": count,
        "need_content": need_content,
    });
    if !ori_question.is_empty() {
        body["ori_question"] = serde_json::json!(ori_question);
    }

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(12))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = client
        .post("https://api.deepseek.com/v1/tools/search")
        .bearer_auth(ctx.search_key.trim())
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(|e| {
        HarnessError::Tool(format!("deepseek_search 响应不是 JSON (HTTP {status}): {e}"))
    })?;
    if !status.is_success() {
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        return Err(HarnessError::Tool(format!(
            "deepseek_search HTTP {status}{}",
            if msg.is_empty() { String::new() } else { format!(": {msg}") }
        )));
    }

    let results = v.get("results").and_then(|r| r.as_array());
    let n = results.map(|r| r.len()).unwrap_or(0);
    let mut out = format!("=== DeepSeek 搜索 === 关键词: {query} · {n} 条结果\n");
    if let Some(results) = results {
        for (i, r) in results.iter().enumerate() {
            let title = r.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let url = r.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let score = r.get("rerank_score").and_then(|s| s.as_f64()).unwrap_or(0.0);
            let auth = r.get("authority").and_then(|x| x.as_i64()).unwrap_or(-1);
            let site = r.get("site_name").and_then(|s| s.as_str()).unwrap_or("");
            let date = r.get("date").and_then(|d| d.as_str()).unwrap_or("");
            let summary = r.get("summary").and_then(|s| s.as_str()).unwrap_or("");
            let content = r.get("content").and_then(|c| c.as_str()).unwrap_or("");
            out.push_str(&format!(
                "\n{}. {}{}\nURL: {}\n相关性: {:.1} 权威度: {}\n",
                i + 1,
                title,
                if site.is_empty() { String::new() } else { format!(" — {site}") },
                url,
                score,
                auth
            ));
            if !date.is_empty() {
                out.push_str(&format!("发布时间: {date}\n"));
            }
            // need_content 打开时优先给正文，否则给面向模型的长摘要
            let body_text = if !content.trim().is_empty() { content } else { summary };
            if !body_text.trim().is_empty() {
                out.push_str(&clip(body_text, if need_content { 2500 } else { 1200 }));
                out.push('\n');
            }
        }
    }
    if n == 0 {
        out.push_str("(无结果)\n");
    }
    Ok(ToolOutcome {
        ok: n > 0,
        title: format!("Searched “{query}” · DeepSeek · {n} 条"),
        display: DisplayKind::Plain(out.clone()),
        for_model: out,
    })
}

/// AnySearch aggregated search — POST JSON API, works without a key
/// (anonymous calls are IP-rate-limited). Set `ANYSEARCH_API_KEY` to attach
/// a bearer token; the API does NOT fall back to anonymous on a bad key,
/// so the header is only sent when a non-empty key exists.
async fn anysearch_search(client: &reqwest::Client, query: &str) -> Result<String, HarnessError> {
    let body = serde_json::json!({
        "query": query,
        "max_results": 8,
        "format": "json"
    });
    let mut req = client
        .post("https://api.anysearch.com/v1/search")
        .json(&body);
    if let Ok(key) = std::env::var("ANYSEARCH_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            req = req.bearer_auth(key);
        }
    }
    let resp = req.send().await?;
    let status = resp.status();
    let v: Value = resp
        .json()
        .await
        .map_err(|e| HarnessError::Tool(format!("anysearch 响应不是 JSON (HTTP {status}): {e}")))?;
    parse_anysearch(&v)
}

/// Render an AnySearch response envelope into readable text.
fn parse_anysearch(v: &Value) -> Result<String, HarnessError> {
    if v.get("code").and_then(|c| c.as_i64()) != Some(0) {
        let msg = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(HarnessError::Tool(format!("anysearch: {msg}")));
    }
    let mut out = String::new();
    if let Some(results) = v.pointer("/data/results").and_then(|r| r.as_array()) {
        for (i, r) in results.iter().enumerate() {
            let title = r.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let url = r.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let snippet = r.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
            out.push_str(&format!("{}. {}\nURL: {}\n", i + 1, title, url));
            if !snippet.is_empty() {
                out.push_str(snippet);
                out.push('\n');
            }
            if let Some(content) = r.get("content").and_then(|c| c.as_str()) {
                let content = content.trim();
                if !content.is_empty() {
                    out.push_str(&clip(content, 800));
                    out.push('\n');
                }
            }
            out.push('\n');
        }
    }
    Ok(out)
}

/// Anti-bot interstitials that carry no results.
fn is_blocked(text: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "百度安全验证",
        "网络不给力，请稍后重试",
        "please verify you are a human",
        "unusual traffic",
        "captcha",
        "检测到有异常请求",
    ];
    let lower = text.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(&m.to_lowercase())) && text.len() < 1200
}

fn clip(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.len() <= max {
        return t.to_string();
    }
    let mut end = max;
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（本平台已截断）", &t[..end])
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_handles_chinese() {
        assert_eq!(urlencode("三角洲行动"), "%E4%B8%89%E8%A7%92%E6%B4%B2%E8%A1%8C%E5%8A%A8");
        assert_eq!(urlencode("a b-c"), "a%20b-c");
    }

    #[test]
    fn detects_baidu_antibot() {
        assert!(is_blocked("百度安全验证\n网络不给力，请稍后重试\n返回首页"));
        assert!(!is_blocked(&"三角洲行动 - 搜索\n约 109,000 条结果\n".repeat(20)));
    }

    #[test]
    fn clip_is_boundary_safe() {
        let s = "中文测试内容";
        let out = clip(s, 3);
        // 3 bytes falls mid-character for CJK, so clip must back off to a
        // boundary rather than panic on a bad slice.
        assert!(out.starts_with('中'), "got {out}");
        assert!(out.contains("已截断"), "got {out}");
        assert_eq!(clip("abc", 10), "abc", "under the limit is untouched");
    }

    #[test]
    fn strips_script_and_keeps_text() {
        let html = r#"<html><head><style>.a{}</style></head>
            <body><h1>Title</h1><script>alert(1)</script><p>Hello &amp; world</p></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & world"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("a{}"));
    }

    #[test]
    fn parses_anysearch_envelope() {
        let v = serde_json::json!({
            "code": 0, "message": "success",
            "data": {"results": [
                {"title": "Go 1.26 Release Notes", "url": "https://go.dev/doc/go1.26",
                 "snippet": "Intro.", "content": "Long content body"},
                {"title": "No content result", "url": "https://example.com/x"}
            ]}
        });
        let out = parse_anysearch(&v).unwrap();
        assert!(out.contains("1. Go 1.26 Release Notes"));
        assert!(out.contains("URL: https://go.dev/doc/go1.26"));
        assert!(out.contains("Long content body"));
        assert!(out.contains("2. No content result"));
        let err = parse_anysearch(&serde_json::json!({"code": 429, "message": "rate limited"})).unwrap_err();
        assert!(err.to_string().contains("rate limited"));
    }
}
