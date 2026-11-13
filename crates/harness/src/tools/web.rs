//! `web_fetch`: pull a URL and reduce it to readable text.
//!
//! Deliberately dependency-free: scripts, styles, and markup are stripped with a
//! small hand-rolled parser rather than a full HTML engine.

use super::*;
use futures::StreamExt;
use serde_json::Value;
use std::net::IpAddr;

/// 响应体硬上限:超过就停止下载,防止巨型响应拖垮内存。
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

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

    let client = crate::config::apply_proxy(reqwest::Client::builder())
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(45))
        .user_agent("Parable-Harness/0.1")
        // 关闭 reqwest 自动重定向:每一跳连网前都做 SSRF 复检。
        // 事后只查最终 URL 拦不住——内网连接在中间跳已经发生。
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let mut current = url.clone();
    let mut resp = None;
    for _ in 0..6 {
        assert_remote_public(&current).await?;
        let r = client.get(&current).send().await?;
        let status = r.status();
        if status.is_redirection() {
            let loc = r
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            current = resolve_redirect(&current, &loc).ok_or_else(|| {
                HarnessError::Tool(format!("unsupported redirect target `{loc}` from {current}"))
            })?;
            continue;
        }
        resp = Some(r);
        break;
    }
    let resp = resp.ok_or_else(|| HarnessError::Tool(format!("too many redirects fetching {url}")))?;
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
    // 流式累积到上限即停,不给巨型响应整块进内存的机会
    let (body, clipped) = read_body_capped(resp, MAX_BODY_BYTES).await?;

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

    let text = if clipped {
        format!("{text}\n\n[response body truncated at the 10 MB download limit]")
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

/// 流式读取响应体,超过 cap 停止下载并返回截断标记。
async fn read_body_capped(
    resp: reqwest::Response,
    cap: usize,
) -> Result<(String, bool), HarnessError> {
    let mut out: Vec<u8> = Vec::new();
    let mut clipped = false;
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let chunk = item?;
        if out.len() + chunk.len() > cap {
            let remain = cap.saturating_sub(out.len());
            out.extend_from_slice(&chunk[..remain.min(chunk.len())]);
            clipped = true;
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Ok((String::from_utf8_lossy(&out).into_owned(), clipped))
}

/// SSRF 防护:解析 URL 主机并检查每个解析结果的 IP,环回/私网/链路本地/
/// 未指定/唯一本地等一律拒绝;主机无法解析同样拒绝。DNS 放阻塞线程执行,
/// 避免卡住异步运行时。
async fn assert_remote_public(url: &str) -> Result<(), HarnessError> {
    let host = extract_host(url)
        .ok_or_else(|| HarnessError::Tool(format!("cannot parse host from {url}")))?;
    let addrs = tokio::task::spawn_blocking(move || {
        use std::net::ToSocketAddrs;
        (host.as_str(), 0u16)
            .to_socket_addrs()
            .map(|it| it.map(|a| a.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|e| HarnessError::Tool(format!("host lookup failed: {e}")))?
    .map_err(|e| HarnessError::Tool(format!("cannot resolve host of {url}: {e}")))?;
    if addrs.is_empty() {
        return Err(HarnessError::Tool(format!(
            "host of {url} resolved to no addresses"
        )));
    }
    for ip in addrs {
        if !is_public_ip(ip) {
            return Err(HarnessError::Tool(format!(
                "refusing to fetch {url}: {ip} is a loopback/private/link-local address"
            )));
        }
    }
    Ok(())
}

/// 从 URL 里取主机名:去 scheme/userinfo/端口;IPv6 字面量取方括号内的部分。
fn extract_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority_end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if let Some(open) = authority.strip_prefix('[') {
        let close = open.find(']')?;
        return Some(open[..close].to_string());
    }
    let host = authority.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// 目标 IP 校验:环回/私网/链路本地/未指定/唯一本地/CGNAT/组播保留段拒绝。
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(
                v4.is_loopback() // 127.0.0.0/8
                || v4.is_private() // 10.0.0.0/8、172.16.0.0/12、192.168.0.0/16
                || v4.is_link_local() // 169.254.0.0/16
                || v4.is_unspecified() // 0.0.0.0
                || o[0] == 0 // 0.0.0.0/8 "本网络"段
                || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGNAT
                || v4.is_multicast() // 224.0.0.0/4
                || v4.is_broadcast() // 255.255.255.255
                || o[0] >= 240 // 240.0.0.0/4 保留段
            )
        }
        IpAddr::V6(v6) => {
            // IPv4 映射地址按内嵌 IPv4 校验,防 `::ffff:10.0.0.1` 绕过
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(
                v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 唯一本地
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 链路本地
                || v6.is_multicast()
            )
        }
    }
}

/// 解析重定向 Location 为绝对 URL。处理绝对地址与根相对路径两种常见形态;
/// 其它形态罕见,直接拒绝以免拼出意外目标。
fn resolve_redirect(base: &str, loc: &str) -> Option<String> {
    let loc = loc.trim();
    if loc.is_empty() {
        return None;
    }
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return Some(loc.to_string());
    }
    if let Some(path) = loc.strip_prefix('/') {
        let (scheme, rest) = base.split_once("://")?;
        let authority = rest
            .split(|c| c == '/' || c == '?' || c == '#')
            .next()?;
        return Some(format!("{scheme}://{authority}/{path}"));
    }
    None
}

/// Strip tags, drop script/style/noscript/svg, collapse whitespace.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let mut in_skip = false;
    let mut skip_tag = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    // 属性引号状态:引号里的 '>' 是内容不是标签结束(如 title="a>b")
    let mut quote: Option<char> = None;
    let mut chars = html.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
                quote = None;
            }
            '>' => {
                if in_tag && quote.is_none() {
                    in_tag = false;
                    // 自闭合写法(<br/>、<div />)容忍尾部的 '/'
                    let t = tag.trim().trim_end_matches('/').to_ascii_lowercase();
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
                } else if in_tag {
                    tag.push(c); // 引号内的 '>' 属于属性内容
                } else if !in_skip {
                    out.push(c);
                }
            }
            _ => {
                if in_tag {
                    if let Some(q) = quote {
                        if c == q {
                            quote = None;
                        }
                    } else if c == '"' || c == '\'' {
                        quote = Some(c);
                    }
                    tag.push(c);
                } else if !in_skip {
                    out.push(c);
                }
            }
        }
    }

    // Decode the entities that matter for readability (named + numeric forms).
    let out = decode_entities(&out);

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

/// 解码 HTML 实体:常见命名实体 + 数字形式(&#NN; 十进制 / &#xHH; 十六进制)
/// 与 &apos;。未识别的实体原样保留。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        // 收一个实体名(到 ';' 为止,最长 10 个字符);收不到就原样吐回
        let mut name = String::new();
        let mut found = false;
        while let Some(&n) = chars.peek() {
            if n == ';' {
                chars.next();
                found = true;
                break;
            }
            if !n.is_ascii_alphanumeric() && n != '#' && n != 'x' && n != 'X' {
                break;
            }
            if name.len() >= 10 {
                break;
            }
            name.push(n);
            chars.next();
        }
        if !found {
            out.push('&');
            out.push_str(&name);
            continue;
        }
        let decoded = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(dec) = name.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match name.as_str() {
                "nbsp" => Some(' '),
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "mdash" => Some('—'),
                "hellip" => Some('…'),
                _ => None,
            }
        };
        match decoded {
            Some(ch) => out.push(ch),
            None => {
                out.push('&');
                out.push_str(&name);
                out.push(';');
            }
        }
    }
    out
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

    // 搜索来源只保留真实的引擎抓取。
    // 原 AnySearch 聚合源实测返回的是裁判文书库（与网络搜索无关，任何关键词都返回判决书），
    // 已整体移除；历史配置里的 "anysearch" 与 "multi" 现在等价，都走下面这组引擎。
    // (label, search url)
    let engines: Vec<(&str, String)> = vec![
        ("必应 (Bing)", format!("https://www.bing.com/search?q={}", urlencode(&query))),
        ("百度 (Baidu)", format!("https://www.baidu.com/s?wd={}", urlencode(&query))),
        ("360搜索", format!("https://www.so.com/s?q={}", urlencode(&query))),
        ("DuckDuckGo", format!("https://html.duckduckgo.com/html/?q={}", urlencode(&query))),
        ("搜狗 (Sogou)", format!("https://www.sogou.com/web?query={}", urlencode(&query))),
    ];

    let client = crate::config::apply_proxy(reqwest::Client::builder())
        .connect_timeout(std::time::Duration::from_secs(12))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let total_platforms = engines.len();
    let mut out = format!(
        "=== 多平台搜索结果 === 关键词: {query} 搜索平台: {} 个\n",
        total_platforms
    );
    let mut ok_count = 0usize;

    for (label, url) in &engines {
        out.push_str(&format!("\n【{label}】\nURL: {url}\n"));
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    // 非 2xx 直接标注状态码,不再当正文往里解析
                    out.push_str(&format!("(HTTP {status})\n"));
                    continue;
                }
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
            }
            Err(e) => out.push_str(&format!("(请求失败: {e})\n")),
        }
    }

    out.push_str(&format!(
        "\n=== 结束 === 成功获取 {} / {} 个平台\n",
        ok_count,
        total_platforms
    ));

    Ok(ToolOutcome {
        ok: ok_count > 0,
        title: format!("Searched “{query}” · {ok_count}/{total_platforms} platforms"),
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

    let client = crate::config::apply_proxy(reqwest::Client::builder())
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
    fn ssrf_rejects_internal_targets() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        let ok = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
        let bad = [
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 31, 255, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 9, 9)),
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            // 唯一本地 fc00::/7 与 IPv4 映射的内网地址
            "fc00::1".parse::<IpAddr>().unwrap(),
            "::ffff:10.0.0.1".parse::<IpAddr>().unwrap(),
        ];
        assert!(is_public_ip(ok));
        for b in bad {
            assert!(!is_public_ip(b), "must reject {b}");
        }
    }

    #[test]
    fn extract_host_covers_forms() {
        assert_eq!(extract_host("https://example.com/a?b=c"), Some("example.com".into()));
        assert_eq!(extract_host("http://example.com:8080/x"), Some("example.com".into()));
        assert_eq!(extract_host("https://user:pw@example.com/"), Some("example.com".into()));
        assert_eq!(extract_host("http://[::1]:9000/x"), Some("::1".into()));
        assert_eq!(extract_host("ftp://example.com/"), None);
        assert_eq!(extract_host("https:///nohost"), None);
    }

    #[test]
    fn redirect_targets_resolve() {
        assert_eq!(
            resolve_redirect("https://a.com/x?y=1", "https://b.com/z"),
            Some("https://b.com/z".into())
        );
        assert_eq!(
            resolve_redirect("https://a.com/x?y=1", "/next/page"),
            Some("https://a.com/next/page".into())
        );
        assert_eq!(resolve_redirect("https://a.com/", "relative"), None);
        assert_eq!(resolve_redirect("https://a.com/", ""), None);
    }

    #[test]
    fn html_self_closing_and_quoted_gt() {
        // 自闭合块级标签 + 属性里的 '>' 不能截断标签
        let html = r#"<div/><p>line1<br/>line2</p><a title="a>b">link</a><span>x</span>"#;
        let text = html_to_text(html);
        assert!(text.contains("line1"), "got {text:?}");
        assert!(text.contains("line2"), "got {text:?}");
        assert!(text.contains("link"), "got {text:?}");
        assert!(text.contains("x"), "got {text:?}");
        assert!(!text.contains("b\">"), "quoted '>' must not eat markup, got {text:?}");
    }

    #[test]
    fn html_numeric_and_apos_entities() {
        let text = html_to_text("<p>&#39;x&#x27;y &apos;z&apos; &copy; a&amp;b</p>");
        assert!(text.contains("'x'y 'z'"), "got {text:?}");
        assert!(text.contains("&copy;"), "unknown entity stays literal");
        assert!(text.contains("a&b"), "got {text:?}");
    }
}
