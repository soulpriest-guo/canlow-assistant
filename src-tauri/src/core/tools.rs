// 工具执行层：文件/搜索/Git/命令
// execute_tool() 供 agent 循环调用（返回给 AI 的文本）；Tauri 命令供前端调用
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::db::Db;
use super::safety::check_dangerous_command;
use super::taskmap::{plan_dispatch, plan_tool_definitions, TaskMapStore};

/// 单次读文件上限：从 50 万字符降到 8 万（超大内容分 segment 读取；
/// 结果若超过 3 万字符还会被 spill 落盘，历史里只留预览，避免每轮重复付费）
pub const MAX_READ_CHARS: usize = 80_000;
pub const MAX_CMD_OUTPUT: usize = 240_000;

// ---------- 异步命令任务表 ----------

#[derive(Clone, Default)]
pub struct CmdStatus {
    pub stdout: String,
    pub stderr: String,
    pub running: bool,
    pub code: Option<i32>,
}

pub struct CmdRegistry {
    pub statuses: Mutex<HashMap<String, Arc<Mutex<CmdStatus>>>>,
    pub pids: Mutex<HashMap<String, u32>>,
}

impl Default for CmdRegistry {
    fn default() -> Self {
        Self {
            statuses: Mutex::new(HashMap::new()),
            pids: Mutex::new(HashMap::new()),
        }
    }
}

// ---------- 路径安全 ----------

/// 把相对路径解析到 base_dir 下，并防止越出 base_dir（realpath 防符号链接逃逸）
pub fn safe_path(base_dir: &str, p: &str) -> Result<PathBuf, String> {
    let base = std::path::absolute(base_dir).map_err(|e| e.to_string())?;
    let raw = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        base.join(p)
    };
    let abs = std::path::absolute(&raw).map_err(|e| e.to_string())?;
    let real_base = base.canonicalize().map_err(|e| format!("工作目录不存在: {e}"))?;

    // 目标本身等于 base（"." / "" / 显式 base 路径）：直接放行。
    // ★ 之前用 abs.parent() 做校验，p 为 "." 时 abs 规范化后 == base，
    //   parent 是 base 的父目录 → 必然不 starts_with(base) → 误报"路径越界"。
    if abs == base {
        return Ok(abs);
    }

    // 校验点：目标存在时校验目标本身；目标不存在时（新建文件/深层目录）
    // 向上找最近存在的祖先目录校验。
    let mut check: &Path = abs.as_path();
    while !check.exists() {
        match check.parent() {
            Some(pp) => check = pp,
            None => break,
        }
    }
    let real_check = check
        .canonicalize()
        .map_err(|e| format!("路径不存在: {e}"))?;
    if !real_check.starts_with(&real_base) {
        return Err(format!("路径越界被拒绝: {p}"));
    }

    // 目标本身是符号链接：无论悬空与否都解析真实目标校验（防逃逸）。
    // 悬空链接保守拒绝——写入该链接会创建 target 文件，可能落在工作区外。
    if let Ok(meta) = std::fs::symlink_metadata(&abs) {
        if meta.file_type().is_symlink() {
            match abs.canonicalize() {
                Ok(real) => {
                    if !real.starts_with(&real_base) {
                        return Err(format!("路径越界被拒绝（符号链接指向工作区外）: {p}"));
                    }
                }
                Err(_) => {
                    return Err(format!("路径越界被拒绝（符号链接无法解析）: {p}"));
                }
            }
        }
    }
    Ok(abs)
}

fn read_text(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_READ_CHARS as u64 {
        return Err(format!(
            "文件过大 ({} 字节)，请使用 read_file_segment",
            meta.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

// ---------- 文件工具 ----------

fn tool_list_dir(base: &str, args: &Value) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let dir = safe_path(base, path)?;
    let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    for e in entries.flatten() {
        let is_dir = e.path().is_dir();
        lines.push(format!(
            "{}{}  {}",
            if is_dir { "[D]" } else { "[F]" },
            e.file_name().to_string_lossy(),
            e.path().to_string_lossy()
        ));
    }
    Ok(if lines.is_empty() {
        "（空目录）".to_string()
    } else {
        lines.join("\n")
    })
}

fn tool_read_file(base: &str, args: &Value) -> Result<String, String> {
    // 支持单文件 path，或一次读多个文件 paths（大项目高效读取）
    let mut paths: Vec<String> = Vec::new();
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        paths.extend(arr.iter().filter_map(|v| v.as_str()).map(String::from));
    } else if let Some(arr) = args.get("path").and_then(|v| v.as_array()) {
        paths.extend(arr.iter().filter_map(|v| v.as_str()).map(String::from));
    } else if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        paths.push(p.to_string());
    } else {
        return Err("缺少 path（或 paths 数组）".into());
    }
    if paths.is_empty() {
        return Err("缺少 path（或 paths 数组）".into());
    }
    if paths.len() == 1 {
        let full = safe_path(base, &paths[0])?;
        return read_text(&full);
    }
    // 多文件：带分隔标题，总和限制 80k 字符（超出部分由 spill 落盘机制接管）
    const MAX_TOTAL: usize = 80_000;
    let mut out = String::new();
    let mut total = 0;
    for p in &paths {
        if total >= MAX_TOTAL {
            out.push_str(&format!("\n...（已达批量读取上限，其余文件未读: {}）", paths.len()));
            break;
        }
        let full = match safe_path(base, p) {
            Ok(f) => f,
            Err(e) => {
                out.push_str(&format!("\n===== {p} =====\n❌ {e}\n"));
                continue;
            }
        };
        let head = format!("\n===== {p} =====\n");
        out.push_str(&head);
        total += head.len();
        match read_text(&full) {
            Ok(text) => {
                let room = MAX_TOTAL.saturating_sub(total);
                let cut: String = text.chars().take(room).collect();
                out.push_str(&cut);
                total += cut.len();
                if cut.len() < text.len() {
                    out.push_str("\n...（该文件内容被截断）");
                }
            }
            Err(e) => {
                out.push_str(&format!("❌ {e}\n"));
            }
        }
    }
    Ok(out)
}

fn tool_read_file_segment(base: &str, args: &Value) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let full = safe_path(base, path)?;
    // 不走 read_text 的 500k 限制：直接读字节并做 UTF-8 无损转换（上限 64MB，防止超大文件撑爆内存）
    let meta = std::fs::metadata(&full).map_err(|e| e.to_string())?;
    const MAX_SEGMENT_FILE: u64 = 64 * 1024 * 1024;
    if meta.len() > MAX_SEGMENT_FILE {
        return Err(format!("文件过大 ({} 字节)，暂不支持分段读取", meta.len()));
    }
    let bytes = std::fs::read(&full).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let total_chars = text.chars().count();
    let segment_index = args.get("segment_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let chars_per_segment = args
        .get("chars_per_segment")
        .and_then(|v| v.as_u64())
        .unwrap_or(50_000) as usize;
    let chars_per_segment = chars_per_segment.max(1); // 防止除零
    let start = (segment_index as usize).saturating_mul(chars_per_segment);
    let seg: String = text.chars().skip(start).take(chars_per_segment).collect();
    let total_segments = total_chars.div_ceil(chars_per_segment).max(1);
    Ok(format!(
        "[分段 {}/{}] (共 {} 字符)\n{}",
        segment_index + 1,
        total_segments,
        total_chars,
        seg
    ))
}

fn tool_write_file(base: &str, args: &Value, db: &Db, conv_id: &str) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let content = args.get("content").and_then(|v| v.as_str()).ok_or("缺少 content")?;
    let full = safe_path(base, path)?;
    // 写前自动备份（支持 undo_file）
    if full.exists() {
        if let Ok(old) = std::fs::read_to_string(&full) {
            let _ = db.backup_add(conv_id, path, &old);
        }
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&full, content).map_err(|e| e.to_string())?;
    Ok(format!("已写入 {} ({} 字节)", path, content.len()))
}

fn tool_replace_in_file(base: &str, args: &Value, db: &Db, conv_id: &str) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let old = args.get("old_str").and_then(|v| v.as_str()).ok_or("缺少 old_str")?;
    let new = args.get("new_str").and_then(|v| v.as_str()).ok_or("缺少 new_str")?;
    let full = safe_path(base, path)?;
    let text = read_text(&full)?;
    let _ = db.backup_add(conv_id, path, &text);
    if !text.contains(old) {
        return Err(format!("old_str 未在文件中找到（{old:?}），请检查内容"));
    }
    let count = text.matches(old).count();
    let new_text = text.replace(old, new);
    std::fs::write(&full, new_text).map_err(|e| e.to_string())?;
    Ok(format!("已替换 {count} 处，文件: {path}"))
}

fn tool_rename_file(base: &str, args: &Value) -> Result<String, String> {
    let from = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let to = args.get("new_name").and_then(|v| v.as_str()).ok_or("缺少 new_name")?;
    let src = safe_path(base, from)?;
    let dst = safe_path(base, to)?;
    rename_fallback(&src, &dst)?;
    Ok(format!("已重命名 {from} → {to}"))
}

fn tool_copy_file(base: &str, args: &Value) -> Result<String, String> {
    let from = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let to = args.get("new_path").and_then(|v| v.as_str()).ok_or("缺少 new_path")?;
    let src = safe_path(base, from)?;
    let dst = safe_path(base, to)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
    Ok(format!("已复制 {from} → {to}"))
}

fn tool_move_file(base: &str, args: &Value) -> Result<String, String> {
    let from = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let to = args.get("new_path").and_then(|v| v.as_str()).ok_or("缺少 new_path")?;
    let src = safe_path(base, from)?;
    let dst = safe_path(base, to)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    rename_fallback(&src, &dst)?;
    Ok(format!("已移动 {from} → {to}"))
}

/// 跨文件系统（EXDEV）时 rename 会失败，回退为 复制+删除
fn rename_fallback(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            if src.is_dir() {
                copy_dir_recursive(src, dst)?;
                std::fs::remove_dir_all(src).map_err(|e| e.to_string())?;
            } else {
                std::fs::copy(src, dst).map_err(|e| e.to_string())?;
                std::fs::remove_file(src).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn tool_create_directory(base: &str, args: &Value) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let full = safe_path(base, path)?;
    std::fs::create_dir_all(&full).map_err(|e| e.to_string())?;
    Ok(format!("已创建目录: {path}"))
}

fn tool_get_file_info(base: &str, args: &Value) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let full = safe_path(base, path)?;
    let meta = std::fs::metadata(&full).map_err(|e| e.to_string())?;
    let kind = if meta.is_dir() { "目录" } else { "文件" };
    Ok(format!(
        "{kind}: {path}\n大小: {} 字节\n修改时间: {:?}",
        meta.len(),
        meta.modified()
    ))
}

fn tool_delete_file(base: &str, args: &Value) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
    let full = safe_path(base, path)?;
    if full.is_dir() {
        std::fs::remove_dir_all(&full).map_err(|e| e.to_string())?;
    } else {
        std::fs::remove_file(&full).map_err(|e| e.to_string())?;
    }
    Ok(format!("已删除: {path}"))
}

// ---------- 搜索工具 ----------

/// 搜索时默认跳过的噪音目录（生成物/依赖/版本控制，避免遍历几十万文件卡死）
const NOISE_DIRS: &[&str] = &[
    "node_modules", ".git", ".hg", ".svn", "dist", "build", "target", "out",
    ".next", ".nuxt", ".output", "coverage", "__pycache__", ".venv", "venv",
    ".idea", ".vscode", ".gradle", "Pods", ".tox", ".cache", ".pytest_cache",
    "DerivedData", ".turbo", ".pnpm-store", "vendor", "third_party",
];

/// 单次搜索遍历的文件/目录上限（防止超大目录把搜索拖到分钟级）
const MAX_WALK_FILES: usize = 30_000;



/// 递归遍历目录树：跳过噪音目录（除非遍历起点本身就是它——显式指定则尊重），
/// 带数量上限。返回 (路径列表, 是否截断)。
fn walk(base: &Path, max: usize) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut stack = vec![base.to_path_buf()];
    let root_noise = base
        .file_name()
        .map(|n| NOISE_DIRS.iter().any(|d| n == *d))
        .unwrap_or(false);
    while let Some(dir) = stack.pop() {
        if out.len() >= max {
            truncated = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if out.len() >= max {
                truncated = true;
                break;
            }
            // 用 DirEntry::file_type（readdir 自带 d_type，不再额外 stat）
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_dir() {
                if !root_noise {
                    let name = e.file_name();
                    if NOISE_DIRS.iter().any(|d| name == *d) {
                        continue;
                    }
                }
                stack.push(e.path());
            }
            out.push(e.path());
        }
    }
    (out, truncated)
}

/// 搜索路径集合：path 为文件时只返回该文件；为目录时递归遍历
/// （修复：grep/search/glob 的 path 传具体文件时无结果的问题）
fn search_paths(base: &Path) -> (Vec<PathBuf>, bool) {
    if base.is_file() {
        (vec![base.to_path_buf()], false)
    } else {
        walk(base, MAX_WALK_FILES)
    }
}

/// 遍历截断提示（附在搜索结果尾部）
fn walk_truncated_hint() -> String {
    format!("

⚠️ 目录过大，遍历已截断（仅扫描前 {MAX_WALK_FILES} 个文件/目录）；建议用 path 参数缩小范围")
}

fn tool_search_files(base: &str, args: &Value) -> Result<String, String> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or("缺少 pattern")?;
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let dir = safe_path(base, path)?;
    let max = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let pl = pattern.to_lowercase();
    let mut hits = Vec::new();
    let (paths, truncated) = search_paths(&dir);
    for p in &paths {
        if hits.len() >= max {
            break;
        }
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.to_lowercase().contains(&pl) {
            hits.push(p.to_string_lossy().to_string());
        }
    }
    let mut out = if hits.is_empty() {
        "未找到匹配文件".to_string()
    } else {
        hits.join("\n")
    };
    if truncated {
        out.push_str(&walk_truncated_hint());
    }
    Ok(out)
}

fn tool_grep_search(base: &str, args: &Value) -> Result<String, String> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or("缺少 pattern")?;
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let dir = safe_path(base, path)?;
    let max = args.get("max_matches").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let case_sensitive = args.get("case_sensitive").and_then(|v| v.as_bool()).unwrap_or(false);
    // ★ 支持 `|` 分隔多关键词（任一命中即匹配），如 "plan_init|plan_breakdown"
    let patterns: Vec<String> = pattern
        .split('|')
        .map(|s| {
            let s = s.trim();
            if case_sensitive { s.to_string() } else { s.to_lowercase() }
        })
        .filter(|s| !s.is_empty())
        .collect();
    let mut hits: Vec<String> = Vec::new();
    let (paths, truncated) = search_paths(&dir);
    // ★ 借鉴 rg --max-filesize 语义：超过阈值的大文件跳过，但统计并提示（默认 1MB，可调）
    let max_file_bytes = args
        .get("max_file_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .saturating_mul(1024 * 1024)
        .max(64 * 1024); // 最小 64KB，防止误设过小
    let mut skipped_count: usize = 0;
    let mut skipped_samples: Vec<(String, u64)> = Vec::new();
    for p in &paths {
        if hits.len() >= max {
            break;
        }
        // metadata 一次判断：目录跳过 / 大文件跳过（默认 >1MB 的内容搜索无意义且拖慢遍历）
        let Ok(meta) = p.metadata() else {
            continue;
        };
        if meta.is_dir() {
            continue;
        }
        if meta.len() > max_file_bytes {
            skipped_count += 1;
            if skipped_samples.len() < 5 {
                skipped_samples.push((p.display().to_string(), meta.len()));
            }
            continue;
        }
        // 直接读取（不走 read_text 的 80KB 限制），二进制文件 read_to_string 会失败自动跳过
        if let Ok(text) = std::fs::read_to_string(p) {
            for (i, line) in text.lines().enumerate() {
                if hits.len() >= max {
                    break;
                }
                let hay = if case_sensitive { line.to_string() } else { line.to_lowercase() };
                if patterns.iter().any(|pl| hay.contains(pl)) {
                    hits.push(format!("{}:{}: {}", p.display(), i + 1, line.trim().chars().take(200).collect::<String>()));
                }
            }
        }
    }
    let mut out = if hits.is_empty() {
        "未找到匹配行".to_string()
    } else {
        hits.join("\n")
    };
    if truncated {
        out.push_str(&walk_truncated_hint());
    }
    if skipped_count > 0 {
        let threshold_mb = max_file_bytes / (1024 * 1024);
        let samples: Vec<String> = skipped_samples
            .iter()
            .map(|(p, sz)| format!("{p}（{}MB）", sz / (1024 * 1024)))
            .collect();
        out.push_str(&format!(
            "\n\n⚠️ 已跳过 {skipped_count} 个大文件（> {threshold_mb}MB）：{}\n如需搜索大文件内容：用 max_file_size 调大阈值，或用 read_file_segment 分段读取后搜索。",
            samples.join("、")
        ));
    }
    Ok(out)
}

fn tool_glob_search(base: &str, args: &Value) -> Result<String, String> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or("缺少 pattern")?;
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let dir = safe_path(base, path)?;
    let max = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let mut hits = Vec::new();
    let (paths, truncated) = search_paths(&dir);
    for p in &paths {
        if hits.len() >= max {
            break;
        }
        let rel = p.strip_prefix(&dir).unwrap_or(p);
        if glob_match(pattern, &rel.to_string_lossy()) {
            hits.push(p.to_string_lossy().to_string());
        }
    }
    let mut out = if hits.is_empty() {
        "未找到匹配文件".to_string()
    } else {
        hits.join("\n")
    };
    if truncated {
        out.push_str(&walk_truncated_hint());
    }
    Ok(out)
}

/// 简易 glob（* 与 ** 支持）
fn glob_match(pattern: &str, text: &str) -> bool {
    fn match_here(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => {
                // ** 可跨目录
                if p.len() > 1 && p[1] == '*' {
                    for i in 0..=t.len() {
                        if match_here(&p[2..], &t[i..]) {
                            return true;
                        }
                    }
                    false
                } else {
                    for i in 0..=t.len() {
                        if match_here(&p[1..], &t[i..]) {
                            return true;
                        }
                    }
                    false
                }
            }
            '?' => !t.is_empty() && match_here(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && match_here(&p[1..], &t[1..]),
        }
    }
    match_here(&pattern.chars().collect::<Vec<_>>(), &text.chars().collect::<Vec<_>>())
}

// ---------- Git 工具 ----------

async fn git_cmd(base: &str, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(base)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("git 执行失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        return Err(format!("git 错误: {}", stderr.trim()));
    }
    Ok(stdout.trim().to_string())
}

async fn tool_git_status(base: &str, _args: &Value) -> Result<String, String> {
    git_cmd(base, &["status", "--short"]).await
}

async fn tool_git_diff(base: &str, args: &Value) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str());
    let staged = args.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut cmd = vec!["diff"];
    if staged {
        cmd.push("--staged");
    }
    if let Some(p) = path {
        cmd.push(p);
    }
    git_cmd(base, &cmd).await
}

async fn tool_git_log(base: &str, _args: &Value) -> Result<String, String> {
    git_cmd(base, &["log", "--oneline", "-20"]).await
}

async fn tool_git_commit(base: &str, args: &Value) -> Result<String, String> {
    let message = args.get("message").and_then(|v| v.as_str()).ok_or("缺少 message")?;
    git_cmd(base, &["commit", "-m", message]).await
}

async fn tool_git_add(base: &str, args: &Value) -> Result<String, String> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    git_cmd(base, &["add", path]).await
}

async fn tool_git_branch(base: &str, _args: &Value) -> Result<String, String> {
    git_cmd(base, &["branch", "-a"]).await
}

// ---------- 命令工具 ----------

async fn tool_run_command(
    base: &str,
    args: &Value,
    registry: &Arc<CmdRegistry>,
) -> Result<String, String> {
    let command = args.get("command").and_then(|v| v.as_str()).ok_or("缺少 command")?;
    if let Some(reason) = check_dangerous_command(command, Some(base)) {
        return Err(format!("⚠️ 安全拦截：{reason}\n如需执行，请在终端手动操作。"));
    }
    let task_id = uuid::Uuid::new_v4().to_string();
    let status = Arc::new(Mutex::new(CmdStatus {
        running: true,
        ..Default::default()
    }));
    registry
        .statuses
        .lock()
        .unwrap()
        .insert(task_id.clone(), status.clone());
    let mut cmd = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
    #[cfg(windows)]
    {
        cmd.arg("/C").arg(command);
    }
    #[cfg(not(windows))]
    {
        cmd.arg("-c").arg(command);
    }
    cmd.current_dir(base)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = child.id().unwrap_or(0);
    registry.pids.lock().unwrap().insert(task_id.clone(), pid);
    let status2 = status.clone();
    tokio::spawn(async move {
        let output = child.wait_with_output().await;
        let mut st = status2.lock().unwrap();
        st.running = false;
        match output {
            Ok(o) => {
                st.stdout = String::from_utf8_lossy(&o.stdout).chars().take(MAX_CMD_OUTPUT).collect();
                st.stderr = String::from_utf8_lossy(&o.stderr).chars().take(MAX_CMD_OUTPUT).collect();
                st.code = o.status.code();
            }
            Err(e) => {
                st.stderr = format!("命令读取失败: {e}");
            }
        }
    });
    Ok(format!(
        "✅ 命令已启动（任务ID: {task_id}）\n命令: {command}\n请使用 check_command(task_id=\"{task_id}\") 查看状态"
    ))
}

fn tool_check_command(
    _base: &str,
    args: &Value,
    registry: &Arc<CmdRegistry>,
) -> Result<String, String> {
    let task_id = args.get("task_id").and_then(|v| v.as_str()).ok_or("缺少 task_id")?;
    let statuses = registry.statuses.lock().unwrap();
    let st = statuses.get(task_id).ok_or("任务不存在或已结束")?;
    let st = st.lock().unwrap();
    let state = if st.running { "running" } else { "done" };
    Ok(format!(
        "任务ID: {task_id}\n状态: {state}\n输出大小: {} 字符\n--- stdout ---\n{}\n--- stderr ---\n{}",
        st.stdout.len(),
        st.stdout.chars().rev().take(8000).collect::<String>().chars().rev().collect::<String>(),
        st.stderr
    ))
}

async fn tool_terminate_command(
    _base: &str,
    args: &Value,
    registry: &Arc<CmdRegistry>,
) -> Result<String, String> {
    let task_id = args.get("task_id").and_then(|v| v.as_str()).ok_or("缺少 task_id")?;
    let pid = registry.pids.lock().unwrap().remove(task_id);
    match pid {
        Some(pid) => {
            #[cfg(unix)]
            {
                let _ = tokio::process::Command::new("kill")
                    .args(["-9", &format!("-{pid}")])
                    .status()
                    .await;
            }
            #[cfg(windows)]
            {
                let _ = tokio::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .status()
                    .await;
            }
            Ok(format!("已发送终止信号给任务 {task_id}"))
        }
        None => Ok(format!("任务 {task_id} 已结束或不存在")),
    }
}

// ---------- 统一入口 ----------

/// 工作类工具：有实际副作用（写文件/执行命令/改 Git 等），
/// 工程模式下要求存在执行焦点（in_progress 任务），确保 AI 按图执行。
/// 注：plan_* 工具在 execute_tool 中提前返回，不经过此校验（plan_delete 属于
/// 任务图操作，由 AI 自行负责，不强制焦点）
const WORK_TOOLS: &[&str] = &[
    "write_file", "replace_in_file", "rename_file", "copy_file", "move_file",
    "create_directory", "delete_file", "run_command", "terminate_command",
    "git_add", "git_commit", "todo_create", "todo_update",
];

/// 校验工作类工具的执行焦点：
/// 已有任务图 + 无 in_progress 任务 → 报错引导先标记任务
/// （工程模式机制始终生效，不依赖会话 engineering_mode 开关）
fn check_work_focus(
    _db: &Db,
    taskmap_store: &Arc<TaskMapStore>,
    conv_id: &str,
    tool_name: &str,
) -> Result<(), String> {
    if !WORK_TOOLS.contains(&tool_name) {
        return Ok(());
    }
    let store = taskmap_store.lock().unwrap();
    let Some(tm) = store.get(conv_id) else {
        return Ok(()); // 无任务图（尚未规划）→ 不强制
    };
    if tm.has_in_progress() {
        return Ok(());
    }
    // 执行焦点校验始终强制（工程模式机制恒生效）
    Err(format!(
        "【执行焦点校验】执行「{tool_name}」前，必须先声明正在执行的任务：\n- 用 plan_update 把当前任务置为 in_progress（或 plan_focus 声明焦点），完成后 plan_update 置为 done。\n- 任务图状态：{}",
        tm.review_summary_compact()
    ))
}

/// 执行工具（agent 循环与前端共用）。返回给 AI 的文本结果。
pub async fn execute_tool(
    name: &str,
    args: &Value,
    base_dir: &str,
    db: &Db,
    registry: &Arc<CmdRegistry>,
    taskmap_store: &Arc<TaskMapStore>,
    conv_id: &str,
) -> Result<String, String> {
    // 任务图工具（plan_*）
    if name.starts_with("plan_") {
        let mut store = taskmap_store.lock().unwrap();
        let mut tm = store.get(conv_id).cloned();
        let result = plan_dispatch(&mut tm, name, args);
        if let Some(t) = tm {
            // 先序列化落库，再移入 store（避免 borrow 冲突）
            if let Ok(json) = serde_json::to_string(&t.data) {
                let _ = db.taskmap_save(conv_id, &json);
            }
            store.insert(conv_id.to_string(), t);
        }
        return result;
    }
    // ★ 执行焦点校验（工程模式按图执行）：工作类工具须有 in_progress 任务
    check_work_focus(db, taskmap_store, conv_id, name)?;
    match name {
        "list_dir" => tool_list_dir(base_dir, args),
        "read_file" => tool_read_file(base_dir, args),
        "read_file_segment" => tool_read_file_segment(base_dir, args),
        "write_file" => tool_write_file(base_dir, args, db, conv_id),
        "replace_in_file" => tool_replace_in_file(base_dir, args, db, conv_id),
        "rename_file" => tool_rename_file(base_dir, args),
        "copy_file" => tool_copy_file(base_dir, args),
        "move_file" => tool_move_file(base_dir, args),
        "create_directory" => tool_create_directory(base_dir, args),
        "get_file_info" => tool_get_file_info(base_dir, args),
        "delete_file" => tool_delete_file(base_dir, args),
        // ★ 目录遍历型搜索走 spawn_blocking：避免同步 std::fs 遍历阻塞 tokio 运行时
        //   （大目录搜索期间 UI 卡在“等待模型响应”的根因）
        "search_files" => {
            let base = base_dir.to_string();
            let a = args.clone();
            tokio::task::spawn_blocking(move || tool_search_files(&base, &a))
                .await
                .map_err(|e| format!("搜索线程异常: {e}"))?
        }
        "grep_search" => {
            let base = base_dir.to_string();
            let a = args.clone();
            tokio::task::spawn_blocking(move || tool_grep_search(&base, &a))
                .await
                .map_err(|e| format!("搜索线程异常: {e}"))?
        }
        "glob_search" => {
            let base = base_dir.to_string();
            let a = args.clone();
            tokio::task::spawn_blocking(move || tool_glob_search(&base, &a))
                .await
                .map_err(|e| format!("搜索线程异常: {e}"))?
        }
        "git_status" => tool_git_status(base_dir, args).await,
        "git_diff" => tool_git_diff(base_dir, args).await,
        "git_log" => tool_git_log(base_dir, args).await,
        "git_commit" => tool_git_commit(base_dir, args).await,
        "git_add" => tool_git_add(base_dir, args).await,
        "git_branch" => tool_git_branch(base_dir, args).await,
        "run_command" => tool_run_command(base_dir, args, registry).await,
        "check_command" => tool_check_command(base_dir, args, registry),
        "terminate_command" => tool_terminate_command(base_dir, args, registry).await,
        "todo_create" => {
            let title = args.get("title").and_then(|v| v.as_str()).ok_or("缺少 title")?;
            let id = db.todo_create(conv_id, title)?;
            Ok(format!("✅ 已创建待办 #{id}: {title}"))
        }
        "todo_list" => {
            let todos = db.todo_list(conv_id)?;
            if todos.is_empty() {
                Ok("（暂无待办）".into())
            } else {
                let lines: Vec<String> = todos
                    .iter()
                    .map(|(id, title, done)| format!("{} #{} {}", if *done { "✅" } else { "⬜" }, id, title))
                    .collect();
                Ok(lines.join("\n"))
            }
        }
        "todo_update" => {
            let id = args.get("id").and_then(|v| v.as_i64()).ok_or("缺少 id")?;
            let done = args.get("done").and_then(|v| v.as_bool());
            let title = args.get("title").and_then(|v| v.as_str());
            db.todo_update(id, done, title)?;
            Ok(format!("✅ 已更新待办 #{id}"))
        }
        "project_info" => project_info(base_dir).await,
        "diff_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
            diff_file(base_dir, path, db, conv_id)
        }
        "undo_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).ok_or("缺少 path")?;
            undo_file(base_dir, path, db, conv_id)
        }
        "search_conversation_history" => {
            let keyword = args.get("keyword").and_then(|v| v.as_str()).ok_or("缺少 keyword")?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let mut hits = db.search_messages(conv_id, keyword, limit)?;
            let mut archive_hits = db.cache_search(conv_id, keyword, limit)?;
            hits.append(&mut archive_hits);
            if hits.is_empty() {
                Ok("未找到相关历史消息".into())
            } else {
                let lines: Vec<String> = hits
                    .iter()
                    .take(limit)
                    .map(|(role, content)| format!("[{role}] {content}"))
                    .collect();
                Ok(lines.join("\n\n"))
            }
        }
        "retrieve_cache_entry" => {
            let id = args.get("entry_id").and_then(|v| v.as_i64()).ok_or("缺少 entry_id")?;
            match db.cache_get(id)? {
                Some((_summary, data)) => Ok(format!("[缓存条目 #{id}]\n{data}")),
                None => Err(format!("缓存条目 #{id} 不存在")),
            }
        }
        // 上下文感知工具：agent 循环内特判（见 agent.rs），此处兜底避免“未知工具”
        "get_context_remaining" => {
            Ok("该工具仅在 agent 会话循环内可用，将返回当前上下文使用量与剩余空间。".into())
        }
        // 子代理工具：agent 循环内特判（见 agent.rs），此处兜底
        "subagent" | "subagent_report" | "list_agents" | "interrupt_agent" => {
            Ok("该工具仅在 agent 会话循环内可用（由循环接管执行）。".into())
        }
        "skill" => tool_skill(base_dir, args),
        "fetch_webpage" => fetch_webpage(base_dir, args).await,
        "search_web" => tool_search_web(db, conv_id, args).await,
        _ => Err(format!("未知工具: {name}")),
    }
}

// ---------- 扩展工具实现 ----------

/// 联网搜索（优化版）：
/// - 同会话同关键词 30 分钟内直接返回缓存结果（0 延迟、0 token）
/// - 优先 DeepSeek /responses 原生 web_search（结果质量最高）
/// - 保底为三引擎并行抓取（Bing/DDG/百度同时请求，第一个成功即返回）
/// - 输出统一压缩到 SEARCH_OUTPUT_CAP_CHARS 以内，避免大结果在历史里每轮重复付费
const SEARCH_OUTPUT_CAP_CHARS: usize = 5_000;
const SEARCH_CACHE_MAX_AGE_MS: i64 = 30 * 60 * 1000;

async fn tool_search_web(db: &Db, conv_id: &str, args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("缺少 query")?
        .trim()
        .to_string();
    if query.is_empty() {
        return Err("缺少 query".into());
    }
    let max_results = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(5).min(10) as usize;
    let cache_key = format!("websearch:{query}");

    // 0) 缓存命中：同会话重复搜索直接返回（省时省 token）
    if let Some(cached) = db.cache_find_by_summary(conv_id, &cache_key, SEARCH_CACHE_MAX_AGE_MS)? {
        return Ok(format!(
            "🔍 联网搜索结果（缓存命中，30 分钟内的同关键词结果，如需最新请换关键词）：\n{cached}"
        ));
    }

    // 1) 优先：DeepSeek 原生 web_search（模型级搜索，质量最高）
    match native_search(db, &query).await {
        Ok(text) => {
            let out = finalize_search_output(format!("🔍 联网搜索结果（原生搜索）：\n{text}"));
            let _ = db.cache_add(conv_id, &cache_key, &out);
            return Ok(out);
        }
        Err(native_err) => {
            // 2) 保底：三引擎并行抓取（结构化解析标题+链接+摘要）
            match scrape_search(&query, max_results).await {
                Ok(text) => {
                    let out = finalize_search_output(format!("🔍 搜索结果（抓取保底）：\n{text}"));
                    let _ = db.cache_add(conv_id, &cache_key, &out);
                    return Ok(out);
                }
                Err(scrape_err) => {
                    return Err(format!(
                        "联网搜索失败：原生搜索不可用（{native_err}），抓取降级也失败（{scrape_err}）"
                    ));
                }
            }
        }
    }
}

/// 搜索结果统一压缩：超长截断并提示
fn cap_search_output(mut out: String) -> String {
    if out.chars().count() > SEARCH_OUTPUT_CAP_CHARS {
        out = out.chars().take(SEARCH_OUTPUT_CAP_CHARS).collect::<String>() + "\n...（结果已截断）";
    }
    out
}

/// 搜索输出定稿：压缩 + 引用来源提醒（对齐 DSH 的 cite-your-sources 纪律）
fn finalize_search_output(out: String) -> String {
    let capped = cap_search_output(out);
    format!("{capped}\n\n请在回答中以 Markdown 链接格式引用上述来源 URL（如 [标题](链接)）。")
}

/// 原生搜索：DeepSeek /responses 的 web_search 工具（Codex 同款路线）
async fn native_search(db: &Db, query: &str) -> Result<String, String> {
    // 读取 DeepSeek Key
    let keys_json = db.setting_get("provider_keys")?;
    let keys: std::collections::HashMap<String, String> = keys_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    let key = keys
        .get("DeepSeek")
        .cloned()
        .ok_or("未配置 DeepSeek API Key（将自动降级为网页抓取搜索）")?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    // max_output_tokens 收紧到 400：搜索回答只需要点，避免额外 token 浪费
    let payload = serde_json::json!({
        "model": "deepseek-v4-flash",
        "input": query,
        "max_output_tokens": 400,
        "tools": [{"type": "web_search"}],
        "store": false,
    });

    let resp = client
        .post("https://api.deepseek.com/responses")
        .header("Authorization", format!("Bearer {key}"))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("搜索请求失败: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("搜索服务返回 HTTP {status}: {}", text.chars().take(300).collect::<String>()));
    }

    let body: Value = resp.json().await.map_err(|e| format!("响应解析失败: {e}"))?;
    parse_search_response(&body)
}

/// 抓取保底：依次尝试 Bing → DuckDuckGo → Baidu，解析结构化结果
async fn scrape_search(query: &str, max_results: usize) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| e.to_string())?;
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
    let q = urlencode(query);

    // ★ 三引擎并行抓取（Bing/DDG/百度同时请求，第一个成功即返回）：
    //   串行最坏 45s+ → 并行最坏 ~15s
    let bing = async {
        let url = format!("https://www.bing.com/search?q={q}&setlang=zh-hans");
        let resp = client
            .get(&url)
            .header("User-Agent", ua)
            .header("Accept-Language", "zh-CN,zh;q=0.9")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let html = resp.text().await.ok()?;
        parse_bing(&html, max_results)
    };
    let ddg = async {
        let url = format!("https://html.duckduckgo.com/html/?q={q}");
        let resp = client.get(&url).header("User-Agent", ua).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let html = resp.text().await.ok()?;
        parse_ddg(&html, max_results)
    };
    let baidu = async {
        let url = format!("https://www.baidu.com/s?wd={q}");
        let resp = client
            .get(&url)
            .header("User-Agent", ua)
            .header("Accept-Language", "zh-CN,zh;q=0.9")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let html = resp.text().await.ok()?;
        parse_baidu(&html, max_results)
    };

    let (b, d, bd) = tokio::join!(bing, ddg, baidu);
    if let Some(r) = b {
        return Ok(r);
    }
    if let Some(r) = d {
        return Ok(r);
    }
    if let Some(r) = bd {
        return Ok(r);
    }
    Err("所有搜索引擎均无法访问或解析失败，请检查网络".into())
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 解码 Bing 跳转链接（/ck/a?...&u=a1<base64url>）
fn decode_bing_url(raw: &str) -> String {
    if let Some(pos) = raw.find("u=a1") {
        let b64 = &raw[pos + 4..]; // 跳过 "u=a1" 前缀
        let b64: String = b64.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        use base64::Engine;
        if let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64) {
            let text = String::from_utf8_lossy(&decoded).to_string();
            // 结果可能仍带 URL 编码
            return urldecode(&text);
        }
    }
    raw.to_string()
}

/// 解码 DDG 跳转链接（//duckduckgo.com/l/?uddg=<urlencoded>）
fn decode_ddg_url(raw: &str) -> String {
    if let Some(pos) = raw.find("uddg=") {
        let enc = &raw[pos + 5..];
        return urldecode(enc);
    }
    raw.to_string()
}

/// 从 HTML 提取块内第一个 <a href> 的链接
fn first_href(block: &str) -> Option<String> {
    let lower = block.to_lowercase();
    let start = lower.find("<a ")?;
    let href_pos = lower[start..].find("href=")? + start;
    let after = &block[href_pos + 5..];
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = after[1..].find(quote)? + 1;
    Some(after[1..end].to_string())
}

/// 提取块内第一个 <h2>/<h3> 后的文本（去标签）
fn first_heading_text(block: &str) -> Option<String> {
    for tag in ["<h2", "<h3"] {
        if let Some(pos) = block.find(tag) {
            let after = &block[pos..];
            if let Some(gt) = after.find('>') {
                return Some(strip_tags(&after[gt + 1..]).chars().take(200).collect());
            }
        }
    }
    None
}

/// 提取块内第一个 <p> 或摘要容器的文本
fn first_snippet(block: &str) -> Option<String> {
    for marker in ["<p", "class=\"b_caption", "c-abstract", "result__snippet"] {
        if let Some(pos) = block.find(marker) {
            let after = &block[pos..];
            if let Some(gt) = after.find('>') {
                let text = strip_tags(&after[gt + 1..]);
                // ★ 摘要收紧到 200 字符：5 条结果 × 200 ≈ 1k 字符，够模型判断相关性
                let text = text.chars().take(200).collect::<String>();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn parse_bing(html: &str, max: usize) -> Option<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < max {
        let pos = rest.find("b_algo")?;
        let block_start = rest[..pos].rfind('<').unwrap_or(pos);
        let block_end = rest[pos..].find("b_algo").map(|p| pos + p).unwrap_or(rest.len());
        let block_end = if block_end == pos { rest.len() } else { block_end };
        let block = &rest[block_start..block_end];
        let title = first_heading_text(block).unwrap_or_default();
        let href = first_href(block).map(|u| decode_bing_url(&u)).unwrap_or_default();
        let snippet = first_snippet(block).unwrap_or_default();
        if !title.is_empty() || !href.is_empty() {
            // ★ 尽力提取发布日期（如 2024-05-12），帮助模型判断时效性
            let date = detect_date(&snippet).map(|d| format!("（{d}）")).unwrap_or_default();
            out.push(format!("{}. {}{date}\n   {}\n   {}", out.len() + 1, title, href, snippet));
        }
        rest = &rest[block_end..];
        if block_end >= rest.len() {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out.join("\n")) }
}

fn parse_ddg(html: &str, max: usize) -> Option<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < max {
        let pos = rest.find("result__a")?;
        let block_start = rest[..pos].rfind('<').unwrap_or(pos.saturating_sub(200));
        let block_end = rest[pos..].find("result__a").map(|p| pos + p).unwrap_or(rest.len());
        let block_end = if block_end == pos { rest.len() } else { block_end };
        let block = &rest[block_start..block_end];
        let title = first_heading_text(block).unwrap_or_default();
        let href = first_href(block).map(|u| decode_ddg_url(&u)).unwrap_or_default();
        let snippet = first_snippet(block).unwrap_or_default();
        if !title.is_empty() || !href.is_empty() {
            let date = detect_date(&snippet).map(|d| format!("（{d}）")).unwrap_or_default();
            out.push(format!("{}. {}{date}\n   {}\n   {}", out.len() + 1, title, href, snippet));
        }
        rest = &rest[block_end..];
        if block_end >= rest.len() {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out.join("\n")) }
}

fn parse_baidu(html: &str, max: usize) -> Option<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < max {
        let pos = rest.find("c-container")?;
        let block_start = rest[..pos].rfind('<').unwrap_or(pos.saturating_sub(200));
        let block_end = rest[pos..].find("c-container").map(|p| pos + p).unwrap_or(rest.len());
        let block_end = if block_end == pos { rest.len() } else { block_end };
        let block = &rest[block_start..block_end];
        let title = first_heading_text(block).unwrap_or_default();
        let href = first_href(block).unwrap_or_default();
        let snippet = first_snippet(block).unwrap_or_default();
        if !title.is_empty() {
            let date = detect_date(&snippet).map(|d| format!("（{d}）")).unwrap_or_default();
            out.push(format!("{}. {}{date}\n   {}\n   {}", out.len() + 1, title, href, snippet));
        }
        rest = &rest[block_end..];
        if block_end >= rest.len() {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out.join("\n")) }
}

/// 解析 /responses 搜索响应：提取 message 文本与引用来源
fn parse_search_response(body: &Value) -> Result<String, String> {
    let mut texts: Vec<String> = Vec::new();
    if let Some(items) = body.get("output").and_then(|o| o.as_array()) {
        for item in items {
            if item.get("type").and_then(|t| t.as_str()) == Some("message") {
                if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                    let mut parts: Vec<String> = Vec::new();
                    for part in content {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            parts.push(t.to_string());
                        }
                        // 引用来源（annotations 或直接 url）
                        if let Some(url) = part.get("url").and_then(|u| u.as_str()) {
                            parts.push(format!("[来源: {url}]"));
                        }
                        if let Some(anns) = part.get("annotations").and_then(|a| a.as_array()) {
                            for ann in anns {
                                if let Some(url) = ann.get("url").and_then(|u| u.as_str()) {
                                    parts.push(format!("[来源: {url}]"));
                                }
                            }
                        }
                    }
                    if !parts.is_empty() {
                        texts.push(parts.join("\n"));
                    }
                }
            }
        }
    }

    if texts.is_empty() {
        if let Some(err) = body.get("error") {
            return Err(format!("搜索失败: {err}"));
        }
        return Err("搜索无结果，请换一个关键词试试".into());
    }
    Ok(texts.join("\n\n"))
}

async fn project_info(base: &str) -> Result<String, String> {
    let entries = std::fs::read_dir(base).map_err(|e| e.to_string())?;
    let names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    let has = |s: &str| names.iter().any(|n| n == s);
    let any_ext = |exts: &[&str]| names.iter().any(|n| exts.iter().any(|e| n.ends_with(e)));

    let (ptype, lang, build, test_framework) = if has("Cargo.toml") {
        ("Rust 项目", "Rust", "cargo", "cargo test")
    } else if has("package.json") {
        ("Node 项目", "JavaScript/TypeScript", "npm", "npm test")
    } else if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        ("Python 项目", "Python", "pip", "pytest")
    } else if has("go.mod") {
        ("Go 项目", "Go", "go build", "go test")
    } else if any_ext(&[".py"]) {
        ("Python 脚本", "Python", "-", "pytest")
    } else if any_ext(&[".rs"]) {
        ("Rust 源码", "Rust", "-", "-")
    } else if any_ext(&[".ts", ".tsx"]) || any_ext(&[".js", ".jsx"]) {
        ("Web 项目", "JavaScript/TypeScript", "npm", "-")
    } else {
        ("未知项目", "未知", "-", "-")
    };
    let git = if has(".git") { "是" } else { "否" };
    Ok(format!(
        "项目类型: {ptype}\n语言: {lang}\n构建: {build}\n测试: {test_framework}\nGit: {git}\n文件数: {}",
        names.len()
    ))
}

fn diff_file(base: &str, path: &str, db: &Db, conv_id: &str) -> Result<String, String> {
    let full = safe_path(base, path)?;
    let current = read_text(&full)?;
    match db.backup_latest(conv_id, path)? {
        Some(old) => {
            if old == current {
                Ok(format!("文件与最近备份一致（{path}）"))
            } else {
                // 简单行级对比
                let old_lines: Vec<&str> = old.lines().collect();
                let cur_lines: Vec<&str> = current.lines().collect();
                let mut diff_lines: Vec<String> = Vec::new();
                let max = old_lines.len().max(cur_lines.len());
                for i in 0..max {
                    let a = old_lines.get(i).unwrap_or(&"");
                    let b = cur_lines.get(i).unwrap_or(&"");
                    if a != b {
                        diff_lines.push(format!("L{}: -{} | +{}", i + 1, a, b));
                    }
                }
                Ok(format!(
                    "{} 与备份差异（{} 处）:\n{}",
                    path,
                    diff_lines.len(),
                    diff_lines.into_iter().take(30).collect::<Vec<_>>().join("\n")
                ))
            }
        }
        None => Err("该文件没有备份记录（写文件时会自动备份）".into()),
    }
}

fn undo_file(base: &str, path: &str, db: &Db, conv_id: &str) -> Result<String, String> {
    let full = safe_path(base, path)?;
    match db.backup_latest(conv_id, path)? {
        Some(old) => {
            std::fs::write(&full, old).map_err(|e| e.to_string())?;
            Ok(format!("✅ 已恢复 {path} 到最近备份"))
        }
        None => Err("该文件没有备份记录".into()),
    }
}

/// 简化版网页抓取：GET + 提取标题和正文文本
async fn fetch_webpage(base: &str, args: &Value) -> Result<String, String> {
    let url = args.get("url").and_then(|v| v.as_str()).ok_or("缺少 url")?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Canlow-Next)")
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    // ★ 响应体上限防护：>8MB 直接拒绝，避免下载巨型页面浪费时间与带宽
    if let Some(len) = resp.content_length() {
        if len > 8 * 1024 * 1024 {
            return Err(format!("页面过大（{len} 字节），已拒绝下载"));
        }
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    // 正文提取：HTML → Markdown（保留链接/列表/代码块/表格结构，对齐 DSH 的 turndown 方案）
    let title = extract_tag(&text, "title").unwrap_or_default();
    let body = html_to_markdown(&text);
    let body = body.chars().take(3000).collect::<String>();
    let _ = base;
    Ok(format!("标题: {title}\n\n{body}"))
}

fn extract_tag(html: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = html.find(&open)?;
    let content_start = html[start..].find('>')? + start + 1;
    let end = html[content_start..].find(&close)? + content_start;
    Some(html[content_start..end].trim().to_string())
}

/// 剥离 HTML 标签并去除 <script>/<style> 块。
/// 按 char 遍历（非字节），中文内容不会 panic / 乱码。
fn strip_tags(html: &str) -> String {
    let chars: Vec<char> = html.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let n = chars.len();
    let mut out = String::new();
    let mut in_tag = false;
    let mut in_raw = false; // script/style 内容区

    // 检查从 idx 开始是否以给定标签开头（且标签名后是合法的边界字符）
    let starts_with_tag = |idx: usize, tag: &str| -> bool {
        let t: Vec<char> = tag.chars().collect();
        if idx + t.len() > n {
            return false;
        }
        if lower[idx..idx + t.len()] != t[..] {
            return false;
        }
        // 边界：标签名后必须是 空白 / '>' / '/'（避免 <scripture> 误判）
        let after = lower.get(idx + t.len()).copied();
        matches!(after, None | Some(' ') | Some('\t') | Some('\n') | Some('>') | Some('/'))
    };

    let mut idx = 0;
    while idx < n {
        let ch = chars[idx];
        if !in_tag && !in_raw && ch == '<' {
            if starts_with_tag(idx, "<script") || starts_with_tag(idx, "<style") {
                in_raw = true;
                idx += 1;
                continue;
            }
            in_tag = true;
            idx += 1;
            continue;
        }
        if in_raw {
            if ch == '<' && (starts_with_tag(idx, "</script") || starts_with_tag(idx, "</style")) {
                in_raw = false;
                idx += 1; // 跳过 '<'，标签其余部分由 in_tag 逻辑处理
                in_tag = true;
                continue;
            }
            idx += 1;
            continue;
        }
        if in_tag {
            if ch == '>' {
                in_tag = false;
            }
            idx += 1;
            continue;
        }
        out.push(ch);
        idx += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 简易 HTML → Markdown 转换（零依赖；思路对齐 DSH 的 turndown）
/// 支持：标题/段落/链接/列表/代码块/表格/强调/图片/引用/分隔线；
/// script/style/noscript 内容丢弃；注释跳过；文本空白折叠。
enum HtTok {
    Text(String),
    Open { name: String, attrs: Vec<(String, String)> },
    Close(String),
}

fn html_tokenize(html: &str) -> Vec<HtTok> {
    let chars: Vec<char> = html.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let n = chars.len();
    let mut toks: Vec<HtTok> = Vec::new();
    let mut text = String::new();
    let mut i = 0usize;
    let flush = |text: &mut String, toks: &mut Vec<HtTok>| {
        if !text.is_empty() {
            toks.push(HtTok::Text(std::mem::take(text)));
        }
    };
    while i < n {
        if chars[i] != '<' {
            text.push(chars[i]);
            i += 1;
            continue;
        }
        // 注释 <!-- ... -->：整段跳过
        if lower[i..].starts_with(&['<', '!', '-', '-']) {
            flush(&mut text, &mut toks);
            let mut j = i + 4;
            while j + 2 < n && !(lower[j] == '-' && lower[j + 1] == '-' && lower[j + 2] == '>') {
                j += 1;
            }
            i = (j + 3).min(n);
            continue;
        }
        // 结束标签 </name>
        if i + 1 < n && lower[i + 1] == '/' {
            let mut j = i + 2;
            let mut name = String::new();
            while j < n && lower[j].is_ascii_alphanumeric() {
                name.push(lower[j]);
                j += 1;
            }
            flush(&mut text, &mut toks);
            toks.push(HtTok::Close(name));
            while j < n && chars[j] != '>' {
                j += 1;
            }
            i = (j + 1).min(n);
            continue;
        }
        // 开始标签 <name attrs>
        let mut j = i + 1;
        let mut name = String::new();
        while j < n && lower[j].is_ascii_alphanumeric() {
            name.push(lower[j]);
            j += 1;
        }
        if name.is_empty() {
            text.push('<');
            i += 1;
            continue;
        }
        let mut attrs: Vec<(String, String)> = Vec::new();
        while j < n && chars[j] != '>' {
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            if j < n && (chars[j] == '>' || chars[j] == '/') {
                if chars[j] == '/' {
                    j += 1;
                }
                break;
            }
            let mut an = String::new();
            while j < n && (lower[j].is_ascii_alphanumeric() || chars[j] == '-' || chars[j] == '_' || chars[j] == ':') {
                an.push(lower[j]);
                j += 1;
            }
            let mut k = j;
            while k < n && chars[k].is_whitespace() {
                k += 1;
            }
            let mut av = String::new();
            if k < n && chars[k] == '=' {
                k += 1;
                while k < n && chars[k].is_whitespace() {
                    k += 1;
                }
                let quote = if k < n && (chars[k] == '"' || chars[k] == '\'') {
                    let q = chars[k];
                    k += 1;
                    Some(q)
                } else {
                    None
                };
                while k < n {
                    if let Some(q) = quote {
                        if chars[k] == q {
                            break;
                        }
                    } else if chars[k].is_whitespace() || chars[k] == '>' {
                        break;
                    }
                    av.push(chars[k]);
                    k += 1;
                }
                if quote.is_some() && k < n {
                    k += 1;
                }
                j = k;
            } else {
                j = k;
            }
            if !an.is_empty() {
                attrs.push((an, av));
            }
        }
        flush(&mut text, &mut toks);
        if j < n {
            j += 1;
        }
        toks.push(HtTok::Open { name, attrs });
        i = j.min(n);
    }
    flush(&mut text, &mut toks);
    toks
}

fn html_attr(attrs: &[(String, String)], key: &str) -> Option<String> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn md_push_text(out: &mut String, s: &str) {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return;
    }
    let ends_mark = out.ends_with(['[', '(', '!', '*', '`']);
    if !out.is_empty() && !out.ends_with(' ') && !out.ends_with('\n') && !ends_mark {
        out.push(' ');
    }
    out.push_str(&collapsed);
}

fn md_ensure_break(out: &mut String) {
    while out.ends_with([' ', '\t', '\n']) {
        out.pop();
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
}

fn html_to_markdown(html: &str) -> String {
    let toks = html_tokenize(html);
    let mut out = String::new();
    let mut stack: Vec<String> = Vec::new();
    let mut lists: Vec<(char, usize)> = Vec::new();
    let mut cell: Option<String> = None;
    let mut row: Vec<String> = Vec::new();
    let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
    let mut thead = false;
    let mut in_pre = false;
    // script/style/noscript 内部文本一律跳过（含嵌套）
    let mut skip_depth = 0usize;
    let is_block = |name: &str| -> bool {
        matches!(name, "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside" | "blockquote" | "figure" | "figcaption" | "details" | "summary" | "form" | "li" | "ul" | "ol" | "table" | "tr" | "thead" | "tbody" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "hr")
    };
    let is_void = |name: &str| -> bool {
        matches!(name, "br" | "hr" | "img" | "input" | "meta" | "link" | "source" | "wbr")
    };
    for tok in toks {
        match tok {
            HtTok::Text(t) => {
                if skip_depth > 0 {
                    continue;
                }
                if in_pre {
                    out.push_str(&t);
                    continue;
                }
                if let Some(buf) = &mut cell {
                    md_push_text(buf, &t);
                    continue;
                }
                md_push_text(&mut out, &t);
            }
            HtTok::Open { name, attrs } => {
                match name.as_str() {
                    "script" | "style" | "noscript" => {
                        skip_depth += 1;
                        stack.push(name.clone());
                    }
                    "pre" => {
                        md_ensure_break(&mut out);
                        out.push_str("```\n");
                        in_pre = true;
                        stack.push(name.clone());
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        md_ensure_break(&mut out);
                        let lvl: usize = name.chars().nth(1).and_then(|c| c.to_digit(10)).unwrap_or(1) as usize;
                        out.push_str(&"#".repeat(lvl));
                        out.push(' ');
                        stack.push(name.clone());
                    }
                    "blockquote" => {
                        md_ensure_break(&mut out);
                        out.push_str("> ");
                        stack.push(name.clone());
                    }
                    "hr" => {
                        md_ensure_break(&mut out);
                        out.push_str("---");
                        md_ensure_break(&mut out);
                    }
                    "ul" | "ol" | "u" => {
                        lists.push((if name == "ol" { 'o' } else { 'u' }, 0));
                        md_ensure_break(&mut out);
                        stack.push(name.clone());
                    }
                    "li" => {
                        md_ensure_break(&mut out);
                        let indent = "  ".repeat(lists.len().saturating_sub(1));
                        let prefix = match lists.last_mut() {
                            Some((kind, cnt)) => {
                                *cnt += 1;
                                if *kind == 'o' { format!("{cnt}. ") } else { "- ".to_string() }
                            }
                            None => "- ".to_string(),
                        };
                        out.push_str(&indent);
                        out.push_str(&prefix);
                        stack.push(name.clone());
                    }
                    "a" => {
                        let href = html_attr(&attrs, "href").unwrap_or_default();
                        out.push('[');
                        stack.push(format!("a:{href}"));
                    }
                    "strong" | "b" => {
                        out.push_str("**");
                        stack.push(name.clone());
                    }
                    "em" | "i" => {
                        out.push('*');
                        stack.push(name.clone());
                    }
                    "code" => {
                        if !in_pre {
                            out.push('`');
                        }
                        stack.push(name.clone());
                    }
                    "img" => {
                        let src = html_attr(&attrs, "src").unwrap_or_default();
                        let alt = html_attr(&attrs, "alt").unwrap_or_default();
                        if !src.is_empty() {
                            out.push_str(&format!("![{alt}]({src})"));
                        }
                    }
                    "table" => {
                        rows.clear();
                        md_ensure_break(&mut out);
                        stack.push(name.clone());
                    }
                    "thead" => {
                        thead = true;
                        stack.push(name.clone());
                    }
                    "tbody" | "tfoot" => {
                        stack.push(name.clone());
                    }
                    "tr" => {
                        row.clear();
                        stack.push(name.clone());
                    }
                    "td" | "th" => {
                        cell = Some(String::new());
                        stack.push(name.clone());
                    }
                    _ if is_block(&name) => {
                        md_ensure_break(&mut out);
                        stack.push(name.clone());
                    }
                    _ => {
                        if !is_void(&name) {
                            stack.push(name.clone());
                        }
                    }
                }
            }
            HtTok::Close(name) => {
                match name.as_str() {
                    "pre" => {
                        in_pre = false;
                        out.push_str("\n```");
                        md_ensure_break(&mut out);
                        stack.pop();
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" => {
                        md_ensure_break(&mut out);
                        stack.pop();
                    }
                    "ul" | "ol" | "u" => {
                        lists.pop();
                        md_ensure_break(&mut out);
                        stack.pop();
                    }
                    "li" => {
                        out.push('\n');
                        stack.pop();
                    }
                    "a" => {
                        if let Some(top) = stack.pop() {
                            if let Some(href) = top.strip_prefix("a:") {
                                out.push_str(&format!("]({href})"));
                            }
                        }
                    }
                    "strong" | "b" => {
                        out.push_str("**");
                        stack.pop();
                    }
                    "em" | "i" => {
                        out.push('*');
                        stack.pop();
                    }
                    "code" => {
                        if !in_pre {
                            out.push('`');
                        }
                        stack.pop();
                    }
                    "td" | "th" => {
                        if let Some(buf) = cell.take() {
                            row.push(buf);
                        }
                        stack.pop();
                    }
                    "tr" => {
                        rows.push((thead, std::mem::take(&mut row)));
                        stack.pop();
                    }
                    "table" => {
                        for (is_header, cells) in &rows {
                            let line = format!("| {} |", cells.iter().map(|c| c.replace('|', "\\|")).collect::<Vec<_>>().join(" | "));
                            out.push_str(&line);
                            out.push('\n');
                            if *is_header {
                                out.push('|');
                                for _ in cells {
                                    out.push_str(" --- |");
                                }
                                out.push('\n');
                            }
                        }
                        md_ensure_break(&mut out);
                        stack.pop();
                    }
                    "thead" => {
                        thead = false;
                        stack.pop();
                    }
                    "tbody" | "tfoot" | "script" | "style" | "noscript" => {
                        if matches!(name.as_str(), "script" | "style" | "noscript") {
                            skip_depth = skip_depth.saturating_sub(1);
                        }
                        stack.pop();
                    }
                    _ if is_block(&name) => {
                        md_ensure_break(&mut out);
                        stack.pop();
                    }
                    _ => {
                        stack.pop();
                    }
                }
            }
        }
    }
    out.trim().to_string()
}

/// 尽力从文本中提取日期（YYYY-MM-DD / YYYY/M/D / YYYY年M月D日），用于搜索结果展示
fn detect_date(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let read_num = |chars: &[char], start: usize| -> (usize, Option<u32>) {
        let mut j = start;
        let mut v: u32 = 0;
        while j < chars.len() && chars[j].is_ascii_digit() {
            v = v * 10 + (chars[j] as u32 - '0' as u32);
            j += 1;
        }
        if j == start { (0, None) } else { (j - start, Some(v)) }
    };
    let valid = |y: u32, m: u32, d: u32| y >= 2000 && y <= 2035 && (1..=12).contains(&m) && (1..=31).contains(&d);
    let mut i = 0usize;
    while i + 3 < n {
        if chars[i..i + 4].iter().all(|c| c.is_ascii_digit()) {
            let y: u32 = chars[i..i + 4].iter().collect::<String>().parse().unwrap_or(0);
            if let Some(sep) = chars.get(i + 4).copied() {
                if sep == '-' || sep == '/' {
                    let (mlen, m) = read_num(&chars, i + 5);
                    if let (Some(mv), Some(sep2)) = (m, chars.get(i + 5 + mlen).copied()) {
                        if sep2 == sep {
                            let (_, d) = read_num(&chars, i + 5 + mlen + 1);
                            if let Some(dv) = d {
                                if valid(y, mv, dv) {
                                    return Some(format!("{y}-{mv:02}-{dv:02}"));
                                }
                            }
                        }
                    }
                } else if sep == '年' {
                    let (mlen, m) = read_num(&chars, i + 5);
                    if let (Some(mv), Some(_)) = (m, chars.get(i + 5 + mlen).copied()) {
                        if chars.get(i + 5 + mlen) == Some(&'月') {
                            let (dlen, d) = read_num(&chars, i + 5 + mlen + 1);
                            if let (Some(dv), Some(_)) = (d, chars.get(i + 5 + mlen + 1 + dlen).copied()) {
                                if chars.get(i + 5 + mlen + 1 + dlen) == Some(&'日') && valid(y, mv, dv) {
                                    return Some(format!("{y}年{mv}月{dv}日"));
                                }
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

// ---------- 技能系统（Skill） ----------
/// 项目级技能目录（相对工作区）
const PROJECT_SKILLS_DIR: &str = ".canlow/skills";

/// 解析极简 frontmatter（--- 开头，name/description 键值），返回 (元数据, 正文)
fn parse_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let body = rest[end + 4..].trim_start().to_string();
            let mut map = HashMap::new();
            for line in fm.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                }
            }
            return (map, body);
        }
    }
    (HashMap::new(), content.trim_start().to_string())
}

/// 收集一个技能目录下的技能（<dir>/<name>/SKILL.md 或 <dir>/<name>.md）
fn collect_skills_from_dir(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        let skill_path = if path.is_dir() {
            path.join("SKILL.md")
        } else if fname.ends_with(".md") {
            path
        } else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&skill_path) else {
            continue;
        };
        let (meta, _body) = parse_frontmatter(&content);
        let base_name = fname.trim_end_matches(".md").to_string();
        let name = meta.get("name").cloned().unwrap_or(base_name);
        let desc = meta
            .get("description")
            .cloned()
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect();
        out.push((name, desc));
    }
}

/// 技能详情（供设置面板展示）：(名称, 描述, 来源标签, 绝对路径)
pub fn skill_infos(work_dir: &str) -> Vec<(String, String, String, String)> {
    let mut out: Vec<(String, String, String, String)> = Vec::new();
    let collect = |dir: &std::path::Path, source: &str, out: &mut Vec<(String, String, String, String)>| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = entry.file_name().to_string_lossy().to_string();
            let skill_path = if path.is_dir() {
                path.join("SKILL.md")
            } else if fname.ends_with(".md") {
                path
            } else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&skill_path) else {
                continue;
            };
            let (meta, _body) = parse_frontmatter(&content);
            let base_name = fname.trim_end_matches(".md").to_string();
            let name = meta.get("name").cloned().unwrap_or(base_name);
            let desc = meta
                .get("description")
                .cloned()
                .unwrap_or_default()
                .chars()
                .take(120)
                .collect();
            out.push((name, desc, source.to_string(), skill_path.to_string_lossy().to_string()));
        }
    };
    collect(&std::path::Path::new(work_dir).join(PROJECT_SKILLS_DIR), "项目", &mut out);
    if let Some(home) = dirs::home_dir() {
        collect(&home.join(".canlow").join("skills"), "用户", &mut out);
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|(n, _, _, _)| seen.insert(n.clone()));
    out
}

/// 技能目录路径：(项目级, 用户级)
pub fn skill_dirs(work_dir: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let proj = std::path::Path::new(work_dir).join(PROJECT_SKILLS_DIR);
    let user = dirs::home_dir()
        .map(|h| h.join(".canlow").join("skills"))
        .unwrap_or_else(|| std::path::PathBuf::from(".canlow-skills"));
    (proj, user)
}

/// 创建技能：写入 <dir>/<name>/SKILL.md（带 frontmatter）。dir 不存在时自动创建。
pub fn skill_create(dir: &std::path::Path, name: &str, description: &str, content: &str) -> Result<String, String> {
    let name_clean = name.trim().trim_matches('/');
    if name_clean.is_empty() {
        return Err("技能名称不能为空".into());
    }
    if !name_clean
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ' ' || c.is_ascii())
    {
        return Err("技能名称只能包含字母、数字、- _ . 与空格".into());
    }
    let skill_path = dir.join(name_clean).join("SKILL.md");
    if skill_path.exists() {
        return Err(format!("技能已存在：{name_clean}"));
    }
    let desc = description.trim();
    let mut md = String::new();
    md.push_str("---\n");
    md.push_str(&format!("name: {name_clean}\n"));
    if !desc.is_empty() {
        md.push_str(&format!("description: {desc}\n"));
    }
    md.push_str("---\n\n");
    md.push_str(if content.trim().is_empty() {
        "# 技能说明\n\n在这里写下技能的具体指令：告诉 AI 何时使用、如何一步步执行、有什么注意事项。\n"
    } else {
        content
    });
    std::fs::create_dir_all(skill_path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&skill_path, md).map_err(|e| format!("写入失败: {e}"))?;
    Ok(skill_path.to_string_lossy().to_string())
}
/// 扫描工作区与用户目录的可用技能（项目优先，去重）
pub fn scan_skills(work_dir: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    collect_skills_from_dir(&std::path::Path::new(work_dir).join(PROJECT_SKILLS_DIR), &mut out);
    if let Some(home) = dirs::home_dir() {
        collect_skills_from_dir(&home.join(".canlow").join("skills"), &mut out);
    }
    // 去重（项目优先）
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|(n, _)| seen.insert(n.clone()));
    out
}

/// 加载技能正文（项目优先，其次用户目录）
fn load_skill(work_dir: &str, name: &str) -> Result<String, String> {
    let name_clean = name.trim().trim_matches('/').to_string();
    if name_clean.is_empty() {
        return Err("缺少技能名称".into());
    }
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    let proj = std::path::Path::new(work_dir).join(PROJECT_SKILLS_DIR);
    candidates.push(proj.join(&name_clean).join("SKILL.md"));
    candidates.push(proj.join(format!("{name_clean}.md")));
    if let Some(home) = dirs::home_dir() {
        let user = home.join(".canlow").join("skills");
        candidates.push(user.join(&name_clean).join("SKILL.md"));
        candidates.push(user.join(format!("{name_clean}.md")));
    }
    for c in &candidates {
        if let Ok(content) = std::fs::read_to_string(c) {
            let (meta, body) = parse_frontmatter(&content);
            let title = meta.get("name").cloned().unwrap_or_else(|| name_clean.clone());
            let desc = meta.get("description").cloned().unwrap_or_default();
            let mut out = format!("# 技能：{title}\n");
            if !desc.is_empty() {
                out.push_str(&format!("（{desc}）\n\n"));
            }
            out.push_str(&body);
            // 上限 3 万字符，超出由 spill 机制落盘
            return Ok(out.chars().take(30_000).collect());
        }
    }
    Err(format!("技能不存在：{name_clean}（可用 skill 不带参数查看可用技能列表）"))
}

/// skill 工具：name 为空返回技能列表；否则加载技能正文
fn tool_skill(work_dir: &str, args: &Value) -> Result<String, String> {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
    if name.is_empty() {
        let skills = scan_skills(work_dir);
        if skills.is_empty() {
            return Ok("（当前工作区与用户目录没有可用技能。技能文件放在 <工作区>/.canlow/skills/<名称>/SKILL.md 或 ~/.canlow/skills/<名称>/SKILL.md，文件头用 --- 标注 name 与 description）".into());
        }
        let lines: Vec<String> = skills
            .iter()
            .map(|(n, d)| if d.is_empty() { format!("- {n}") } else { format!("- {n}：{d}") })
            .collect();
        return Ok(format!("【可用技能】\n{}\n\n（加载技能内容用 skill 工具传对应名称）", lines.join("\n")));
    }
    load_skill(work_dir, name)
}

/// 任务图工具定义（工程模式相关）
pub fn taskmap_tools() -> Value {
    plan_tool_definitions()
}

/// 工具 JSON Schema 定义（固定、缓存友好）
pub fn tool_definitions() -> Value {
    let file_args = |extra: Option<Value>| -> Value {
        let mut p = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "相对工作目录的路径"}
            },
            "required": ["path"]
        });
        if let Some(e) = extra {
            let props = p.get_mut("properties").unwrap().as_object_mut().unwrap();
            for (k, v) in e.as_object().unwrap() {
                props.insert(k.clone(), v.clone());
            }
        }
        p
    };
    let f = |name: &str, desc: &str, params: Value| {
        json!({
            "type": "function",
            "function": {"name": name, "description": desc, "parameters": params}
        })
    };
    let str_param = |desc: &str| json!({"type": "string", "description": desc});
    let num_param = |desc: &str| json!({"type": "integer", "description": desc});

    json!([
        f("list_dir", "列出目录内容", file_args(None)),
        f("read_file", "读取文件。可用 path 读单个文件，或用 paths 数组一次读多个文件（大项目推荐）", json!({
            "type": "object",
            "properties": {
                "path": json!({"oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}], "description": "单个文件路径，或路径数组"}),
                "paths": json!({"type": "array", "items": {"type": "string"}, "description": "要读取的多个文件路径（推荐）"})
            }
        })),
        f("read_file_segment", "分段读取大文件", json!({
            "type": "object",
            "properties": {
                "path": str_param("文件路径"),
                "segment_index": num_param("从 0 开始的段号"),
                "chars_per_segment": num_param("每段字符数，默认 50000")
            },
            "required": ["path", "segment_index"]
        })),
        f("write_file", "写入文件（覆盖）", json!({
            "type": "object",
            "properties": {"path": str_param("路径"), "content": str_param("完整内容")},
            "required": ["path", "content"]
        })),
        f("replace_in_file", "精准替换文件中的文本", json!({
            "type": "object",
            "properties": {
                "path": str_param("路径"),
                "old_str": str_param("要替换的原文"),
                "new_str": str_param("替换后的文本")
            },
            "required": ["path", "old_str", "new_str"]
        })),
        f("rename_file", "重命名", file_args(Some(json!({"new_name": str_param("新名称")})))),
        f("copy_file", "复制文件", file_args(Some(json!({"new_path": str_param("目标路径")})))),
        f("move_file", "移动文件", file_args(Some(json!({"new_path": str_param("目标路径")})))),
        f("create_directory", "创建目录", file_args(None)),
        f("get_file_info", "查看文件信息", file_args(None)),
        f("delete_file", "删除文件或目录", file_args(None)),
        f("search_files", "按文件名搜索（★ 默认跳过 node_modules/.git/dist/target 等噪音目录；大目录遍历自动截断并提示）", json!({
            "type": "object",
            "properties": {
                "pattern": str_param("文件名关键词"),
                "path": str_param("搜索目录，默认当前"),
                "max_results": num_param("最大结果数")
            },
            "required": ["pattern"]
        })),
        f("grep_search", "按内容搜索（★ 默认跳过 node_modules/.git/dist/target 等噪音目录；超过 max_file_size 的大文件跳过并在结果尾部列出；大目录遍历自动截断并提示；如需搜噪音目录请把 path 直接指向其内部）", json!({
            "type": "object",
            "properties": {
                "pattern": str_param("搜索关键词"),
                "path": str_param("搜索目录"),
                "max_matches": num_param("最大匹配数"),
                "case_sensitive": json!({"type": "boolean"}),
                "max_file_size": num_param("跳过超过该 MB 数的文件（默认 1，即 >1MB 跳过；搜日志/大文件时调大，如 50）")
            },
            "required": ["pattern"]
        })),
        f("glob_search", "glob 模式搜索（★ 默认跳过 node_modules/.git/dist/target 等噪音目录；大目录遍历自动截断并提示）", json!({
            "type": "object",
            "properties": {
                "pattern": str_param("如 **/*.py"),
                "path": str_param("搜索目录"),
                "max_results": num_param("最大结果数")
            },
            "required": ["pattern"]
        })),
        f("git_status", "Git 状态", json!({"type": "object", "properties": {}})),
        f("git_diff", "Git 差异", json!({
            "type": "object",
            "properties": {
                "path": str_param("可选路径"),
                "staged": json!({"type": "boolean"})
            }
        })),
        f("git_log", "Git 提交历史", json!({"type": "object", "properties": {}})),
        f("git_commit", "Git 提交", json!({
            "type": "object",
            "properties": {"message": str_param("提交信息")},
            "required": ["message"]
        })),
        f("git_add", "Git 暂存", json!({
            "type": "object",
            "properties": {"path": str_param("路径，默认 .")}
        })),
        f("git_branch", "Git 分支列表", json!({"type": "object", "properties": {}})),
        f("run_command", "执行命令（有安全拦截）", json!({
            "type": "object",
            "properties": {"command": str_param("shell 命令")},
            "required": ["command"]
        })),
        f("check_command", "查看异步命令状态", json!({
            "type": "object",
            "properties": {"task_id": str_param("任务ID")},
            "required": ["task_id"]
        })),
        f("terminate_command", "终止异步命令", json!({
            "type": "object",
            "properties": {"task_id": str_param("任务ID")},
            "required": ["task_id"]
        })),
        f("todo_create", "创建待办事项", json!({
            "type": "object",
            "properties": {"title": str_param("待办标题")},
            "required": ["title"]
        })),
        f("todo_list", "列出待办事项", json!({"type": "object", "properties": {}})),
        f("todo_update", "更新待办", json!({
            "type": "object",
            "properties": {
                "id": num_param("待办ID"),
                "done": json!({"type": "boolean", "description": "是否完成"}),
                "title": str_param("新标题")
            },
            "required": ["id"]
        })),
        f("project_info", "检测当前项目类型/语言/构建/测试", json!({"type": "object", "properties": {}})),
        f("diff_file", "对比文件与最近备份的差异", file_args(None)),
        f("undo_file", "把文件恢复到最近一次备份（写文件时自动备份）", file_args(None)),
        f("search_conversation_history", "搜索当前对话的历史消息", json!({
            "type": "object",
            "properties": {
                "keyword": str_param("搜索关键词"),
                "limit": num_param("最大条数，默认 10")
            },
            "required": ["keyword"]
        })),
        f("retrieve_cache_entry", "取回被压缩保存的原始消息", json!({
            "type": "object",
            "properties": {"entry_id": num_param("缓存条目ID")},
            "required": ["entry_id"]
        })),
        f("get_context_remaining", "查看当前上下文使用量与剩余空间", json!({"type": "object", "properties": {}})),
        f("fetch_webpage", "抓取【已知 URL】的网页内容（标题 + Markdown 正文，保留链接/列表/代码块/表格结构）；只能抓指定链接，不能搜索，搜索请用 search_web", json!({
            "type": "object",
            "properties": {"url": str_param("网页 URL")},
            "required": ["url"]
        })),
        f("skill", "加载技能完整内容：传 name 加载对应技能（技能文件位于 <工作区>/.canlow/skills/<名称>/SKILL.md 或 ~/.canlow/skills/<名称>/SKILL.md，文件头用 --- 标注 name/description）；省略 name 返回当前可用技能列表", json!({
            "type": "object",
            "properties": {"name": str_param("技能名称（可选；省略返回技能列表）")}
        })),
        f("subagent", "委派一个自包含任务给子代理（独立上下文、共享工作区、看不到本对话历史）。prompt 必须完整自包含（包含全部背景与要求，子代理不能追问）；运行结束返回子代理的最终总结", json!({
            "type": "object",
            "properties": {
                "description": str_param("3-5 字任务描述"),
                "prompt": str_param("完整独立的任务提示词"),
                "run_in_background": json!({"type": "boolean", "description": "true=后台运行（立即返回子代理 ID，之后用 subagent_report 获取结果、list_agents 查看状态、interrupt_agent 中断）；默认 false=前台等待完成"})
            },
            "required": ["description", "prompt"]
        })),
        f("subagent_report", "获取子代理运行结果（运行中返回当前状态，已完成返回最终总结）", json!({
            "type": "object",
            "properties": {"subagent_id": str_param("子代理 ID（subagent 返回或 list_agents 列出）")},
            "required": ["subagent_id"]
        })),
        f("list_agents", "列出所有子代理及其状态（运行中/已完成/已中断、轮数）", json!({
            "type": "object",
            "properties": {}
        })),
        f("interrupt_agent", "请求中断一个正在运行的子代理", json!({
            "type": "object",
            "properties": {"agent_id": str_param("子代理 ID")},
            "required": ["agent_id"]
        })),
        f("search_web", "★ 联网搜索（首选）：需要最新/实时信息时用这个，返回带来源的结果；同会话重复搜索会命中缓存（30 分钟内），如需最新结果请换更具体的关键词；不要用 fetch_webpage 代替搜索", json!({
            "type": "object",
            "properties": {
                "query": str_param("搜索关键词，简洁明确"),
                "max_results": num_param("最大结果数，默认 5，最多 10")
            },
            "required": ["query"]
        })),
    ])
}

#[cfg(test)]
mod safe_path_tests {
    use super::*;

    /// 创建临时工作目录 + 同级"外部"目录，返回 (base, sibling)
    fn setup() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("canlow-safepath-{}", uuid::Uuid::new_v4()));
        let base = root.join("workdir");
        let sibling = root.join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(base.join("a.txt"), "hello").unwrap();
        (base, sibling)
    }

    #[test]
    fn dot_and_empty_path_are_allowed() {
        // ★ 回归：AI 第一步 list_dir(path=".") 曾误报"路径越界"
        let (base, _) = setup();
        let b = base.to_string_lossy().to_string();
        assert!(safe_path(&b, ".").is_ok(), "\".\" 不应报越界");
        assert!(safe_path(&b, "").is_ok(), "空路径不应报越界");
        assert!(safe_path(&b, &b).is_ok(), "显式 base 路径不应报越界");
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    #[test]
    fn relative_subpaths_allowed() {
        let (base, _) = setup();
        let b = base.to_string_lossy().to_string();
        // 存在的文件
        assert!(safe_path(&b, "a.txt").is_ok());
        // 不存在的文件（父目录存在）
        assert!(safe_path(&b, "nope.txt").is_ok());
        // 深层新建路径（父目录也不存在）
        assert!(safe_path(&b, "x/y/z/deep.txt").is_ok());
        // 含 .. 但仍在工作区内
        assert!(safe_path(&b, "sub/../a.txt").is_ok());
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    #[test]
    fn escaping_paths_rejected() {
        let (base, sibling) = setup();
        let b = base.to_string_lossy().to_string();
        // 相对越界：../outside
        assert!(safe_path(&b, "../outside/evil.txt").is_err(), ".. 越界应拒绝");
        assert!(safe_path(&b, "../outside").is_err());
        // 绝对越界
        assert!(safe_path(&b, "/etc/passwd").is_err(), "绝对路径越界应拒绝");
        assert!(safe_path(&b, "/tmp").is_err(), "外部目录应拒绝");
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
        let _ = sibling;
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected_symlink_inside_allowed() {
        let (base, sibling) = setup();
        let b = base.to_string_lossy().to_string();
        // 指向工作区内的符号链接 → 放行
        let in_link = base.join("link_in.txt");
        std::os::unix::fs::symlink(base.join("a.txt"), &in_link).unwrap();
        assert!(safe_path(&b, "link_in.txt").is_ok(), "区内符号链接应放行");
        // 指向工作区外的符号链接 → 拒绝
        let out_link = base.join("link_out.txt");
        std::os::unix::fs::symlink(sibling.join("secret.txt"), &out_link).unwrap();
        let r = safe_path(&b, "link_out.txt");
        assert!(r.is_err(), "指向区外的符号链接应拒绝: {:?}", r);
        // 悬空链接（目标不存在）：无法解析真实目标，保守拒绝（写入会创建 target，可能落在区外）
        let dead_link = base.join("link_dead.txt");
        std::os::unix::fs::symlink(sibling.join("ghost.txt"), &dead_link).unwrap();
        assert!(safe_path(&b, "link_dead.txt").is_err(), "悬空链接应保守拒绝（防写入逃逸）");
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn sample_response() -> Value {
        json!({
            "output": [
                {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "searching"}]},
                {"type": "web_search_call", "id": "ws_1"},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "Rust 最新稳定版是 1.97.1。", "annotations": [{"type": "url_citation", "url": "https://blog.rust-lang.org"}]}
                ]}
            ]
        })
    }

    #[test]
    fn parses_search_result_text_and_citation() {
        let out = parse_search_response(&sample_response()).unwrap();
        assert!(out.contains("1.97.1"));
        assert!(out.contains("https://blog.rust-lang.org"));
    }

    #[test]
    fn empty_search_returns_error() {
        let err = parse_search_response(&json!({"output": []})).unwrap_err();
        assert!(err.contains("无结果"));
    }

    #[test]
    fn error_payload_surfaces_message() {
        let err = parse_search_response(&json!({"error": "rate limited"})).unwrap_err();
        assert!(err.contains("rate limited"));
    }
}

#[cfg(test)]
mod scrape_tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn frontmatter_parses_name_and_description() {
        let content = "---\nname: code-review\ndescription: 代码评审流程\n---\n\n# 正文\n步骤...";
        let (meta, body) = parse_frontmatter(content);
        assert_eq!(meta.get("name").map(|s| s.as_str()), Some("code-review"));
        assert_eq!(meta.get("description").map(|s| s.as_str()), Some("代码评审流程"));
        assert!(body.starts_with("# 正文"), "正文应去掉 frontmatter: {body}");
    }

    #[test]
    fn frontmatter_without_header_keeps_whole_content() {
        let content = "纯文本技能内容";
        let (meta, body) = parse_frontmatter(content);
        assert!(meta.is_empty());
        assert_eq!(body, "纯文本技能内容");
    }

    #[test]
    fn scan_and_load_skill_from_temp_dir() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("canlow-skill-test-{}", uuid::Uuid::new_v4()));
        let skill_dir = dir.join(".canlow").join("skills").join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut f = std::fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        writeln!(f, "---\nname: my-skill\ndescription: 测试技能\n---\n").unwrap();
        writeln!(f, "# 我的技能\n第一步做 A。").unwrap();

        let skills = scan_skills(dir.to_str().unwrap());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].0, "my-skill");
        assert_eq!(skills[0].1, "测试技能");

        let loaded = load_skill(dir.to_str().unwrap(), "my-skill").unwrap();
        assert!(loaded.contains("# 技能：my-skill"), "加载应带标题: {loaded}");
        assert!(loaded.contains("第一步做 A"), "正文应完整: {loaded}");
        assert!(load_skill(dir.to_str().unwrap(), "nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn html_to_markdown_basic_structures() {
        let html = concat!(
            "<html><head><title>T</title><script>var x=1;</script></head><body>",
            "<h1>标题</h1>",
            "<p>一段文字带<a href=\"https://example.com\">链接</a>和<strong>加粗</strong>。</p>",
            "<ul><li>第一项</li><li>第二项</li></ul>",
            "<pre><code>let a = 1;</code></pre>",
            "</body></html>"
        );
        let md = html_to_markdown(html);
        assert!(md.contains("# 标题"), "标题应转为 h1: {md}");
        assert!(md.contains("[链接](https://example.com)"), "链接应保留: {md}");
        assert!(md.contains("**加粗**"), "加粗应保留: {md}");
        assert!(md.contains("- 第一项"), "列表项应保留: {md}");
        assert!(md.contains("```"), "代码块应有围栏: {md}");
        assert!(!md.contains("var x=1"), "script 内容应丢弃: {md}");
        assert!(!md.contains("<p>"), "不应残留 HTML 标签: {md}");
    }

    #[test]
    fn html_to_markdown_table() {
        let html = "<table><thead><tr><th>名称</th><th>版本</th></tr></thead>
            <tbody><tr><td>Rust</td><td>1.97</td></tr></tbody></table>";
        let md = html_to_markdown(html);
        assert!(md.contains("| 名称 | 版本 |"), "表头行: {md}");
        assert!(md.contains("| --- | --- |"), "分隔行: {md}");
        assert!(md.contains("| Rust | 1.97 |"), "数据行: {md}");
    }

    #[test]
    fn detect_date_formats() {
        assert_eq!(detect_date("发布于 2024-05-12 的文章"), Some("2024-05-12".into()));
        assert_eq!(detect_date("2024年5月12日更新"), Some("2024年5月12日".into()));
        assert_eq!(detect_date("2024/5/2 发表"), Some("2024-05-02".into()));
        assert_eq!(detect_date("没有任何日期"), None);
        assert_eq!(detect_date("版本号 3.1.2 发布"), None, "不应误判版本号");
    }

    #[test]
    fn bing_jump_url_decodes() {
        let real = "https://example.com/page?x=1";
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(real.as_bytes());
        let raw = format!("https://www.bing.com/ck/a?u=a1{b64}&ntb=1");
        assert_eq!(decode_bing_url(&raw), real);
    }

    #[test]
    fn ddg_jump_url_decodes() {
        let raw = format!("//duckduckgo.com/l/?uddg={}", urlencode("https://example.com/测试"));
        assert_eq!(decode_ddg_url(&raw), "https://example.com/测试");
    }

    #[test]
    fn parse_bing_html_extracts_results() {
        let html = r#"<html><body>
            <li class="b_algo"><h2><a href="https://www.bing.com/ck/a?u=a1aHR0cHM6Ly9leGFtcGxlLmNvbQ">Example Site</a></h2><p>This is the snippet.</p></li>
            <li class="b_algo"><h2><a href="https://example.org">Org</a></h2><p>Second result.</p></li>
        </body></html>"#;
        let out = parse_bing(html, 5).unwrap();
        assert!(out.contains("Example Site"));
        assert!(out.contains("https://example.com"));
        assert!(out.contains("This is the snippet"));
        assert!(out.contains("Org"));
    }

    #[test]
    fn parse_fails_on_empty() {
        assert!(parse_bing("<html>nothing here</html>", 5).is_none());
    }

    #[test]
    fn url_roundtrip() {
        let q = "你好 world 2026";
        assert_eq!(urldecode(&urlencode(q)), q);
    }
}
