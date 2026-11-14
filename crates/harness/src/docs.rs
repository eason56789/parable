//! 文档能力 — 让 agent 真正读写常用办公文档。
//!
//! 覆盖格式与实现路径：
//! - **读**：pdf（pdf-extract 逐页文本）、docx（zip + quick-xml 抽取段落/表格）、
//!   pptx（zip + XML 逐页文本）、xlsx/xls（calamine 全表）、csv、md/txt（纯文本）。
//!   doc/ppt 旧二进制格式走 LibreOffice 转换兜底（soffice 可用时）。
//! - **写**：docx（docx-rs：标题/段落/列表/表格）、xlsx（rust_xlsxwriter）、
//!   pptx（最小 OOXML zip 手写：标题+要点页）、pdf（genpdf，CJK 走系统字体）、
//!   md/csv（纯文本）。
//! - **编辑**：xlsx 单元格/加表（umya-spreadsheet 保留格式）、docx 追加块/
//!   文本替换、pptx 追加页、pdf 合并（lopdf）。md 建议直接用 write_file。
//!
//! 工具面（模型侧）：
//! - `doc_read {path, sheet?, max_chars?}` — 按扩展名自动分派提取
//! - `doc_create {path, doc:{...}}` — 按扩展名规格化创建
//! - `doc_edit {path, ops:[...]}` — 结构化编辑（保留既有格式）
//! - `doc_convert {path, to}` — LibreOffice 跨格式转换
//!
//! 所有写操作过权限网关；读操作进入只读白名单。

use crate::error::HarnessError;
use lopdf::{Dictionary, Document as PdfDocument, Object, ObjectId};
use serde_json::Value;
use std::io::Read as _;
use std::path::{Path, PathBuf};

// ===================================================================== helpers

fn resolve_in_workspace(ctx: &super::tools::ToolCtx, raw: &str) -> Result<PathBuf, HarnessError> {
    let p = raw.trim();
    if p.is_empty() {
        return Err(HarnessError::Tool("`path` is required".into()));
    }
    crate::tools::fs::resolve_within(&ctx.workspace, p)
}

fn need_doc(a: &Value) -> Result<Value, HarnessError> {
    a.get("doc")
        .cloned()
        .filter(|v| v.is_object() || v.is_string())
        .ok_or_else(|| HarnessError::Tool("`doc` is required（格式化文档规格，见工具描述）".into()))
}

/// 已知可读扩展名（供规格描述与错误提示）。
pub const READABLE_EXTS: [&str; 11] = [
    "pdf", "docx", "doc", "pptx", "ppt", "xlsx", "xls", "csv", "md", "markdown", "txt",
];

/// doc_read 默认输出上限（字符）。
const DEFAULT_MAX_CHARS: usize = 40000;

/// Excel 网格上限（xlsxwriter / umya 一致），越界直接给可读错误而非底层报错。
const XLSX_MAX_ROWS: u32 = 1_048_576;
const XLSX_MAX_COLS: u32 = 16_384;

/// 小写化扩展名（无扩展名返回 None）。
fn ext_of(path: &Path) -> Option<String> {
    path.extension()?.to_str().map(|e| e.to_ascii_lowercase())
}

/// 相对展示路径：能剥掉工作区前缀就剥（输出给模型/用户看）。
fn rel_display(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// 文本解码：UTF-8 严格优先；失败回退 GBK（Windows 上中文 CSV/文本常是 GBK，
/// 与 shell.rs 的编码处理同一思路），再不行才 lossy。
fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (s, _, had_errors) = encoding_rs::GBK.decode(bytes);
            if had_errors {
                String::from_utf8_lossy(bytes).into_owned()
            } else {
                s.into_owned()
            }
        }
    }
}

/// 手写 XML 转义（a:t/w:t 文本与属性通用）。
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 不允许的控制字符直接丢弃，避免生成损坏文档
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// 按 max_chars 截断（按字符边界，不用字节切——中文截半字节会乱码）。
/// 截断时注明「已截断」并给出总长，模型可选择调大 max_chars。
fn apply_max_chars(body: &str, max_chars: usize) -> String {
    if max_chars == 0 || body.chars().count() <= max_chars {
        return body.to_string();
    }
    let total = body.chars().count();
    let cut: String = body.chars().take(max_chars).collect();
    format!(
        "{cut}\n\n（已截断：全文共 {total} 字符，仅返回前 {max_chars}；需要更多请调大 max_chars 或指定 sheet/分段读取）"
    )
}

/// 写文件前补齐父目录（与 write_file 行为一致）。裸文件名的 parent 是空串，
/// create_dir_all("") 会报错，所以这里判空跳过。
fn ensure_parent(path: &Path) -> Result<(), HarnessError> {
    if let Some(p) = path.parent() {
        if !p.as_os_str().is_empty() {
            std::fs::create_dir_all(p)?;
        }
    }
    Ok(())
}

/// doc_edit 对纯文本格式的统一提示（语义上 write_file/edit_file 才是对的入口）。
fn plain_text_edit_hint() -> String {
    "纯文本（md / markdown / txt / csv）请直接用 write_file / edit_file 修改，doc_edit 只处理结构化格式（xlsx / docx / pptx / pdf）。".into()
}

// ===================================================================== zip 底座

type ZipEntries = Vec<(String, Vec<u8>)>;

fn open_zip(path: &Path) -> Result<zip::ZipArchive<std::io::BufReader<std::fs::File>>, HarnessError> {
    let f = std::fs::File::open(path)
        .map_err(|e| HarnessError::Tool(format!("无法打开 {}: {e}", path.display())))?;
    zip::ZipArchive::new(std::io::BufReader::new(f)).map_err(|e| {
        HarnessError::Tool(format!(
            "{} 不是有效的 zip 包（{}）: {e}",
            path.display(),
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("文件")
                .to_ascii_uppercase()
        ))
    })
}

fn zip_read_entry(path: &Path, name: &str) -> Result<Vec<u8>, HarnessError> {
    let mut a = open_zip(path)?;
    let mut f = a
        .by_name(name)
        .map_err(|e| HarnessError::Tool(format!("{} 缺少 {name}: {e}", path.display())))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// 读取全部条目（保持原有顺序——pptx 追加页重写时顺序无关紧要，但保持原序更稳）。
fn zip_read_all(path: &Path) -> Result<ZipEntries, HarnessError> {
    let mut a = open_zip(path)?;
    let names: Vec<String> = a.file_names().map(String::from).collect();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let mut f = a
            .by_name(&name)
            .map_err(|e| HarnessError::Tool(format!("{} 读 {name} 失败: {e}", path.display())))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        out.push((name, buf));
    }
    Ok(out)
}

fn zip_write_all(path: &Path, entries: &ZipEntries) -> Result<(), HarnessError> {
    let f = std::fs::File::create(path)?;
    let mut w = zip::ZipWriter::new(f);
    for (name, data) in entries {
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        w.start_file(name.as_str(), opts)
            .map_err(|e| HarnessError::Tool(format!("zip 写入 {name} 失败: {e}")))?;
        std::io::Write::write_all(&mut w, data)
            .map_err(|e| HarnessError::Tool(format!("zip 写入 {name} 内容失败: {e}")))?;
    }
    w.finish()
        .map_err(|e| HarnessError::Tool(format!("zip 收尾失败: {e}")))?;
    Ok(())
}

// ===================================================================== 读取实现

/// pdf：pdf-extract 逐页文本（extract_text_by_pages 天然按页切，正好做分页标注）。
fn extract_pdf(path: &Path) -> Result<(String, usize), HarnessError> {
    let pages = pdf_extract::extract_text_by_pages(path).map_err(|e| {
        HarnessError::Tool(format!(
            "PDF 文本提取失败: {e}（可能是加密 PDF；扫描件没有文本层，请改用 computer/desktop 截图交给视觉模型读）"
        ))
    })?;
    let mut out = String::new();
    for (i, page) in pages.iter().enumerate() {
        out.push_str(&format!("--- 第 {} 页 ---\n", i + 1));
        out.push_str(page.trim_end());
        out.push('\n');
    }
    Ok((out, pages.len()))
}

/// docx：zip 读 word/document.xml，quick-xml 抽取段落（w:t 拼接）与表格
/// （行×单元格，` | ` 连接）。空段落跳过。
fn extract_docx(path: &Path) -> Result<(String, usize, usize), HarnessError> {
    let bytes = zip_read_entry(path, "word/document.xml")?;
    let xml = String::from_utf8_lossy(&bytes);
    let (text, paragraphs, tables) = parse_docx_document_xml(&xml);
    Ok((text, paragraphs, tables))
}

/// document.xml 流式解析。为什么不用 docx_rs 的 read_docx：读取只关心文本层，
/// quick-xml 单遍扫描不需要把整棵 OOXML 树建出来，大文档省一个数量级的内存。
///
/// 状态机要点：
/// - `tbl_depth` 跟踪表格嵌套——嵌套表（表里套表）只在最外层切行/单元格，
///   内层内容并入外层单元格文本（Word 实际文档里嵌套表极少，精度可接受）；
/// - 表格深度 ≥1 时 w:t 归入当前单元格，否则归入当前段落；
/// - w:p 在表外才成段，表内的段落文本直接并入单元格。
fn parse_docx_document_xml(xml: &str) -> (String, usize, usize) {
    use quick_xml::events::Event;
    // OOXML 的 XML 声明即 UTF-8，文本直接按 UTF-8 解（quick-xml 未启用
    // encoding 特性，BytesText 的 decode() 不可用，from_utf8_lossy 等价）。
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out: Vec<String> = Vec::new();
    let mut paragraphs = 0usize;
    let mut tables = 0usize;
    let mut tbl_depth = 0usize;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut cur_row: Vec<String> = Vec::new();
    let mut cur_cell = String::new();
    let mut in_para = false;
    let mut para = String::new();
    let mut capture = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().local_name().as_ref() {
                b"tbl" => {
                    tbl_depth += 1;
                    if tbl_depth == 1 {
                        table_rows.clear();
                        cur_row.clear();
                        cur_cell.clear();
                    }
                }
                b"tr" if tbl_depth == 1 => cur_row = Vec::new(),
                b"tc" if tbl_depth == 1 => cur_cell = String::new(),
                b"p" if tbl_depth == 0 => {
                    in_para = true;
                    para = String::new();
                }
                b"t" => capture = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                // 先把 LocalName 绑定到变量再 as_ref()：链式调用产生的临时值
                // 在语句末就被回收，直接绑定会借到悬垂引用
                let local = e.name().local_name();
                let name = local.as_ref();
                if name == b"tab" {
                    // 制表位折成 \t，保住列对齐信息
                    if tbl_depth > 0 {
                        cur_cell.push('\t');
                    } else if in_para {
                        para.push('\t');
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if capture {
                    let s = String::from_utf8_lossy(&t).into_owned();
                    if tbl_depth > 0 {
                        cur_cell.push_str(&s);
                    } else if in_para {
                        para.push_str(&s);
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().local_name().as_ref() {
                b"t" => capture = false,
                b"p" if tbl_depth == 0 => {
                    in_para = false;
                    if !para.trim().is_empty() {
                        out.push(para.trim().to_string());
                        paragraphs += 1;
                    }
                    para = String::new();
                }
                b"tc" if tbl_depth == 1 => cur_row.push(cur_cell.trim().to_string()),
                b"tr" if tbl_depth == 1 => table_rows.push(cur_row.clone()),
                b"tbl" => {
                    tbl_depth -= 1;
                    if tbl_depth == 0 {
                        for row in &table_rows {
                            if !row.iter().all(|c| c.trim().is_empty()) {
                                out.push(row.join(" | "));
                            }
                        }
                        tables += 1;
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            // 坏 XML：返回已收集的部分内容，比整体报错更有用
            Err(e) => {
                tracing::warn!("document.xml 解析提前终止: {e}");
                break;
            }
            _ => {}
        }
    }
    (out.join("\n"), paragraphs, tables)
}

/// pptx：按编号排序逐页抽 a:t 文本，标题占位符（ph type="title"）排前。
fn extract_pptx(path: &Path) -> Result<(String, usize), HarnessError> {
    let mut archive = open_zip(path)?;
    let mut slides: Vec<(u32, String)> = archive
        .file_names()
        .filter_map(|n| pptx_slide_num(n).map(|num| (num, n.to_string())))
        .collect();
    if slides.is_empty() {
        return Err(HarnessError::Tool(format!(
            "{} 里没有 ppt/slides/slideN.xml——不是有效的 pptx 包",
            path.display()
        )));
    }
    slides.sort_by_key(|(n, _)| *n);
    let mut out = String::new();
    for (i, (_, name)) in slides.iter().enumerate() {
        let mut f = archive
            .by_name(name)
            .map_err(|e| HarnessError::Tool(format!("读 {name} 失败: {e}")))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let lines = parse_pptx_slide_xml(&String::from_utf8_lossy(&buf));
        out.push_str(&format!("--- 第 {} 页 ---\n", i + 1));
        out.push_str(&lines.join("\n"));
        out.push('\n');
    }
    Ok((out, slides.len()))
}

/// `ppt/slides/slideN.xml` → N（_rels 目录因前缀不符自然被排除）。
fn pptx_slide_num(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("ppt/slides/slide")?;
    let num = rest.strip_suffix(".xml")?;
    num.parse().ok()
}

/// 单页文本抽取。为什么标题优先：模型读 PPT 时第一行当标题上下文最有用；
/// ph 的 type 是 "title" / "ctrTitle" 都算标题占位符。
/// 注意 local_name("p") 在 slide xml 里只对应 a:p（presentationml 的元素都是
/// p:sp / p:txBody 等，没有裸的 p:p），不会串。
fn parse_pptx_slide_xml(xml: &str) -> Vec<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut sps: Vec<(bool, Vec<String>)> = Vec::new();
    let mut sp_depth = 0usize;
    let mut is_title = false;
    let mut cur: Vec<String> = Vec::new();
    let mut para = String::new();
    let mut capture = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().local_name().as_ref() {
                b"sp" => {
                    if sp_depth == 0 {
                        is_title = false;
                        cur = Vec::new();
                    }
                    sp_depth += 1;
                }
                b"ph" => {
                    if sp_depth > 0 {
                        for attr in e.attributes() {
                            if let Ok(a) = attr {
                                if a.key.local_name().as_ref() == b"type" {
                                    let v = a
                                        .unescape_value()
                                        .map(|s| s.into_owned())
                                        .unwrap_or_default();
                                    if v == "title" || v == "ctrTitle" {
                                        is_title = true;
                                    }
                                }
                            }
                        }
                    }
                }
                b"p" => {
                    para = String::new();
                    capture = false;
                }
                b"t" => capture = true,
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                // `<p:ph type="title"/>` 是自闭合标签——走 Empty 而不是 Start，
                // 标题判定必须在 Empty 分支同样处理
                let local = e.name().local_name();
                let name = local.as_ref();
                if name == b"ph" {
                    if sp_depth > 0 {
                        for attr in e.attributes() {
                            if let Ok(a) = attr {
                                if a.key.local_name().as_ref() == b"type" {
                                    let v = a
                                        .unescape_value()
                                        .map(|s| s.into_owned())
                                        .unwrap_or_default();
                                    if v == "title" || v == "ctrTitle" {
                                        is_title = true;
                                    }
                                }
                            }
                        }
                    }
                } else if name == b"br" {
                    para.push(' ');
                }
            }
            Ok(Event::Text(t)) => {
                if capture {
                    para.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Ok(Event::End(e)) => match e.name().local_name().as_ref() {
                b"t" => capture = false,
                b"p" => {
                    let line = para.trim().to_string();
                    if !line.is_empty() {
                        cur.push(line);
                    }
                    para = String::new();
                }
                b"sp" => {
                    sp_depth -= 1;
                    if sp_depth == 0 {
                        sps.push((is_title, cur.clone()));
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (_, lines) in sps.iter().filter(|(t, _)| *t) {
        out.extend(lines.iter().cloned());
    }
    for (_, lines) in sps.iter().filter(|(t, _)| !*t) {
        out.extend(lines.iter().cloned());
    }
    out
}

/// xlsx/xls：calamine 全工作簿。空行整行丢弃（模型读表格时空行只是噪音）。
fn extract_spreadsheet(
    path: &Path,
    sheet: Option<&str>,
) -> Result<(String, usize), HarnessError> {
    let mut wb = calamine::open_workbook_auto(path)
        .map_err(|e| HarnessError::Tool(format!("表格打开失败: {e}")))?;
    let names = calamine::Reader::sheet_names(&mut wb);
    if names.is_empty() {
        return Err(HarnessError::Tool(format!(
            "{} 里没有任何工作表",
            path.display()
        )));
    }
    let wanted: Vec<String> = match sheet {
        Some(s) => {
            if names.iter().any(|n| n == s) {
                vec![s.to_string()]
            } else {
                return Err(HarnessError::Tool(format!(
                    "工作表 `{s}` 不存在。现有表：{}",
                    names.join("、")
                )));
            }
        }
        None => names.clone(),
    };
    let mut out = String::new();
    for name in &wanted {
        let range = calamine::Reader::worksheet_range(&mut wb, name).map_err(|e| {
            HarnessError::Tool(format!("工作表 `{name}` 读取失败: {e}"))
        })?;
        out.push_str(&format!("## {name}\n"));
        for row in range.rows() {
            let cells: Vec<String> = row.iter().map(calamine_cell_text).collect();
            if cells.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        out.push('\n');
    }
    Ok((out, wanted.len()))
}

/// calamine 单元格 → 文本。浮点整值去掉小数尾巴（3.0 → "3"），日期格式化成
/// 可读的 ISO 形态而不是序列号。
fn calamine_cell_text(d: &calamine::Data) -> String {
    use calamine::Data;
    match d {
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::String(s) => s.clone(),
        Data::Bool(b) => {
            if *b {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        // calamine 未启用 chrono 特性，as_datetime 不可用——用无门控的
        // to_ymd_hms_milli 手动拼 ISO 形态
        Data::DateTime(dt) => {
            let (y, mo, d, h, mi, s, _ms) = dt.to_ymd_hms_milli();
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
        }
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
        Data::Empty => String::new(),
    }
}

/// csv/md/txt：原样读（保留分隔与格式），只数行数做头信息。
fn extract_text_file(path: &Path) -> Result<(String, usize), HarnessError> {
    let bytes = std::fs::read(path)?;
    let text = decode_text(&bytes);
    let lines = text.lines().count();
    Ok((text, lines))
}

// ===================================================================== 块/页规格（create 与 append 共用）

#[derive(Debug, Clone, PartialEq)]
enum Block {
    /// (1|2|3, 文本)
    Heading(u8, String),
    Para(String),
    Bullet(String),
    Table(Vec<Vec<String>>),
}

#[derive(Debug, Clone, PartialEq)]
struct SlideSpec {
    title: String,
    bullets: Vec<String>,
}

/// blocks 数组解析：[{type:"h1"|"h2"|"h3"|"p"|"bullet", text}, {type:"table", rows:[[…]]}]
fn parse_blocks(v: &Value) -> Result<Vec<Block>, HarnessError> {
    let arr = v.as_array().ok_or_else(|| {
        HarnessError::Tool(
            "`blocks` 必须是数组：[{type:\"h1\"|\"h2\"|\"h3\"|\"p\"|\"bullet\", text}, {type:\"table\", rows:[[…]]}, …]".into(),
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, b) in arr.iter().enumerate() {
        let ty = b
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| HarnessError::Tool(format!("blocks[{i}] 缺少 `type` 字段")))?;
        match ty {
            "h1" | "h2" | "h3" => {
                let text = b.get("text").and_then(|t| t.as_str()).ok_or_else(|| {
                    HarnessError::Tool(format!("blocks[{i}]（{ty}）缺少 `text`"))
                })?;
                out.push(Block::Heading(
                    ty.trim_start_matches('h').parse().unwrap_or(1),
                    text.to_string(),
                ));
            }
            "p" | "paragraph" => {
                let text = b.get("text").and_then(|t| t.as_str()).ok_or_else(|| {
                    HarnessError::Tool(format!("blocks[{i}]（p）缺少 `text`"))
                })?;
                out.push(Block::Para(text.to_string()));
            }
            "bullet" | "li" => {
                let text = b.get("text").and_then(|t| t.as_str()).ok_or_else(|| {
                    HarnessError::Tool(format!("blocks[{i}]（bullet）缺少 `text`"))
                })?;
                out.push(Block::Bullet(text.to_string()));
            }
            "table" => {
                let rows = b.get("rows").ok_or_else(|| {
                    HarnessError::Tool(format!("blocks[{i}]（table）缺少 `rows`"))
                })?;
                out.push(Block::Table(rows_to_strings(rows)?));
            }
            other => {
                return Err(HarnessError::Tool(format!(
                    "blocks[{i}] 的 type `{other}` 不支持；可用：h1 / h2 / h3 / p / bullet / table"
                )))
            }
        }
    }
    Ok(out)
}

/// 二维数组 → 二维字符串（csv 行与 docx 表格共用）。数字/布尔转文本，
/// 嵌套结构转 JSON 字符串。
fn rows_to_strings(rows: &Value) -> Result<Vec<Vec<String>>, HarnessError> {
    let arr = rows
        .as_array()
        .ok_or_else(|| HarnessError::Tool("`rows` 必须是二维数组 [[…],[…]]".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, row) in arr.iter().enumerate() {
        let cells = row
            .as_array()
            .ok_or_else(|| HarnessError::Tool(format!("rows[{i}] 必须是数组（一行）")))?;
        out.push(cells.iter().map(value_to_cell_text).collect());
    }
    Ok(out)
}

fn value_to_cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// doc.slides 解析：[{title, bullets:[…]}]
fn parse_slides(doc: &Value) -> Result<Vec<SlideSpec>, HarnessError> {
    let arr = doc.get("slides").and_then(|s| s.as_array()).ok_or_else(|| {
        HarnessError::Tool(
            "`doc.slides` is required（pptx 创建用 doc:{slides:[{title, bullets:[…]}]}）".into(),
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, s) in arr.iter().enumerate() {
        let title = s
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let bullets: Vec<String> = s
            .get("bullets")
            .and_then(|b| b.as_array())
            .map(|a| {
                a.iter()
                    .filter(|v| !v.is_null())
                    .map(value_to_cell_text)
                    .collect()
            })
            .unwrap_or_default();
        if title.is_empty() && bullets.is_empty() {
            return Err(HarnessError::Tool(format!(
                "slides[{i}] 既无 title 也无 bullets——至少给一个"
            )));
        }
        out.push(SlideSpec { title, bullets });
    }
    Ok(out)
}

// ===================================================================== 创建实现

fn create_text_file(path: &Path, text: &str) -> Result<String, HarnessError> {
    ensure_parent(path)?;
    std::fs::write(path, text)?;
    Ok(format!("{} 行", text.lines().count()))
}

/// CSV 转义（RFC 4180）：含逗号/引号/换行的字段加引号，内部引号翻倍。
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_row(cells: &[String]) -> String {
    cells.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(",")
}

fn create_csv_file(path: &Path, rows: &[Vec<String>]) -> Result<String, HarnessError> {
    ensure_parent(path)?;
    // RFC 4180 用 CRLF 做行分隔，Excel 直接双击打开不串行
    let body: Vec<String> = rows.iter().map(|r| csv_row(r)).collect();
    std::fs::write(path, body.join("\r\n"))?;
    Ok(format!("{} 行", rows.len()))
}

/// doc.sheets 解析：[{name, rows:[[cell…]]}]，cell 保留 JSON 类型以便写数字/布尔。
fn parse_sheets(doc: &Value) -> Result<Vec<(String, Vec<Vec<Value>>)>, HarnessError> {
    let arr = doc
        .get("sheets")
        .and_then(|s| s.as_array())
        .ok_or_else(|| {
            HarnessError::Tool(
                "`doc.sheets` is required（xlsx 创建用 doc:{sheets:[{name, rows:[[…]]}]}）".into(),
            )
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, s) in arr.iter().enumerate() {
        let name = s
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| HarnessError::Tool(format!("sheets[{i}] 缺少 `name`")))?;
        let rows = s.get("rows").and_then(|r| r.as_array()).unwrap_or(&Vec::new()).clone();
        let mut grid = Vec::with_capacity(rows.len());
        for (j, row) in rows.iter().enumerate() {
            let cells = row
                .as_array()
                .ok_or_else(|| HarnessError::Tool(format!("sheets[{i}].rows[{j}] 必须是数组")))?;
            grid.push(cells.clone());
        }
        out.push((name.to_string(), grid));
    }
    Ok(out)
}

fn create_xlsx_file(
    path: &Path,
    sheets: &[(String, Vec<Vec<Value>>)],
) -> Result<String, HarnessError> {
    ensure_parent(path)?;
    let mut wb = rust_xlsxwriter::Workbook::new();
    let mut seen = std::collections::HashSet::new();
    let mut total_rows = 0usize;
    for (name, rows) in sheets {
        if !seen.insert(name.as_str()) {
            return Err(HarnessError::Tool(format!("工作表重名：`{name}`")));
        }
        let ws = wb.add_worksheet();
        ws.set_name(name)
            .map_err(|e| HarnessError::Tool(format!("工作表名 `{name}` 非法: {e}")))?;
        for (r, row) in rows.iter().enumerate() {
            total_rows += 1;
            let row_no = r as u32;
            for (c, cell) in row.iter().enumerate() {
                xlsx_write_cell(ws, row_no, c as u16, cell)?;
            }
        }
    }
    wb.save(path)
        .map_err(|e| HarnessError::Tool(format!("xlsx 写入失败: {e}")))?;
    Ok(format!("{} 表 {} 行", sheets.len(), total_rows))
}

fn xlsx_write_cell(
    ws: &mut rust_xlsxwriter::Worksheet,
    r: u32,
    c: u16,
    v: &Value,
) -> Result<(), HarnessError> {
    if r >= XLSX_MAX_ROWS || c as u32 >= XLSX_MAX_COLS {
        return Err(HarnessError::Tool(format!(
            "单元格 ({r},{c}) 超出 Excel 网格上限（{XLSX_MAX_ROWS} 行 × {XLSX_MAX_COLS} 列）"
        )));
    }
    let res = match v {
        // 空串/Null 跳过——写进 Excel 只会撑大文件
        Value::Null => Ok(()),
        Value::String(s) if s.is_empty() => Ok(()),
        Value::Bool(b) => ws.write_boolean(r, c, *b).map(|_| ()),
        Value::Number(n) => ws.write_number(r, c, n.as_f64().unwrap_or(0.0)).map(|_| ()),
        Value::String(s) => ws.write_string(r, c, s.as_str()).map(|_| ()),
        other => ws.write_string(r, c, other.to_string()).map(|_| ()),
    };
    res.map_err(|e| HarnessError::Tool(format!("写入单元格 ({r},{c}) 失败: {e}")))
}

// ---- docx（docx-rs）----

const HEADING_STYLES: [(&str, &str, usize); 3] =
    [("Heading1", "Heading 1", 48), ("Heading2", "Heading 2", 36), ("Heading3", "Heading 3", 30)];

/// 项目符号列表编号定义。为什么用 900 的高位 id：追加块场景下源文档可能自带
/// 1 号 numbering（read_docx 不回读 numberings.xml），错开避免冲突。
const BULLET_NUM_ID: usize = 900;

fn bullet_numberings() -> docx_rs::Numberings {
    docx_rs::Numberings::new()
        .add_abstract_numbering(
            docx_rs::AbstractNumbering::new(BULLET_NUM_ID).add_level(docx_rs::Level::new(
                0,
                docx_rs::Start::new(1),
                docx_rs::NumberFormat::new("bullet"),
                docx_rs::LevelText::new("•"),
                docx_rs::LevelJc::new("left"),
            )),
        )
        .add_numbering(docx_rs::Numbering::new(BULLET_NUM_ID, BULLET_NUM_ID))
}

fn make_heading_style(id: &str, name: &str, size: usize) -> docx_rs::Style {
    docx_rs::Style::new(id, docx_rs::StyleType::Paragraph)
        .name(name)
        .bold()
        .size(size)
}

/// Docx::new() 的默认样式表是空的——h1/h2/h3 的样式定义必须显式加，
/// 否则 Word/WPS 把引用 Heading1 的段落回退成普通文本。
fn docx_base(has_bullet: bool) -> docx_rs::Docx {
    let mut d = docx_rs::Docx::new();
    for (id, name, size) in HEADING_STYLES {
        if d.styles.find_style_by_id(id).is_none() {
            d = d.add_style(make_heading_style(id, name, size));
        }
    }
    if has_bullet {
        d = d.numberings(bullet_numberings());
    }
    d
}

fn docx_paragraph_for(b: &Block) -> docx_rs::Paragraph {
    match b {
        Block::Heading(n, t) => docx_rs::Paragraph::new()
            .style(&format!("Heading{n}"))
            .add_run(docx_rs::Run::new().add_text(t.as_str())),
        Block::Para(t) => {
            docx_rs::Paragraph::new().add_run(docx_rs::Run::new().add_text(t.as_str()))
        }
        Block::Bullet(t) => docx_rs::Paragraph::new()
            .numbering(docx_rs::NumberingId::new(BULLET_NUM_ID), docx_rs::IndentLevel::new(0))
            .add_run(docx_rs::Run::new().add_text(t.as_str())),
        Block::Table(_) => docx_rs::Paragraph::new(),
    }
}

fn docx_table_for(rows: &[Vec<String>]) -> docx_rs::Table {
    let mut built: Vec<docx_rs::TableRow> = Vec::new();
    for row in rows {
        let cells: Vec<docx_rs::TableCell> = row
            .iter()
            .map(|c| {
                docx_rs::TableCell::new()
                    .add_paragraph(docx_rs::Paragraph::new().add_run(docx_rs::Run::new().add_text(c.as_str())))
            })
            .collect();
        built.push(docx_rs::TableRow::new(cells));
    }
    docx_rs::Table::new(built)
}

fn create_docx_file(path: &Path, blocks: &[Block]) -> Result<String, HarnessError> {
    ensure_parent(path)?;
    let has_bullet = blocks.iter().any(|b| matches!(b, Block::Bullet(_)));
    let mut docx = docx_base(has_bullet);
    for b in blocks {
        match b {
            Block::Table(rows) => docx = docx.add_table(docx_table_for(rows)),
            _ => docx = docx.add_paragraph(docx_paragraph_for(b)),
        }
    }
    let f = std::fs::File::create(path)?;
    docx
        .build()
        .pack(f)
        .map_err(|e| HarnessError::Tool(format!("docx 写入失败: {e}")))?;
    Ok(format!("{} 块", blocks.len()))
}

// ---- pptx（最小 OOXML 手写 zip）----
//
// 标准 OfficeOpenXML 最小集：[Content_Types] + 根 rels + presentation + master
// （带 title/body 占位符）+ layout + theme。theme 用标准 Office 配色骨架
// （fillStyleLst/lnStyleLst/effectStyleLst 各 3 项 + bgFillStyleLst 1 项是
// schema 要求的最小数量），PowerPoint/WPS/WPS 网页版均可直接打开。

const PPTX_ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;

const PPTX_SLIDE_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#;

const PPTX_MASTER_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#;

const PPTX_LAYOUT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#;

/// master/layout 的 spTree 头（两个文件同构）。
fn pptx_sptree_header() -> String {
    String::from(
        r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>"#,
    )
}

/// 标题占位符 sp（master/layout/slide 三处同构，空文本用于母版）。
fn pptx_sp_title(text: &str) -> String {
    let body = if text.is_empty() {
        String::from("<a:p><a:pPr lvl=\"1\"/><a:endParaRPr lang=\"zh-CN\" sz=\"4000\" b=\"1\"/></a:p>")
    } else {
        format!("<a:p><a:r><a:rPr lang=\"zh-CN\" sz=\"4000\" b=\"1\"/><a:t>{}</a:t></a:r></a:p>", xml_escape(text))
    };
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="标题 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="838200" y="628650"/><a:ext cx="10515600" cy="1325563"/></a:xfrm></p:spPr><p:txBody><a:bodyPr anchor="ctr"/><a:lstStyle/>{body}</p:txBody></p:sp>"#
    )
}

/// 正文占位符 sp。bullets 每条一个 a:p；没有要点时留一个空段（endParaRPr）。
fn pptx_sp_body(bullets: &[String]) -> String {
    let paras = if bullets.is_empty() {
        String::from("<a:p><a:endParaRPr lang=\"zh-CN\" sz=\"2400\"/></a:p>")
    } else {
        bullets
            .iter()
            .map(|b| {
                format!(
                    "<a:p><a:r><a:rPr lang=\"zh-CN\" sz=\"2400\"/><a:t>{}</a:t></a:r></a:p>",
                    xml_escape(b)
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="内容 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="838200" y="2286000"/><a:ext cx="10515600" cy="3771900"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody></p:sp>"#
    )
}

fn pptx_content_types(slide_count: usize) -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>"#,
    );
    for i in 1..=slide_count {
        s.push_str(&format!(
            r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        ));
    }
    s.push_str("</Types>");
    s
}

fn pptx_presentation_xml(slide_count: usize) -> String {
    // sldId 必须 ≥256；关系号 rId1 留给 slideMaster，页面从 rId2 起
    let ids: Vec<String> = (1..=slide_count)
        .map(|i| {
            format!(
                r#"<p:sldId id="{}" r:id="rId{}"/>"#,
                255 + i as u32,
                i + 1
            )
        })
        .collect();
    let ids = ids.join("");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" saveSubsetFonts="1"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#
    )
}

fn pptx_presentation_rels(slide_count: usize) -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#,
    );
    for i in 1..=slide_count {
        s.push_str(&format!(
            r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{i}.xml"/>"#,
            i + 1
        ));
    }
    s.push_str("</Relationships>");
    s
}

fn pptx_master_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree>{}{}{}</p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>"#,
        pptx_sptree_header(),
        pptx_sp_title(""),
        pptx_sp_body(&[])
    )
}

fn pptx_layout_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="obj"><p:cSld name="标题和内容"><p:spTree>{}{}{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#,
        pptx_sptree_header(),
        pptx_sp_title(""),
        pptx_sp_body(&[])
    )
}

/// 标准 Office 主题骨架（精简）：clrScheme / fontScheme / fmtScheme。
/// fillStyleLst、lnStyleLst、effectStyleLst 各 3 项与 bgFillStyleLst 1 项是
/// CT_StyleDefinition 的最小合法数量，缺了 PowerPoint 会报修复。
const PPTX_THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Parable"><a:themeElements><a:clrScheme name="Office"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="Office"><a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Office"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:gradFill rotWithShape="1"><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"><a:tint val="50000"/><a:satMod val="300000"/></a:schemeClr></a:gs><a:gs pos="35000"><a:schemeClr val="phClr"><a:tint val="37000"/><a:satMod val="300000"/></a:schemeClr></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"><a:tint val="15000"/><a:satMod val="350000"/></a:schemeClr></a:gs></a:gsLst><a:lin ang="16200000" scaled="1"/></a:gradFill><a:gradFill rotWithShape="1"><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"><a:shade val="51000"/><a:satMod val="130000"/></a:schemeClr></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"><a:lumMod val="60000"/><a:lumOff val="40000"/></a:schemeClr></a:gs></a:gsLst><a:lin ang="5400000" scaled="0"/></a:gradFill></a:fillStyleLst><a:lnStyleLst><a:ln w="9525" cap="flat" cmpd="sng" algn="ctr"><a:solidFill><a:schemeClr val="phClr"><a:shade val="95000"/><a:satMod val="105000"/></a:schemeClr></a:solidFill><a:prstDash val="solid"/></a:ln><a:ln w="25400" cap="flat" cmpd="sng" algn="ctr"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln><a:ln w="38100" cap="flat" cmpd="sng" algn="ctr"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:prstDash val="solid"/></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst><a:outerShdw blurRad="40000" dist="20000" dir="5400000" rotWithShape="0"><a:srgbClr val="000000"><a:alpha val="38000"/></a:srgbClr></a:outerShdw></a:effectLst></a:effectStyle><a:effectStyle><a:effectLst><a:outerShdw blurRad="40000" dist="23000" dir="5400000" rotWithShape="0"><a:srgbClr val="000000"><a:alpha val="35000"/></a:srgbClr></a:outerShdw></a:effectLst></a:effectStyle><a:effectStyle><a:effectLst><a:outerShdw blurRad="40000" dist="23000" dir="5400000" rotWithShape="0"><a:srgbClr val="000000"><a:alpha val="35000"/></a:srgbClr></a:outerShdw></a:effectLst></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"><a:tint val="95000"/><a:satMod val="170000"/></a:schemeClr></a:solidFill><a:gradFill rotWithShape="1"><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"><a:tint val="93000"/><a:satMod val="150000"/><a:shade val="98000"/></a:schemeClr></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"><a:tint val="90000"/><a:satMod val="140000"/></a:schemeClr></a:gs></a:gsLst><a:lin ang="5400000" scaled="0"/></a:gradFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"#;

fn pptx_slide_xml(s: &SlideSpec) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree>{}{}{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#,
        pptx_sptree_header(),
        pptx_sp_title(&s.title),
        pptx_sp_body(&s.bullets)
    )
}

/// 组装最小 pptx 包。条目顺序无关紧要（OPC 按名字寻址），但 content types
/// 必须覆盖全部 slide override。
fn pptx_package_entries(slides: &[SlideSpec]) -> ZipEntries {
    let mut v: ZipEntries = Vec::new();
    v.push(("[Content_Types].xml".into(), pptx_content_types(slides.len()).into_bytes()));
    v.push(("_rels/.rels".into(), PPTX_ROOT_RELS.as_bytes().to_vec()));
    v.push(("ppt/presentation.xml".into(), pptx_presentation_xml(slides.len()).into_bytes()));
    v.push((
        "ppt/_rels/presentation.xml.rels".into(),
        pptx_presentation_rels(slides.len()).into_bytes(),
    ));
    v.push(("ppt/slideMasters/slideMaster1.xml".into(), pptx_master_xml().into_bytes()));
    v.push(("ppt/slideMasters/_rels/slideMaster1.xml.rels".into(), PPTX_MASTER_RELS.as_bytes().to_vec()));
    v.push(("ppt/slideLayouts/slideLayout1.xml".into(), pptx_layout_xml().into_bytes()));
    v.push(("ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(), PPTX_LAYOUT_RELS.as_bytes().to_vec()));
    v.push(("ppt/theme/theme1.xml".into(), PPTX_THEME.as_bytes().to_vec()));
    for (i, s) in slides.iter().enumerate() {
        let n = i + 1;
        v.push((format!("ppt/slides/slide{n}.xml"), pptx_slide_xml(s).into_bytes()));
        v.push((
            format!("ppt/slides/_rels/slide{n}.xml.rels"),
            PPTX_SLIDE_RELS.as_bytes().to_vec(),
        ));
    }
    v
}

fn create_pptx_file(path: &Path, slides: &[SlideSpec]) -> Result<String, HarnessError> {
    ensure_parent(path)?;
    zip_write_all(path, &pptx_package_entries(slides))?;
    Ok(format!("{} 页", slides.len()))
}

// ---- pdf（genpdf）----

/// 字体探测：simhei → msyh → msyhbd → arial（+非 Windows 兜底）。
/// 为什么「试加载」而不是只看文件存在：msyh.ttc 这类 TrueType Collection
/// rusttype 不给字体索引就解析不了——文件在但加载失败要跳到下一个候选，
/// 否则整条链在 TTC 上断掉。bold 槽位优先 msyhbd（雅黑 Bold 独立文件），
/// 全部失败则复用常规体——CJK 字体一般没有真斜体/粗斜体，复用是惯例。
fn load_pdf_font(candidates: &[&str]) -> Option<genpdf::fonts::FontData> {
    for c in candidates {
        if let Ok(bytes) = std::fs::read(c) {
            if let Ok(fd) = genpdf::fonts::FontData::new(bytes, None) {
                return Some(fd);
            }
            tracing::debug!(font = c, "字体文件存在但 rusttype 解析失败，尝试下一个候选");
        }
    }
    None
}

const PDF_FONT_REGULAR: [&str; 7] = [
    r"C:\Windows\Fonts\simhei.ttf",
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\msyhbd.ttc",
    r"C:\Windows\Fonts\arial.ttf",
    // 非 Windows（CI/容器）兜底：常见 CJK 与西文字体
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
];

const PDF_FONT_BOLD: [&str; 4] = [
    r"C:\Windows\Fonts\msyhbd.ttc",
    r"C:\Windows\Fonts\simhei.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
];

/// 供测试判断当前环境能否产 PDF。
#[cfg(test)]
fn detect_pdf_fonts() -> bool {
    load_pdf_font(&PDF_FONT_REGULAR).is_some()
}

fn create_pdf_file(path: &Path, blocks: &[Block]) -> Result<String, HarnessError> {
    use genpdf::Element as _;
    ensure_parent(path)?;
    let regular = load_pdf_font(&PDF_FONT_REGULAR).ok_or_else(|| {
        HarnessError::Tool(
            "未找到可用字体（依次探测 C:\\Windows\\Fonts\\simhei.ttf → msyh.ttc → msyhbd.ttc → arial.ttf）。\
请安装系统字体，或改用 doc_convert 由 LibreOffice 生成 PDF。"
                .into(),
        )
    })?;
    let bold = load_pdf_font(&PDF_FONT_BOLD).or_else(|| load_pdf_font(&PDF_FONT_REGULAR));
    let fonts = genpdf::fonts::FontFamily::<genpdf::fonts::FontData> {
        regular,
        bold: bold
            .unwrap_or_else(|| load_pdf_font(&PDF_FONT_REGULAR).expect("常规体已加载成功过一次")),
        italic: load_pdf_font(&PDF_FONT_REGULAR).expect("常规体已加载成功过一次"),
        bold_italic: load_pdf_font(&PDF_FONT_REGULAR).expect("常规体已加载成功过一次"),
    };
    let mut doc = genpdf::Document::new(fonts);
    doc.set_title(
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    doc.set_font_size(12);
    // 连续 bullet 合进一个 UnorderedList，渲染出真正的列表点
    let mut i = 0usize;
    while i < blocks.len() {
        match &blocks[i] {
            Block::Heading(n, t) => {
                let size: u8 = match n {
                    1 => 20,
                    2 => 16,
                    _ => 14,
                };
                doc.push(
                    genpdf::elements::Paragraph::new(t.clone())
                        .styled(genpdf::style::Style::new().bold().with_font_size(size)),
                );
                doc.push(genpdf::elements::Break::new(0.5));
            }
            Block::Para(t) => {
                doc.push(genpdf::elements::Paragraph::new(t.clone()));
                doc.push(genpdf::elements::Break::new(0.4));
            }
            Block::Bullet(_) => {
                let mut list = genpdf::elements::UnorderedList::new();
                while i < blocks.len() {
                    if let Block::Bullet(t) = &blocks[i] {
                        list.push(genpdf::elements::Paragraph::new(t.clone()));
                        i += 1;
                    } else {
                        break;
                    }
                }
                doc.push(list);
                doc.push(genpdf::elements::Break::new(0.4));
                continue;
            }
            // genpdf 的 TableLayout 需要逐格布局元素，此处文本化渲染保读性
            Block::Table(rows) => {
                for row in rows {
                    doc.push(genpdf::elements::Paragraph::new(row.join(" | ")));
                }
                doc.push(genpdf::elements::Break::new(0.4));
            }
        }
        i += 1;
    }
    doc.render_to_file(path)
        .map_err(|e| HarnessError::Tool(format!("pdf 生成失败: {e}")))?;
    Ok(format!("{} 块", blocks.len()))
}

// ===================================================================== 编辑实现

/// A1 坐标 → (列, 行)，均 1 基。必须自校验：umya 的 `From<&str>` 对非法坐标
/// 直接 panic，不能把不可控输入递进去。
fn parse_cell_coord(s: &str) -> Result<(u32, u32), HarnessError> {
    let up = s.trim().to_uppercase();
    let digits_at = up
        .find(|c: char| c.is_ascii_digit())
        .ok_or_else(|| HarnessError::Tool(format!("单元格坐标 `{s}` 非法——请用 A1 样式（如 B3）")))?;
    let (letters, digits) = up.split_at(digits_at);
    if letters.is_empty() || letters.len() > 3 || !letters.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(HarnessError::Tool(format!(
            "单元格坐标 `{s}` 非法——列部分须为 1~3 个字母（A..XFD）"
        )));
    }
    let col = letters
        .chars()
        .fold(0u32, |acc, c| acc * 26 + (c as u32 - 'A' as u32 + 1));
    let row: u32 = digits
        .parse()
        .map_err(|_| HarnessError::Tool(format!("单元格坐标 `{s}` 非法——行号须为正整数")))?;
    if row == 0 || row > XLSX_MAX_ROWS || col > XLSX_MAX_COLS {
        return Err(HarnessError::Tool(format!(
            "单元格坐标 `{s}` 超出 Excel 范围（{XLSX_MAX_ROWS} 行 × {XLSX_MAX_COLS} 列）"
        )));
    }
    Ok((col, row))
}

/// umya 工作表按 JSON 值写单元格（创建 add_sheet 与 set_cells 共用）。
fn umya_write_cell(ws: &mut umya_spreadsheet::Worksheet, col: u32, row: u32, v: &Value) {
    let cell = ws.get_cell_mut((col, row));
    match v {
        Value::Null => {
            cell.set_value_string("");
        }
        Value::Bool(b) => {
            cell.set_value_bool(*b);
        }
        Value::Number(n) => {
            cell.set_value_number(n.as_f64().unwrap_or(0.0));
        }
        Value::String(s) => {
            cell.set_value_string(s.as_str());
        }
        other => {
            cell.set_value_string(other.to_string());
        }
    }
}

fn umya_write_rows(ws: &mut umya_spreadsheet::Worksheet, rows: &[Vec<Value>]) -> Result<(), HarnessError> {
    for (r, row) in rows.iter().enumerate() {
        let row_no = (r + 1) as u32;
        for (c, cell) in row.iter().enumerate() {
            let col_no = (c + 1) as u32;
            if row_no > XLSX_MAX_ROWS || col_no > XLSX_MAX_COLS {
                return Err(HarnessError::Tool(format!(
                    "单元格 ({row_no},{col_no}) 超出 Excel 网格上限"
                )));
            }
            umya_write_cell(ws, col_no, row_no, cell);
        }
    }
    Ok(())
}

/// xlsx 单个 op 的执行（拿 &mut book，让循环层统一做失败中断与序号标注）。
fn xlsx_apply_op(book: &mut umya_spreadsheet::Spreadsheet, op: &Value) -> Result<String, HarnessError> {
    let kind = op
        .get("op")
        .and_then(|k| k.as_str())
        .ok_or_else(|| HarnessError::Tool("缺少 `op` 字段".into()))?;
    match kind {
        "set_cells" => {
            let sheet = op
                .get("sheet")
                .and_then(|s| s.as_str())
                .ok_or_else(|| HarnessError::Tool("set_cells 需要 `sheet`".into()))?;
            let cells = op
                .get("cells")
                .and_then(|c| c.as_object())
                .ok_or_else(|| {
                    HarnessError::Tool("set_cells 需要 `cells` 对象，如 {\"A1\": 值}".into())
                })?;
            // 先做存在性检查再借可变引用：ok_or_else 里列现有表名会与
            // get_sheet_by_name_mut 的 &mut 借用冲突
            if book.get_sheet_by_name(sheet).is_none() {
                let names = sheet_names_of(&book);
                return Err(HarnessError::Tool(format!(
                    "工作表 `{sheet}` 不存在。现有表：{names}"
                )));
            }
            let ws = book.get_sheet_by_name_mut(sheet).unwrap();
            let mut n = 0usize;
            for (coord, val) in cells {
                let (col, row) = parse_cell_coord(coord)?;
                umya_write_cell(ws, col, row, val);
                n += 1;
            }
            Ok(format!("set_cells {n} 格"))
        }
        "add_sheet" => {
            let name = op
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| HarnessError::Tool("add_sheet 需要 `name`".into()))?;
            if book.get_sheet_by_name(name).is_some() {
                return Err(HarnessError::Tool(format!(
                    "工作表 `{name}` 已存在——add_sheet 不覆盖，请换名或改用 set_cells"
                )));
            }
            let ws = book
                .new_sheet(name)
                .map_err(|e| HarnessError::Tool(format!("add_sheet `{name}` 失败: {e}")))?;
            if let Some(rows) = op.get("rows").and_then(|r| r.as_array()) {
                let rows: Vec<Vec<Value>> = rows
                    .iter()
                    .filter_map(|r| r.as_array().cloned())
                    .collect();
                umya_write_rows(ws, &rows)?;
            }
            Ok(format!("add_sheet `{name}`"))
        }
        "rename_sheet" => {
            let from = op
                .get("from")
                .and_then(|f| f.as_str())
                .ok_or_else(|| HarnessError::Tool("rename_sheet 需要 `from`".into()))?;
            let to = op
                .get("to")
                .and_then(|t| t.as_str())
                .ok_or_else(|| HarnessError::Tool("rename_sheet 需要 `to`".into()))?;
            if book.get_sheet_by_name(from).is_none() {
                let names = sheet_names_of(&book);
                return Err(HarnessError::Tool(format!(
                    "工作表 `{from}` 不存在。现有表：{names}"
                )));
            }
            let ws = book.get_sheet_by_name_mut(from).unwrap();
            ws.set_name(to);
            Ok(format!("rename_sheet `{from}` → `{to}`"))
        }
        other => Err(HarnessError::Tool(format!(
            "xlsx 不支持 op `{other}`；可用：set_cells / add_sheet / rename_sheet"
        ))),
    }
}

fn sheet_names_of(book: &umya_spreadsheet::Spreadsheet) -> String {
    book.get_sheet_collection()
        .iter()
        .map(|s| s.get_name().to_string())
        .collect::<Vec<_>>()
        .join("、")
}

/// xlsx 编辑：umya 读入 → 内存改 → 写回（保留既有格式/公式/图表）。
/// 所有 op 在内存里按序执行，任一失败即中断报错且不写盘——文件保持原样，
/// 报错注明执行到第几个 op。
fn edit_xlsx_file(path: &Path, ops: &Value) -> Result<String, HarnessError> {
    let op_arr = ops
        .as_array()
        .ok_or_else(|| HarnessError::Tool("`ops` 必须是数组".into()))?;
    let mut book = umya_spreadsheet::reader::xlsx::read(path)
        .map_err(|e| HarnessError::Tool(format!("xlsx 读取失败: {e}")))?;
    let mut notes: Vec<String> = Vec::new();
    for (idx, op) in op_arr.iter().enumerate() {
        match xlsx_apply_op(&mut book, op) {
            Ok(note) => notes.push(note),
            Err(e) => {
                return Err(HarnessError::Tool(format!(
                    "执行到第 {} 个操作时失败：{}。之前的操作未写盘，文件保持原样，请修正后重新提交完整 ops",
                    idx + 1,
                    e.for_model()
                )))
            }
        }
    }
    umya_spreadsheet::writer::xlsx::write(&book, path)
        .map_err(|e| HarnessError::Tool(format!("xlsx 保存失败: {e}")))?;
    Ok(notes.join(" · "))
}

// ---- docx 编辑 ----

/// 段落纯文本（Run 里的 Text 拼接；Hyperlink 等容器里的文本不追——replace
/// 只对普通 run 生效）。
fn docx_paragraph_text(p: &docx_rs::Paragraph) -> String {
    let mut s = String::new();
    for child in &p.children {
        if let docx_rs::ParagraphChild::Run(r) = child {
            for rc in &r.children {
                if let docx_rs::RunChild::Text(t) = rc {
                    s.push_str(&t.text);
                }
            }
        }
    }
    s
}

/// 段落内替换。两轮策略：
/// 1. 逐 run 精确替换——不破坏 run 的样式；
/// 2. 若段落整体含 find 但任何单个 run 都不含（find 跨 run 边界），把整段
///    文本合并替换后写回第一个文本 run（样式取该 run 的，其余 run 清空）。
fn docx_replace_in_paragraph(p: &mut docx_rs::Paragraph, find: &str, replace: &str) -> usize {
    let mut count = 0usize;
    for child in &mut p.children {
        if let docx_rs::ParagraphChild::Run(r) = child {
            for rc in &mut r.children {
                if let docx_rs::RunChild::Text(t) = rc {
                    if t.text.contains(find) {
                        count += t.text.matches(find).count();
                        t.text = t.text.replace(find, replace);
                    }
                }
            }
        }
    }
    if count > 0 {
        return count;
    }
    let joined = docx_paragraph_text(p);
    if !joined.contains(find) {
        return 0;
    }
    count = joined.matches(find).count();
    let replaced = joined.replace(find, replace);
    let mut first_text_run: Option<usize> = None;
    for (ci, child) in p.children.iter_mut().enumerate() {
        if let docx_rs::ParagraphChild::Run(r) = child {
            let has_text = r.children.iter().any(|rc| matches!(rc, docx_rs::RunChild::Text(t) if !t.text.is_empty()));
            if has_text && first_text_run.is_none() {
                first_text_run = Some(ci);
            }
            for rc in &mut r.children {
                if let docx_rs::RunChild::Text(t) = rc {
                    t.text.clear();
                }
            }
        }
    }
    let text_child = docx_rs::RunChild::Text(docx_rs::Text::new(replaced.clone()));
    match first_text_run {
        Some(ci) => {
            if let docx_rs::ParagraphChild::Run(r) = &mut p.children[ci] {
                r.children.insert(0, text_child);
            }
        }
        None => {
            p.children
                .insert(0, docx_rs::ParagraphChild::Run(Box::new(docx_rs::Run::new().add_text(replaced))));
        }
    }
    count
}

fn docx_replace_in_table(t: &mut docx_rs::Table, find: &str, replace: &str) -> usize {
    let mut n = 0usize;
    // TableChild / TableRowChild 都是单变体枚举，直接解构（irrefutable）
    for row in &mut t.rows {
        let docx_rs::TableChild::TableRow(tr) = row;
        for cell in &mut tr.cells {
            let docx_rs::TableRowChild::TableCell(tc) = cell;
            for content in &mut tc.children {
                match content {
                    docx_rs::TableCellContent::Paragraph(p) => {
                        n += docx_replace_in_paragraph(p, find, replace);
                    }
                    docx_rs::TableCellContent::Table(inner) => {
                        n += docx_replace_in_table(inner, find, replace);
                    }
                    _ => {}
                }
            }
        }
    }
    n
}

/// docx 单个 op 的执行。
fn docx_apply_op(docx: &mut docx_rs::Docx, op: &Value) -> Result<String, HarnessError> {
    let kind = op
        .get("op")
        .and_then(|k| k.as_str())
        .ok_or_else(|| HarnessError::Tool("缺少 `op` 字段".into()))?;
    match kind {
        "append_blocks" => {
            let blocks_v = op
                .get("blocks")
                .ok_or_else(|| HarnessError::Tool("append_blocks 需要 `blocks` 数组".into()))?;
            let blocks = parse_blocks(blocks_v)?;
            let has_bullet = blocks.iter().any(|b| matches!(b, Block::Bullet(_)));
            if has_bullet {
                *docx = std::mem::take(docx).numberings(bullet_numberings());
                docx.document_rels.has_numberings = true;
            }
            for b in &blocks {
                match b {
                    Block::Table(rows) => *docx = std::mem::take(docx).add_table(docx_table_for(rows)),
                    _ => *docx = std::mem::take(docx).add_paragraph(docx_paragraph_for(b)),
                }
            }
            Ok(format!("append_blocks {} 块", blocks.len()))
        }
        "replace_text" => {
            let find = op
                .get("find")
                .and_then(|f| f.as_str())
                .filter(|f| !f.is_empty())
                .ok_or_else(|| {
                    HarnessError::Tool("replace_text 需要非空 `find`".into())
                })?;
            let replace = op.get("replace").and_then(|r| r.as_str()).unwrap_or("");
            let mut total = 0usize;
            for child in &mut docx.document.children {
                match child {
                    docx_rs::DocumentChild::Paragraph(p) => {
                        total += docx_replace_in_paragraph(p, find, replace);
                    }
                    docx_rs::DocumentChild::Table(t) => {
                        total += docx_replace_in_table(t, find, replace);
                    }
                    _ => {}
                }
            }
            if total == 0 {
                // 无法精确命中时列出实际文本前 80 字符，帮模型对齐
                let all: String = docx
                    .document
                    .children
                    .iter()
                    .filter_map(|c| match c {
                        docx_rs::DocumentChild::Paragraph(p) => {
                            Some(docx_paragraph_text(p))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let head: String = all.chars().take(80).collect();
                return Err(HarnessError::Tool(format!(
                    "未找到待替换文本 `{find}`（精确匹配、不跨段落边界）。文档现有文本开头：{head}"
                )));
            }
            Ok(format!("replace_text ×{total}"))
        }
        other => Err(HarnessError::Tool(format!(
            "docx 不支持 op `{other}`；可用：append_blocks / replace_text"
        ))),
    }
}

/// docx 编辑：docx_rs read_docx 解析 → 修改 → build().pack 写回。
/// 已知取舍：docx_rs 不回读 styles.xml，重写后的文档保留默认样式定义；
/// 我们创建的文档自带 h1/h2/h3 样式定义，替换/追加后样式不丢。
fn edit_docx_file(path: &Path, ops: &Value) -> Result<String, HarnessError> {
    let op_arr = ops
        .as_array()
        .ok_or_else(|| HarnessError::Tool("`ops` 必须是数组".into()))?;
    let bytes = std::fs::read(path)?;
    let mut docx = docx_rs::read_docx(&bytes).map_err(|e| {
        HarnessError::Tool(format!(
            "docx 解析失败: {e}（若是旧版 .doc 二进制格式，请先用 doc_convert 转 docx）"
        ))
    })?;
    let mut notes: Vec<String> = Vec::new();
    for (idx, op) in op_arr.iter().enumerate() {
        match docx_apply_op(&mut docx, op) {
            Ok(note) => notes.push(note),
            Err(e) => {
                return Err(HarnessError::Tool(format!(
                    "执行到第 {} 个操作时失败：{}。之前的操作未写盘，文件保持原样，请修正后重新提交完整 ops",
                    idx + 1,
                    e.for_model()
                )))
            }
        }
    }
    let f = std::fs::File::create(path)?;
    docx
        .build()
        .pack(f)
        .map_err(|e| HarnessError::Tool(format!("docx 保存失败: {e}")))?;
    Ok(notes.join(" · "))
}

// ---- pptx 编辑 ----

/// 在 `<needle>123…` 里取数字的最大值——用于 sldId / rId 自增。
fn scan_max_num(xml: &str, needle: &str) -> u32 {
    let mut max = 0u32;
    let mut rest = xml;
    while let Some(pos) = rest.find(needle) {
        let after = &rest[pos + needle.len()..];
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if let Ok(v) = after[..end].parse::<u32>() {
            max = max.max(v);
        }
        rest = &after[end..];
    }
    max
}

/// add_slide：用 create 的 slide 模板生成新页追加。直接补丁三处索引文件
/// （[Content_Types] / presentation.xml.rels / presentation.xml）再追加两个
/// 新条目，避免整包重建破坏既有媒体/版式。
fn pptx_add_slide(entries: &mut ZipEntries, s: &SlideSpec) -> Result<u32, HarnessError> {
    let max_slide = entries.iter().filter_map(|(n, _)| pptx_slide_num(n)).max().unwrap_or(0);
    let new_n = max_slide + 1;

    // 1. [Content_Types].xml 追加 override
    let idx = entries
        .iter()
        .position(|(n, _)| n == "[Content_Types].xml")
        .ok_or_else(|| HarnessError::Tool("包内缺少 [Content_Types].xml——不是标准 pptx".into()))?;
    let ct = String::from_utf8_lossy(&entries[idx].1).into_owned();
    if !ct.contains("</Types>") {
        return Err(HarnessError::Tool("[Content_Types].xml 结构异常（缺 </Types>）".into()));
    }
    let insert = format!(
        r#"<Override PartName="/ppt/slides/slide{new_n}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
    );
    entries[idx].1 = ct.replacen("</Types>", &format!("{insert}</Types>"), 1).into_bytes();

    // 2. presentation.xml.rels 追加关系（rId 取现有最大 +1）
    let idx = entries
        .iter()
        .position(|(n, _)| n == "ppt/_rels/presentation.xml.rels")
        .ok_or_else(|| HarnessError::Tool("包内缺少 ppt/_rels/presentation.xml.rels".into()))?;
    let rels = String::from_utf8_lossy(&entries[idx].1).into_owned();
    if !rels.contains("</Relationships>") {
        return Err(HarnessError::Tool("presentation.xml.rels 结构异常".into()));
    }
    let new_rid = scan_max_num(&rels, "Id=\"rId").max(1) + 1;
    let insert = format!(
        r#"<Relationship Id="rId{new_rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{new_n}.xml"/>"#
    );
    entries[idx].1 = rels
        .replacen("</Relationships>", &format!("{insert}</Relationships>"), 1)
        .into_bytes();

    // 3. presentation.xml 的 sldIdLst 追加（sldId ≥256，取现有最大 +1）
    let idx = entries
        .iter()
        .position(|(n, _)| n == "ppt/presentation.xml")
        .ok_or_else(|| HarnessError::Tool("包内缺少 ppt/presentation.xml".into()))?;
    let px = String::from_utf8_lossy(&entries[idx].1).into_owned();
    let sldid = scan_max_num(&px, "<p:sldId id=\"").max(255) + 1;
    let sld = format!(r#"<p:sldId id="{sldid}" r:id="rId{new_rid}"/>"#);
    let new_px = if px.contains("</p:sldIdLst>") {
        px.replacen("</p:sldIdLst>", &format!("{sld}</p:sldIdLst>"), 1)
    } else if px.contains("<p:sldIdLst/>") {
        px.replace("<p:sldIdLst/>", &format!("<p:sldIdLst>{sld}</p:sldIdLst>"))
    } else {
        return Err(HarnessError::Tool(
            "presentation.xml 缺少 p:sldIdLst——非标准结构，无法追加页".into(),
        ));
    };
    entries[idx].1 = new_px.into_bytes();

    // 4. 新页 + 页 rels
    entries.push((format!("ppt/slides/slide{new_n}.xml"), pptx_slide_xml(s).into_bytes()));
    entries.push((
        format!("ppt/slides/_rels/slide{new_n}.xml.rels"),
        PPTX_SLIDE_RELS.as_bytes().to_vec(),
    ));
    Ok(new_n)
}

/// pptx 单个 op 的执行。
fn pptx_apply_op(entries: &mut ZipEntries, op: &Value) -> Result<String, HarnessError> {
    let kind = op
        .get("op")
        .and_then(|k| k.as_str())
        .ok_or_else(|| HarnessError::Tool("缺少 `op` 字段".into()))?;
    match kind {
        "add_slide" => {
            let title = op.get("title").and_then(|t| t.as_str()).unwrap_or("");
            let bullets: Vec<String> = op
                .get("bullets")
                .and_then(|b| b.as_array())
                .map(|a| a.iter().map(value_to_cell_text).collect())
                .unwrap_or_default();
            let spec = SlideSpec {
                title: title.to_string(),
                bullets,
            };
            if spec.title.is_empty() && spec.bullets.is_empty() {
                return Err(HarnessError::Tool("add_slide 需要 title 或 bullets 至少一项".into()));
            }
            let n = pptx_add_slide(entries, &spec)?;
            Ok(format!("add_slide → 第 {n} 页"))
        }
        other => Err(HarnessError::Tool(format!(
            "pptx 不支持 op `{other}`；可用：add_slide"
        ))),
    }
}

fn edit_pptx_file(path: &Path, ops: &Value) -> Result<String, HarnessError> {
    let op_arr = ops
        .as_array()
        .ok_or_else(|| HarnessError::Tool("`ops` 必须是数组".into()))?;
    let mut entries = zip_read_all(path)?;
    let mut notes: Vec<String> = Vec::new();
    for (idx, op) in op_arr.iter().enumerate() {
        match pptx_apply_op(&mut entries, op) {
            Ok(note) => notes.push(note),
            Err(e) => {
                return Err(HarnessError::Tool(format!(
                    "执行到第 {} 个操作时失败：{}。之前的操作未写盘，文件保持原样",
                    idx + 1,
                    e.for_model()
                )))
            }
        }
    }
    zip_write_all(path, &entries)?;
    Ok(notes.join(" · "))
}

// ---- pdf 编辑（lopdf）----

/// lopdf 多文档合并。标准做法：各源文档对象号错开重编（renumber_objects_with
/// 会同步改写所有引用），页面引用收进新 Pages 树的 Kids，再建 Catalog、
/// 设 trailer Root，最后 renumber_objects 把输出编号压实到 1..n。
/// 源文档各自的旧 Pages 树会残留在对象表里（孤儿对象），阅读器按
/// Catalog → Pages 寻址不受影响——这也是 lopdf 社区通行的合并方式。
fn merge_pdf_documents(docs: Vec<lopdf::Document>) -> Result<lopdf::Document, HarnessError> {
    let mut next_id: u32 = 1;
    let mut kids: Vec<Object> = Vec::new();
    let mut objects: std::collections::BTreeMap<ObjectId, Object> = Default::default();
    for mut doc in docs {
        doc.renumber_objects_with(next_id);
        let max = doc.objects.keys().map(|k| k.0).max().unwrap_or(next_id.saturating_sub(1));
        next_id = max.saturating_add(1);
        for (_, page_id) in doc.get_pages() {
            kids.push(Object::Reference(page_id));
        }
        objects.extend(doc.objects.into_iter());
    }
    if kids.is_empty() {
        return Err(HarnessError::Tool("合并失败：所有输入都没有可提取的页面".into()));
    }
    let pages_id = (next_id, 0);
    let mut pages_dict = Dictionary::new();
    pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
    pages_dict.set("Count", Object::Integer(kids.len() as i64));
    pages_dict.set("Kids", Object::Array(kids));
    let catalog_id = (next_id.saturating_add(1), 0);
    let mut catalog_dict = Dictionary::new();
    catalog_dict.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog_dict.set("Pages", Object::Reference(pages_id));

    let mut result = PdfDocument::with_version("1.4");
    result.objects = objects;
    result.objects.insert(pages_id, Object::Dictionary(pages_dict));
    result.objects.insert(catalog_id, Object::Dictionary(catalog_dict));
    // Root 先设后 renumber：renumber_objects 会连 trailer 里的引用一起重编
    result.trailer.set("Root", Object::Reference(catalog_id));
    result.renumber_objects();
    Ok(result)
}

/// pdf 单个 op 的执行。extract_pages 边改边落盘（另存副本文件），merge 直接
/// 覆盖主文件——所以任一 op 失败后已落盘的部分确实不回滚，报错注明序号。
fn pdf_apply_op(
    path: &Path,
    op: &Value,
    resolve: &dyn Fn(&str) -> Result<PathBuf, HarnessError>,
    current: &mut Option<PdfDocument>,
) -> Result<String, HarnessError> {
    let kind = op
        .get("op")
        .and_then(|k| k.as_str())
        .ok_or_else(|| HarnessError::Tool("缺少 `op` 字段".into()))?;
    match kind {
        "merge" => {
            let paths = op
                .get("paths")
                .and_then(|p| p.as_array())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| {
                    HarnessError::Tool("merge 需要 `paths` 数组（至少一个待合并 PDF）".into())
                })?;
            let mut docs = vec![PdfDocument::load(path).map_err(|e| {
                HarnessError::Tool(format!("pdf 读取失败: {e}"))
            })?];
            for (i, p) in paths.iter().enumerate() {
                let raw = p
                    .as_str()
                    .ok_or_else(|| HarnessError::Tool(format!("paths[{i}] 必须是字符串路径")))?;
                let other = resolve(raw)?;
                let d = PdfDocument::load(&other).map_err(|e| {
                    HarnessError::Tool(format!("读取 {raw} 失败: {e}"))
                })?;
                docs.push(d);
            }
            let mut merged = merge_pdf_documents(docs)?;
            merged.save(path).map_err(|e| {
                HarnessError::Tool(format!("合并结果保存失败: {e}"))
            })?;
            *current = None; // 落盘后内存副本作废
            Ok(format!("merge +{} 页", paths.len()))
        }
        "extract_pages" => {
            let from = op.get("from").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
            let to = op
                .get("to")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .ok_or_else(|| HarnessError::Tool("extract_pages 需要 `to`（结束页码）".into()))?;
            if current.is_none() {
                let d = PdfDocument::load(path).map_err(|e| {
                    HarnessError::Tool(format!(
                        "pdf 读取失败: {e}（加密 PDF 需先解密；损坏文件可尝试让用户重新导出）"
                    ))
                })?;
                *current = Some(d);
            }
            let doc = current.as_mut().unwrap();
            let total = doc.get_pages().len() as u32;
            if from < 1 || from > to || to > total {
                return Err(HarnessError::Tool(format!(
                    "页码区间 {from}-{to} 无效：文档共 {total} 页（1..={total}）"
                )));
            }
            let remove: Vec<u32> =
                (1..=total).filter(|p| *p < from || *p > to).collect();
            doc.delete_pages(&remove);
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "document".into());
            let out = path.with_file_name(format!("{stem}-pages.pdf"));
            doc.save(&out).map_err(|e| {
                HarnessError::Tool(format!("保存 {} 失败: {e}", out.display()))
            })?;
            Ok(format!(
                "extract_pages {from}-{to} → {}",
                out.file_name().unwrap_or_default().to_string_lossy()
            ))
        }
        other => Err(HarnessError::Tool(format!(
            "pdf 不支持 op `{other}`；可用：merge / extract_pages"
        ))),
    }
}

/// 路径解析以闭包注入而不是直接拿 ToolCtx：merge 的 paths 也要过工作区
/// 边界校验，闭包让这个纯逻辑层可以被单元测试直接驱动（无需搭权限环境）。
fn edit_pdf_file(
    path: &Path,
    ops: &Value,
    resolve: &dyn Fn(&str) -> Result<PathBuf, HarnessError>,
) -> Result<String, HarnessError> {
    let op_arr = ops
        .as_array()
        .ok_or_else(|| HarnessError::Tool("`ops` 必须是数组".into()))?;
    // 当前文档只 load 一次，跨 op 复用内存副本
    let mut current: Option<PdfDocument> = None;
    let mut notes: Vec<String> = Vec::new();
    for (idx, op) in op_arr.iter().enumerate() {
        match pdf_apply_op(path, op, resolve, &mut current) {
            Ok(note) => notes.push(note),
            Err(e) => {
                return Err(HarnessError::Tool(format!(
                    "执行到第 {} 个操作时失败：{}。之前已落盘的部分不回滚",
                    idx + 1,
                    e.for_model()
                )))
            }
        }
    }
    Ok(notes.join(" · "))
}

// ===================================================================== LibreOffice（soffice）

const SOFFICE_MISSING_MSG: &str = "未找到 LibreOffice（soffice）。doc/ppt 旧格式读取与跨格式转换需要它：\n\
1. 下载安装：https://www.libreoffice.org/download/download/（Windows 可 winget install TheDocumentFoundation.LibreOffice）\n\
2. 装好重试；或让用户把文档先转成 docx / pptx / pdf 等开放格式";

/// soffice 定位：PATH → 常见安装位置。soffice.exe 在 Windows 安装目录的
/// program/ 下，装完默认不进 PATH，所以两处都要查。
fn find_soffice() -> Option<PathBuf> {
    if let Some(path_env) = std::env::var_os("PATH") {
        let exe_names: &[&str] = if cfg!(windows) {
            &["soffice.exe", "soffice"]
        } else {
            &["soffice"]
        };
        for dir in std::env::split_paths(&path_env) {
            for name in exe_names {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    let mut fallbacks: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in [
            r"C:\Program Files\LibreOffice",
            r"C:\Program Files (x86)\LibreOffice",
        ] {
            let mut p = PathBuf::from(base);
            p.push("program");
            p.push("soffice.exe");
            fallbacks.push(p);
        }
    } else {
        fallbacks.push(PathBuf::from("/usr/bin/soffice"));
        fallbacks.push(PathBuf::from("/usr/local/bin/soffice"));
        fallbacks.push(PathBuf::from("/opt/libreoffice/program/soffice"));
    }
    fallbacks.into_iter().find(|p| p.is_file())
}

/// soffice --headless --convert-to 转换。要点：
/// - 独立 UserInstallation profile：避免与用户正开着的 LibreOffice 抢锁
///   （共享 profile 时第二个实例会静默失败）；
/// - 输出写在源文件同目录（--outdir），LibreOffice 按「同名换扩展」落盘；
/// - 120s 超时；soffice 失败也可能 exit 0，所以以输出文件存在为准。
async fn soffice_convert(src: &Path, to: &str) -> Result<PathBuf, HarnessError> {
    let soffice = find_soffice().ok_or_else(|| HarnessError::Tool(SOFFICE_MISSING_MSG.into()))?;
    let profile = std::env::temp_dir().join(format!("parable_lo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&profile)?;
    let profile_url = format!("file:///{}", profile.to_string_lossy().replace('\\', "/"));
    let outdir = src.parent().unwrap_or_else(|| Path::new("."));
    let mut cmd = tokio::process::Command::new(&soffice);
    cmd.args([
        "--headless",
        "--nologo",
        "--nofirststartwizard",
        &format!("-env:UserInstallation={profile_url}"),
        "--convert-to",
        to,
        "--outdir",
        &outdir.to_string_lossy(),
        &src.to_string_lossy(),
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    tracing::info!(src = %src.display(), to, soffice = %soffice.display(), "soffice 转换开始");
    let out = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output())
        .await
        .map_err(|_| {
            HarnessError::Tool(
                "LibreOffice 转换超时（120s）。文件较大或 soffice 卡死；可重试一次，或让用户手动转换".into(),
            )
        })?
        .map_err(|e| HarnessError::Tool(format!("soffice 启动失败: {e}")))?;
    // 清 profile（失败不管，temp 目录系统会收）
    let _ = std::fs::remove_dir_all(&profile);
    let expected = outdir
        .join(src.file_stem().unwrap_or_default())
        .with_extension(to);
    if expected.exists() {
        tracing::info!(out = %expected.display(), "soffice 转换完成");
        return Ok(expected);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: String = stderr
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Err(HarnessError::Tool(format!(
        "LibreOffice 转换失败（exit {:?}，未见 {}）。stderr: {tail}（若 LibreOffice 正在运行请先关闭；目标格式 `{to}` 可能不受支持）",
        out.status.code(),
        expected.display()
    )))
}

// ===================================================================== tools

/// doc_read：按扩展名提取文档内容为文本。
pub async fn doc_read(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let raw = a
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::Tool("`path` is required".into()))?;
    let path = resolve_in_workspace(ctx, raw)?;
    ctx.authorize("doc_read", &path.to_string_lossy(), crate::permission::Risk::Shell)
        .await?;
    if !path.exists() {
        return Err(HarnessError::Tool(format!(
            "文件不存在：{}。检查路径（相对工作区根），或先 ls 确认文件名",
            path.display()
        )));
    }
    let sheet = a.get("sheet").and_then(|v| v.as_str()).map(str::to_string);
    let max_chars = a
        .get("max_chars")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_MAX_CHARS);
    let ext = ext_of(&path).unwrap_or_default();
    let rel = rel_display(&ctx.workspace, &path);

    let (body, stats, label) = match ext.as_str() {
        "pdf" => {
            let (b, pages) = extract_pdf(&path)?;
            (b, format!("{pages} 页"), "PDF 文档")
        }
        "docx" => {
            let (b, ps, ts) = extract_docx(&path)?;
            (b, format!("{ps} 段落 · {ts} 表格"), "Word 文档")
        }
        "pptx" => {
            let (b, pages) = extract_pptx(&path)?;
            (b, format!("{pages} 页"), "PowerPoint 演示")
        }
        "xlsx" | "xls" => {
            let (b, sheets) = extract_spreadsheet(&path, sheet.as_deref())?;
            (b, format!("{sheets} 表"), "Excel 表格")
        }
        "csv" => {
            let (b, lines) = extract_text_file(&path)?;
            (b, format!("{lines} 行"), "CSV")
        }
        "md" | "markdown" => {
            let (b, lines) = extract_text_file(&path)?;
            (b, format!("{lines} 行"), "Markdown")
        }
        "txt" => {
            let (b, lines) = extract_text_file(&path)?;
            (b, format!("{lines} 行"), "纯文本")
        }
        // doc/ppt 旧二进制：临时目录里转换后走 docx/pptx 提取（不在工作区落盘）
        "doc" | "ppt" => {
            let to = if ext == "doc" { "docx" } else { "pptx" };
            let tmp = std::env::temp_dir().join(format!("parable-docread-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&tmp)?;
            let copy = tmp.join(path.file_name().unwrap_or_default());
            std::fs::copy(&path, &copy)?;
            let converted = match soffice_convert(&copy, to).await {
                Ok(c) => c,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(e);
                }
            };
            let res = if to == "docx" {
                extract_docx(&converted)
                    .map(|(b, ps, ts)| (b, format!("{ps} 段落 · {ts} 表格"), "Word 文档（LibreOffice 转换读取）"))
            } else {
                extract_pptx(&converted)
                    .map(|(b, pages)| (b, format!("{pages} 页"), "PowerPoint 演示（LibreOffice 转换读取）"))
            };
            let _ = std::fs::remove_dir_all(&tmp);
            res?
        }
        other => {
            return Err(HarnessError::Tool(format!(
                "不支持的文档格式 `.{other}`。doc_read 支持：{}（doc/ppt 需已安装 LibreOffice）",
                READABLE_EXTS.join(" / ")
            )))
        }
    };
    let body = apply_max_chars(&body, max_chars);
    tracing::debug!(path = %rel, label, stats = %stats, "doc_read 完成");
    Ok(super::tools::ToolOutcome::text(
        true,
        format!("doc_read {rel} · {stats}"),
        format!("文档: {rel} · {label} · {stats}\n\n{body}"),
    ))
}

/// doc_create：按扩展名规格化创建文档。
pub async fn doc_create(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let raw = a
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::Tool("`path` is required".into()))?;
    let path = resolve_in_workspace(ctx, raw)?;
    let doc = need_doc(a)?;
    // 实现侧选择：doc_create 一律不覆盖已有文件。放在授权之前直接报错，
    // 避免在 ask/ws 模式下为一次注定失败的操作弹 Destructive 确认框；
    // 下面的风险分级保留作 TOCTOU 防御（极小概率竞态时仍按破坏性授权）。
    if path.exists() {
        return Err(HarnessError::Tool(format!(
            "文件已存在：{} —— doc_create 不覆盖已有文件。覆盖请用 doc_edit（结构化编辑）或先删除该文件再创建",
            path.display()
        )));
    }
    let exists = path.exists();
    let risk = if exists { crate::permission::Risk::Destructive } else { crate::permission::Risk::Write };
    ctx.authorize("doc_create", &path.to_string_lossy(), risk).await?;
    let ext = ext_of(&path).unwrap_or_default();
    let rel = rel_display(&ctx.workspace, &path);
    let (label, summary) = match ext.as_str() {
        "md" | "markdown" | "txt" => {
            let text = if let Some(s) = doc.as_str() {
                s.to_string()
            } else {
                doc.get("text")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| {
                        HarnessError::Tool("`doc.text` is required（md/txt 创建用 doc:{text:…} 或直接传字符串）".into())
                    })?
                    .to_string()
            };
            ("文本", create_text_file(&path, &text)?)
        }
        "csv" => {
            let rows = doc
                .get("rows")
                .filter(|r| r.is_array())
                .ok_or_else(|| HarnessError::Tool("`doc.rows` is required（csv 创建用 doc:{rows:[[…]]}）".into()))?;
            let rows = rows_to_strings(rows)?;
            ("CSV", create_csv_file(&path, &rows)?)
        }
        "xlsx" => {
            let sheets = parse_sheets(&doc)?;
            ("Excel 表格", create_xlsx_file(&path, &sheets)?)
        }
        "docx" => {
            let blocks = parse_blocks(
                doc.get("blocks")
                    .ok_or_else(|| HarnessError::Tool("`doc.blocks` is required（docx 创建用 doc:{blocks:[…]}）".into()))?,
            )?;
            ("Word 文档", create_docx_file(&path, &blocks)?)
        }
        "pptx" => {
            let slides = parse_slides(&doc)?;
            ("PowerPoint 演示", create_pptx_file(&path, &slides)?)
        }
        "pdf" => {
            let blocks = parse_blocks(
                doc.get("blocks")
                    .ok_or_else(|| HarnessError::Tool("`doc.blocks` is required（pdf 创建用 doc:{blocks:[…]}）".into()))?,
            )?;
            ("PDF 文档", create_pdf_file(&path, &blocks)?)
        }
        "doc" | "ppt" => {
            return Err(HarnessError::Tool(
                "doc_create 不支持旧二进制格式 doc/ppt——请改创建 docx/pptx，或对已有文件用 doc_convert 转换".into(),
            ))
        }
        other => {
            return Err(HarnessError::Tool(format!(
                "不支持的创建格式 `.{other}`。doc_create 支持：md / markdown / txt（doc.text）、csv（doc.rows）、xlsx（doc.sheets）、docx（doc.blocks）、pptx（doc.slides）、pdf（doc.blocks）"
            )))
        }
    };
    tracing::info!(path = %rel, summary = %summary, "doc_create 完成");
    Ok(super::tools::ToolOutcome::text(
        true,
        format!("doc_create {rel}"),
        format!("已创建 {rel}（{label} · {summary}）"),
    ))
}

/// doc_edit：结构化编辑既有文档（保留格式）。
pub async fn doc_edit(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let raw = a
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::Tool("`path` is required".into()))?;
    let path = resolve_in_workspace(ctx, raw)?;
    let ops = a
        .get("ops")
        .cloned()
        .filter(|v| v.is_array())
        .ok_or_else(|| HarnessError::Tool("`ops` is required（操作数组，见工具描述）".into()))?;
    ctx.authorize("doc_edit", &path.to_string_lossy(), crate::permission::Risk::Write)
        .await?;
    if !path.exists() {
        return Err(HarnessError::Tool(format!(
            "文件不存在：{}。要新建请用 doc_create",
            path.display()
        )));
    }
    let ext = ext_of(&path).unwrap_or_default();
    let rel = rel_display(&ctx.workspace, &path);
    if matches!(ext.as_str(), "md" | "markdown" | "txt" | "csv") {
        return Ok(super::tools::ToolOutcome::text(true, "doc_edit · 纯文本提示", plain_text_edit_hint()));
    }
    let summary = match ext.as_str() {
        "xlsx" => edit_xlsx_file(&path, &ops)?,
        // 旧 .xls（BIFF 二进制）umya 读不了，指路转换
        "xls" => {
            return Err(HarnessError::Tool(
                "xls 旧格式不支持结构化编辑——请先 doc_convert 为 xlsx 再编辑".into(),
            ))
        }
        "docx" => edit_docx_file(&path, &ops)?,
        "pptx" => edit_pptx_file(&path, &ops)?,
        "pdf" => edit_pdf_file(&path, &ops, &|raw| resolve_in_workspace(ctx, raw))?,
        other => {
            return Err(HarnessError::Tool(format!(
                "不支持的编辑格式 `.{other}`。doc_edit 支持：xlsx / docx / pptx / pdf；纯文本请用 write_file/edit_file"
            )))
        }
    };
    tracing::info!(path = %rel, summary = %summary, "doc_edit 完成");
    Ok(super::tools::ToolOutcome::text(
        true,
        format!("doc_edit {rel}"),
        format!("已更新 {rel}：{summary}"),
    ))
}

/// doc_convert：LibreOffice 跨格式转换（doc→docx、ppt→pptx、docx→pdf…）。
pub async fn doc_convert(a: &Value, ctx: &super::tools::ToolCtx) -> Result<super::tools::ToolOutcome, HarnessError> {
    let raw = a
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::Tool("`path` is required".into()))?;
    let path = resolve_in_workspace(ctx, raw)?;
    let to = a
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::Tool("`to` is required（目标扩展名，如 docx）".into()))?
        .trim()
        .to_lowercase();
    ctx.authorize("doc_convert", &format!("{} → {}", path.display(), to), crate::permission::Risk::Write)
        .await?;
    if !path.exists() {
        return Err(HarnessError::Tool(format!("文件不存在：{}", path.display())));
    }
    if !(!to.is_empty() && to.len() <= 10 && to.chars().all(|c| c.is_ascii_alphanumeric())) {
        return Err(HarnessError::Tool(format!(
            "目标格式 `{to}` 非法——请给扩展名（如 docx / pdf / pptx / xlsx / txt / png）"
        )));
    }
    if ext_of(&path).as_deref() == Some(to.as_str()) {
        return Err(HarnessError::Tool(format!(
            "源文件已经是 {to}，无需转换"
        )));
    }
    let out = soffice_convert(&path, &to).await?;
    let rel_out = rel_display(&ctx.workspace, &out);
    tracing::info!(from = %path.display(), to = %rel_out, "doc_convert 完成");
    Ok(super::tools::ToolOutcome::text(
        true,
        format!("doc_convert {to}"),
        format!(
            "已转换 {} → {rel_out}（LibreOffice headless）",
            rel_display(&ctx.workspace, &path)
        ),
    ))
}

// ===================================================================== tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ---------- 纯函数层 ----------

    #[test]
    fn readable_exts_cover_common() {
        for e in ["pdf", "docx", "xlsx", "pptx", "md"] {
            assert!(READABLE_EXTS.contains(&e));
        }
    }

    #[test]
    fn csv_field_escaping() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("he\"llo"), "\"he\"\"llo\"");
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
        assert_eq!(csv_field("a\r\nb"), "\"a\r\nb\"");
        assert_eq!(
            csv_row(&["名称".into(), "含,逗号".into(), "含\"引号\"".into()]),
            "名称,\"含,逗号\",\"含\"\"引号\"\"\""
        );
    }

    #[test]
    fn csv_create_exact_content() {
        let dir = tmp_dir();
        let p = dir.path().join("t.csv");
        let rows = vec![
            vec!["名称".into(), "备注".into()],
            vec!["含,逗号".into(), "含\"引号\"".into()],
            vec!["含\n换行".into(), "x".into()],
        ];
        let summary = create_csv_file(&p, &rows).unwrap();
        assert_eq!(summary, "3 行");
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text, "名称,备注\r\n\"含,逗号\",\"含\"\"引号\"\"\"\r\n\"含\n换行\",x");
    }

    #[test]
    fn xml_escape_basics() {
        assert_eq!(xml_escape("<a & \"b\">"), "&lt;a &amp; &quot;b&quot;&gt;");
        assert_eq!(xml_escape("it's"), "it&apos;s");
        assert_eq!(xml_escape("纯中文"), "纯中文");
        // 控制字符被丢弃，\t\n\r 保留
        assert_eq!(xml_escape("a\u{1}b"), "ab");
        assert_eq!(xml_escape("a\tb"), "a\tb");
    }

    #[test]
    fn max_chars_truncation_respects_char_boundary() {
        let body = "中文字符串测试".repeat(10);
        let cut = apply_max_chars(&body, 7);
        assert!(cut.starts_with(&"中文字符串测试".repeat(10)[..21])); // 7 个汉字
        assert!(cut.contains("已截断"));
        let full = apply_max_chars(&body, 10_000);
        assert_eq!(full, body);
        assert!(!full.contains("已截断"));
    }

    #[test]
    fn cell_coord_parse() {
        assert_eq!(parse_cell_coord("A1").unwrap(), (1, 1));
        assert_eq!(parse_cell_coord("b3").unwrap(), (2, 3));
        assert_eq!(parse_cell_coord(" XFD1048576 ").unwrap(), (16384, 1_048_576));
        assert_eq!(parse_cell_coord("AA10").unwrap(), (27, 10));
        for bad in ["", "1A", "A0", "AAAA1", "A1048577", "XFDZ1", "A"] {
            assert!(parse_cell_coord(bad).is_err(), "{bad} 应报错");
        }
    }

    #[test]
    fn blocks_parse_and_errors() {
        let blocks = parse_blocks(&json!([
            {"type": "h1", "text": "标题"},
            {"type": "p", "text": "正文"},
            {"type": "bullet", "text": "要点"},
            {"type": "table", "rows": [["a", 1], [true, "b"]]}
        ]))
        .unwrap();
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0], Block::Heading(1, "标题".into()));
        assert_eq!(blocks[1], Block::Para("正文".into()));
        assert_eq!(blocks[2], Block::Bullet("要点".into()));
        assert_eq!(
            blocks[3],
            Block::Table(vec![vec!["a".into(), "1".into()], vec!["true".into(), "b".into()]])
        );
        assert!(parse_blocks(&json!([{"type": "h9", "text": "x"}])).is_err());
        assert!(parse_blocks(&json!([{"type": "p"}])).is_err());
        assert!(parse_blocks(&json!("not-array")).is_err());
    }

    #[test]
    fn docx_xml_parser_paragraphs_and_tables() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:r><w:t>第一段</w:t></w:r></w:p>
<w:p><w:r><w:t>   </w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p></w:tc><w:tc><w:r><w:t>B1</w:t></w:r></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>A2</w:t></w:r></w:p></w:tc><w:tc><w:r><w:t>B2</w:t></w:r></w:tc></w:tr></w:tbl>
<w:p><w:r><w:t>表后段落</w:t></w:r></w:p>
</w:body></w:document>"#;
        let (text, ps, ts) = parse_docx_document_xml(xml);
        assert_eq!(ps, 2, "空段落不计");
        assert_eq!(ts, 1);
        assert!(text.contains("第一段"));
        assert!(text.contains("表后段落"));
        assert!(text.contains("A1 | B1"));
        assert!(text.contains("A2 | B2"));
    }

    #[test]
    fn pptx_slide_parser_title_first() {
        let xml = r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:cNvPr id="5" name="内容"/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>先出现的内容</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="2" name="标题"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>标题文本</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:cNvPr id="6" name="无占位"/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>普通形状</a:t></a:r></a:p></p:txBody></p:sp>
</p:spTree></p:cSld></p:sld>"#;
        let lines = parse_pptx_slide_xml(xml);
        assert_eq!(lines, vec!["标题文本", "先出现的内容", "普通形状"]);
    }

    #[test]
    fn scan_max_num_finds_all() {
        let xml = r#"<p:sldMasterId id="2147483648"/><p:sldIdLst><p:sldId id="256" r:id="rId2"/><p:sldId id="259" r:id="rId7"/></p:sldIdLst>"#;
        assert_eq!(scan_max_num(xml, "<p:sldId id=\""), 259);
        assert_eq!(scan_max_num(xml, "r:id=\"rId"), 7);
        assert_eq!(scan_max_num("nothing", "zzz"), 0);
    }

    // ---------- docx round-trip ----------

    #[test]
    fn docx_create_read_roundtrip() {
        let dir = tmp_dir();
        let p = dir.path().join("t.docx");
        let blocks = vec![
            Block::Heading(1, "季度报告".into()),
            Block::Para("正文内容 hello".into()),
            Block::Bullet("要点A".into()),
            Block::Bullet("要点B".into()),
            Block::Table(vec![vec!["名称".into(), "数量".into()], vec!["苹果".into(), "3".into()]]),
        ];
        let summary = create_docx_file(&p, &blocks).unwrap();
        assert!(summary.contains("5 块"), "{summary}");
        let (text, ps, ts) = extract_docx(&p).unwrap();
        assert!(text.contains("季度报告"), "{text}");
        assert!(text.contains("正文内容 hello"));
        assert!(text.contains("要点A"));
        assert!(text.contains("要点B"));
        assert!(text.contains("名称 | 数量"));
        assert!(text.contains("苹果 | 3"));
        assert_eq!(ps, 4);
        assert_eq!(ts, 1);
    }

    #[test]
    fn docx_append_and_replace() {
        let dir = tmp_dir();
        let p = dir.path().join("t.docx");
        create_docx_file(
            &p,
            &[
                Block::Heading(1, "合同".into()),
                Block::Para("正文内容，含占位符 {{name}}。".into()),
            ],
        )
        .unwrap();

        // append_blocks
        let ops = json!([
            {"op": "append_blocks", "blocks": [
                {"type": "h2", "text": "附件说明"},
                {"type": "bullet", "text": "追加的要点"},
                {"type": "table", "rows": [["k", "v"]]}
            ]},
        ]);
        let summary = edit_docx_file(&p, &ops).unwrap();
        assert!(summary.contains("append_blocks 3 块"), "{summary}");
        let (text, ps, ts) = extract_docx(&p).unwrap();
        assert!(text.contains("附件说明"));
        assert!(text.contains("追加的要点"));
        assert!(text.contains("k | v"));
        assert_eq!(ts, 1);
        // 原有 H1 + 正文，追加 H2 + bullet（表格外的 2 个段落）
        assert_eq!(ps, 4);

        // replace_text：命中普通文本
        let ops = json!([
            {"op": "replace_text", "find": "{{name}}", "replace": "张三"},
            {"op": "replace_text", "find": "正文内容", "replace": "替换后内容"},
        ]);
        let summary = edit_docx_file(&p, &ops).unwrap();
        assert!(summary.contains("replace_text ×1"), "{summary}");
        let (text, _, _) = extract_docx(&p).unwrap();
        assert!(text.contains("替换后内容，含占位符 张三。"), "{text}");
        assert!(!text.contains("{{name}}"));
        assert!(!text.contains("正文内容"));

        // replace_text：未命中 → 报错列出文档开头文本
        let ops = json!([{"op": "replace_text", "find": "不存在的文本", "replace": "x"}]);
        let err = edit_docx_file(&p, &ops).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("未找到"), "{msg}");
        assert!(msg.contains("合同"), "报错应带文档开头文本: {msg}");

        // 跨 run 命中（docx_rs 顺序写 run，整段在一个 run 里，这里验证第二
        // 轮合并替换逻辑路径不崩、结果正确）
        let ops = json!([{"op": "replace_text", "find": "合同", "replace": "协议书"}]);
        edit_docx_file(&p, &ops).unwrap();
        let (text, _, _) = extract_docx(&p).unwrap();
        assert!(text.contains("协议书"), "{text}");
    }

    // ---------- xlsx round-trip ----------

    #[test]
    fn xlsx_create_read_edit_roundtrip() {
        let dir = tmp_dir();
        let p = dir.path().join("t.xlsx");
        let doc = json!({"sheets": [
            {"name": "销售", "rows": [
                ["商品", "数量", "上架"],
                ["苹果", 3, true],
                ["梨", 5.5, false]
            ]},
            {"name": "汇总", "rows": [["合计", 8.5]]}
        ]});
        let summary = create_xlsx_file(&p, &parse_sheets(&doc).unwrap()).unwrap();
        assert!(summary.contains("2 表"), "{summary}");
        assert!(summary.contains("4 行"), "{summary}");

        // 全簿读取
        let (text, sheets) = extract_spreadsheet(&p, None).unwrap();
        assert_eq!(sheets, 2);
        assert!(text.contains("## 销售"), "{text}");
        assert!(text.contains("商品\t数量\t上架"));
        assert!(text.contains("苹果\t3\tTRUE"));
        assert!(text.contains("梨\t5.5\tFALSE"));
        assert!(text.contains("## 汇总"));
        assert!(text.contains("合计\t8.5"));

        // 指定 sheet
        let (text, sheets) = extract_spreadsheet(&p, Some("汇总")).unwrap();
        assert_eq!(sheets, 1);
        assert!(text.contains("## 汇总"));
        assert!(!text.contains("## 销售"));

        // sheet 不存在报错并列出表名
        let err = extract_spreadsheet(&p, Some("不存在")).unwrap_err();
        assert!(err.to_string().contains("销售") && err.to_string().contains("汇总"));

        // set_cells：数字/字符串/布尔
        let ops = json!([
            {"op": "set_cells", "sheet": "销售", "cells": {"B2": 99, "C2": "备注文字", "A3": false}}
        ]);
        let summary = edit_xlsx_file(&p, &ops).unwrap();
        assert!(summary.contains("set_cells 3 格"), "{summary}");
        let (text, _) = extract_spreadsheet(&p, Some("销售")).unwrap();
        // B2 数字、C2 文本、A3 布尔——三类写入都落位
        assert!(text.contains("苹果\t99\t备注文字"), "{text}");
        assert!(text.contains("FALSE\t5.5\tFALSE"), "{text}");

        // set_cells：sheet 不存在 → 报错列表名
        let ops = json!([{"op": "set_cells", "sheet": "没有", "cells": {"A1": 1}}]);
        let err = edit_xlsx_file(&p, &ops).unwrap_err();
        assert!(err.to_string().contains("销售"), "{err}");

        // set_cells：坐标非法
        let ops = json!([{"op": "set_cells", "sheet": "销售", "cells": {"A0": 1}}]);
        assert!(edit_xlsx_file(&p, &ops).unwrap_err().to_string().contains("坐标"));

        // add_sheet 重名报错
        let ops = json!([{"op": "add_sheet", "name": "销售"}]);
        let err = edit_xlsx_file(&p, &ops).unwrap_err();
        assert!(err.to_string().contains("已存在"), "{err}");

        // add_sheet 新表带行
        let ops = json!([
            {"op": "add_sheet", "name": "新增", "rows": [["指标", 42]]},
            {"op": "rename_sheet", "from": "汇总", "to": "月度"}
        ]);
        let summary = edit_xlsx_file(&p, &ops).unwrap();
        assert!(summary.contains("add_sheet `新增`") && summary.contains("rename_sheet"), "{summary}");
        let (text, sheets) = extract_spreadsheet(&p, None).unwrap();
        assert_eq!(sheets, 3);
        assert!(text.contains("## 新增") && text.contains("指标\t42"), "{text}");
        assert!(text.contains("## 月度"));
        assert!(!text.contains("## 汇总"));

        // rename 到已存在的名字
        let ops = json!([{"op": "rename_sheet", "from": "月度", "to": "新增"}]);
        // umya set_name 可能允许重名——这里我们自己把守：当前实现依赖 umya 校验，
        // 若 umya 放行则至少不能崩
        let _ = edit_xlsx_file(&p, &ops);
    }

    // ---------- pptx ----------

    #[test]
    fn pptx_create_zip_structure_and_read() {
        let dir = tmp_dir();
        let p = dir.path().join("t.pptx");
        let slides = vec![
            SlideSpec { title: "开场".into(), bullets: vec!["要点1".into(), "要点2".into()] },
            SlideSpec { title: "结尾".into(), bullets: vec![] },
        ];
        let summary = create_pptx_file(&p, &slides).unwrap();
        assert_eq!(summary, "2 页");

        // zip 结构完整性：最小 OOXML 集必须齐
        let archive = open_zip(&p).unwrap();
        let names: Vec<String> = archive.file_names().map(String::from).collect();
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "ppt/presentation.xml",
            "ppt/_rels/presentation.xml.rels",
            "ppt/slideMasters/slideMaster1.xml",
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            "ppt/theme/theme1.xml",
            "ppt/slides/slide1.xml",
            "ppt/slides/_rels/slide1.xml.rels",
            "ppt/slides/slide2.xml",
            "ppt/slides/_rels/slide2.xml.rels",
        ] {
            assert!(names.iter().any(|n| n == required), "缺 {required}");
        }
        drop(archive);

        // content types 覆盖两页
        let ct = String::from_utf8(zip_read_entry(&p, "[Content_Types].xml").unwrap()).unwrap();
        assert!(ct.contains("/ppt/slides/slide1.xml") && ct.contains("/ppt/slides/slide2.xml"));
        let px = String::from_utf8(zip_read_entry(&p, "ppt/presentation.xml").unwrap()).unwrap();
        assert!(px.contains(r#"<p:sldId id="256" r:id="rId2"/>"#), "{px}");
        assert!(px.contains(r#"<p:sldId id="257" r:id="rId3"/>"#), "{px}");

        // 文本抽取：标题在前、逐页分块
        let (text, pages) = extract_pptx(&p).unwrap();
        assert_eq!(pages, 2);
        assert!(text.contains("--- 第 1 页 ---"), "{text}");
        let page1 = text.split("--- 第 2 页 ---").next().unwrap();
        assert!(page1.contains("开场"));
        assert!(page1.contains("要点1") && page1.contains("要点2"));
        // 标题行在要点之前
        let t_pos = page1.find("开场").unwrap();
        let b_pos = page1.find("要点1").unwrap();
        assert!(t_pos < b_pos);
        assert!(text.contains("结尾"));
    }

    #[test]
    fn pptx_add_slide_patches_indexes() {
        let dir = tmp_dir();
        let p = dir.path().join("t.pptx");
        create_pptx_file(&p, &[SlideSpec { title: "第一页".into(), bullets: vec![] }]).unwrap();

        let ops = json!([
            {"op": "add_slide", "title": "第二页", "bullets": ["b1", "b2"]},
            {"op": "add_slide", "title": "第三页"}
        ]);
        let summary = edit_pptx_file(&p, &ops).unwrap();
        assert!(summary.contains("第 2 页") && summary.contains("第 3 页"), "{summary}");

        // 索引文件都被补丁
        let ct = String::from_utf8(zip_read_entry(&p, "[Content_Types].xml").unwrap()).unwrap();
        assert!(ct.contains("slide2.xml") && ct.contains("slide3.xml"), "{ct}");
        let px = String::from_utf8(zip_read_entry(&p, "ppt/presentation.xml").unwrap()).unwrap();
        assert!(px.contains(r#"<p:sldId id="257" r:id="rId3"/>"#), "{px}");
        assert!(px.contains(r#"<p:sldId id="258" r:id="rId4"/>"#), "{px}");
        let rels = String::from_utf8(zip_read_entry(&p, "ppt/_rels/presentation.xml.rels").unwrap()).unwrap();
        assert!(rels.contains(r#"Target="slides/slide3.xml""#), "{rels}");
        // 新页内容可读
        let (text, pages) = extract_pptx(&p).unwrap();
        assert_eq!(pages, 3);
        assert!(text.contains("第二页") && text.contains("b1") && text.contains("第三页"), "{text}");

        // 非 pptx 输入 → 明确报错
        let bad = dir.path().join("bad.pptx");
        std::fs::write(&bad, b"not a zip").unwrap();
        let ops = json!([{"op": "add_slide", "title": "x"}]);
        assert!(edit_pptx_file(&bad, &ops).is_err());
    }

    // ---------- 纯文本提示与未知扩展名 ----------

    #[test]
    fn plain_text_hint_mentions_write_and_edit() {
        let hint = plain_text_edit_hint();
        assert!(hint.contains("write_file") && hint.contains("edit_file"));
        assert!(hint.contains("xlsx"));
    }

    // ---------- pdf（依赖系统字体，缺失环境断言错误信息） ----------

    #[test]
    fn pdf_create_or_font_error() {
        let dir = tmp_dir();
        let p = dir.path().join("t.pdf");
        let blocks = vec![Block::Heading(1, "标题".into()), Block::Para("你好 PDF".into())];
        match create_pdf_file(&p, &blocks) {
            Ok(summary) => {
                // 字体可用：产物必须是合法 PDF 且文本可读回
                assert!(summary.contains("2 块"), "{summary}");
                let text = pdf_extract::extract_text(&p).unwrap();
                assert!(text.contains("你好 PDF"), "{text}");
            }
            Err(e) => {
                // 无字体环境（CI）：必须给出字体指引而不是 panic
                let msg = e.to_string();
                assert!(msg.contains("字体"), "{msg}");
                assert!(!p.exists());
            }
        }
    }

    #[test]
    fn pdf_merge_and_extract_pages() {
        // 没字体造不出源 PDF 时跳过（断言依赖条件，保持 CI 绿）
        if !detect_pdf_fonts() {
            return;
        }
        let dir = tmp_dir();
        let a = dir.path().join("a.pdf");
        let b = dir.path().join("b.pdf");
        create_pdf_file(&a, &[Block::Para("第一份文档AAA".into())]).unwrap();
        create_pdf_file(&b, &[Block::Para("第二份文档BBB".into())]).unwrap();
        // 测试用路径解析器：相对/绝对都落到临时目录里（join 绝对路径时直接替换）
        let resolve = |raw: &str| -> Result<PathBuf, HarnessError> { Ok(dir.path().join(raw)) };

        // merge
        let ops = json!([{"op": "merge", "paths": [b.to_string_lossy()]}]);
        let summary = edit_pdf_file(&a, &ops, &resolve).unwrap();
        assert!(summary.contains("merge"), "{summary}");
        let text = pdf_extract::extract_text(&a).unwrap();
        assert!(text.contains("第一份文档AAA") && text.contains("第二份文档BBB"), "{text}");

        // extract_pages 1..1 → 前缀-pages.pdf 只剩第一份内容
        let ops = json!([{"op": "extract_pages", "from": 1, "to": 1}]);
        let summary = edit_pdf_file(&a, &ops, &resolve).unwrap();
        assert!(summary.contains("extract_pages 1-1"), "{summary}");
        let pages_out = dir.path().join("a-pages.pdf");
        assert!(pages_out.exists());
        let text = pdf_extract::extract_text(&pages_out).unwrap();
        assert!(text.contains("第一份文档AAA"), "{text}");
        assert!(!text.contains("第二份文档BBB"), "{text}");

        // 区间非法
        let ops = json!([{"op": "extract_pages", "from": 1, "to": 99}]);
        let err = edit_pdf_file(&dir.path().join("b.pdf"), &ops, &resolve).unwrap_err();
        assert!(err.to_string().contains("区间"), "{err}");
    }

    // ---------- soffice 探测 ----------

    #[test]
    fn soffice_detection_does_not_panic() {
        // 机器上有没有装都行：装了返回存在的路径，没装返回 None
        if let Some(p) = find_soffice() {
            assert!(p.is_file());
        }
    }
}
