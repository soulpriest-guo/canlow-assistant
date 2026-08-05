// Agent 循环：流式多轮 tool_calls（参考 Codex 的 turn 结构）
// - 每轮：完整历史(追加式) + 新输出 → 追加回存储
// - 工具调用：授权（ask/allow_all）→ 执行 → 结果作为 tool 消息追加
// - 停止：stop_flag 在每轮/每块检查
use futures_util::StreamExt;
use serde::Serialize;
use std::error::Error as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

use super::db::Db;
use super::taskmap::{TaskMap, TaskMapData, TaskMapStore};
use super::tools::{self, CmdRegistry};
use super::types::{ChatMessage, ProviderConfig, ToolCall};

pub fn build_system_prompt(engineering_mode: bool, taskmap_review: Option<&str>) -> String {
    let mut p = r#"你是 Canlow，一个智能编程助手。
你与用户共享一个工作区。保持对话简洁、直接，用中文回复。
可以使用工具完成任务：文件操作、搜索、Git、命令执行、任务图规划。
规则：
1. 命令执行后用 check_command 跟进一次状态。
2. 大文件用 read_file_segment 分段读取。
3. 修改文件小改动用 replace_in_file，重写用 write_file。
4. 所有路径为相对路径。
5. 大项目策略：先 glob_search / grep_search 定位，再用 read_file 的 paths 参数一次读多个文件，不要逐个单文件读取。
6. 工具输出可能被截断（会有提示）：被截断时用针对性读取/搜索获取需要部分，不要盲目重读整个文件。
7. 工具失败不要盲目重试；连续 2 次失败就基于已有信息回答。
8. 可用 get_context_remaining 查看剩余上下文空间，据此决定继续读取还是先总结。
9. 早期对话被压缩后完整内容会自动归档，search_conversation_history 会同时搜索当前历史和归档快照；需要找回被压缩的细节时用它，取回后直接使用，不要反复搜索。
10. 【联网搜索】需要最新/实时信息（新闻、版本号、API 文档、天气、事件、你不知道的事实等）时，必须优先调用 search_web 工具——它是内置联网搜索，会返回带来源的结果。fetch_webpage 只能抓取你【已经知道】的具体 URL，不能代替搜索。search_web 结果不够时，再用 fetch_webpage 抓取结果中的链接看细节。
11. 完成目标后必须立即给出最终总结，不要继续调用工具。"#
        .to_string();
    if engineering_mode {
        p.push_str(
            "

【工程模式】当前已开启：任务图是唯一的执行计划，你必须严格按图执行。
              ★ 任务图尚未创建时（本会话还没有任务图）：
              - 你可以先用只读工具（list_dir / read_file / project_info / grep_search 等）了解项目环境，再 plan_init 创建任务图；
              - 创建任务图之前不能执行写文件/运行命令等有副作用的操作（会被告知先规划）。
              ★ 执行顺序语义（图排列 = 实际执行指令，不是可行性约束）：
              - 任务图的上下排列就是 AI 的实际执行顺序指令：同父子任务默认按图从上到下依次执行（串行），每完成一个（done）再执行下一个，不要跳过排在前面的未完成任务；
              - 当前版本没有子代理（subagent）：同级任务一律按图从上到下竖排依次执行，不设横向并排；后续引入子代理能力后再支持并行横排；
              - 规划/调整任务时用 deps 声明执行顺序：先执行的作为后执行者的 deps（同层级形成串行链 → 布局为上下排列）；无需声明并行（当前无子代理，同级默认竖排）。这个排列就是给你的执行指令。
              ★ 任务编号规则（防止混淆，务必遵守）：
              - 每个任务有唯一层级编号：编号 = 父任务自身编号段 + 本级字母 + 本级数字，只保留「父+自身」两段，不携带祖父级编号；
                例：一级任务 a1 → 其子任务 a1b1/a1b2 → a1b2 的子任务 b2c1 → b2c1 的子任务 c1d5；
              - 同一字母层级下数字全局递增不重复（如 b1c1/b1c2、b2c3/b2c4、b3c5/b3c6）；
              - plan_review / plan_find 输出中会展示层级编号，引用任务时优先用编号（ID）精确指定，避免标题混淆。
              ★ 执行纪律（必须遵守）：
              - 开始每个任务节点之前，必须先 plan_review 仔细阅读任务表，确认当前要执行哪个任务（用编号识别）；
              - 开始执行某个任务前，先调用 plan_update 把该任务置为 in_progress（系统会记录开始时间，并自动设为执行焦点）；
              - 没有 in_progress 任务时，写文件/执行命令等有副作用的工具会被系统拦截（执行焦点校验），必须先 plan_update 或 plan_focus 声明正在执行的任务；
              - 执行过程中按序推进，完成后立即 plan_update 置为 done（进度自动 100%）；
              - 每个任务处理完后（置 done 前）再 plan_review 一次，对照任务表确认该任务的子任务/依赖已完成，防止遗漏或混淆；
              - 每个阶段结束都要更新任务图状态，让用户实时看到进度；禁止跳过 plan_update 直接总结；
              - 状态必须按流程流转：todo→in_progress→done（或 blocked），禁止跳过（如 todo 直接 done 会被拒绝）；
              - 阻塞时：先完成被阻塞任务的前序任务，不要跳过；
              - 关键路径：plan_review 会给出最长串行链，优先推进关键路径上的任务；
              - 误操作（如误删节点）可用 plan_undo 回滚。
              ★ 层级语义（任务图的结构就是执行计划，层级必须有意义）：
              - 任务目标节点（root）是唯一的「总节点」：只在一开始创建任务图时存在，高于所有任务，它本身不是任务，所有任务都是它的子节点；
              - 一级任务 = 任务总结节点（对整个任务的总结/目标），挂在任务目标总节点（root）之下；一个任务图的一级任务就是若干个任务总结；
              - 二级任务 = 完成一级任务的方法步骤；三级任务 = 进一步细分的步骤，以此类推；
              - 细分到可直接执行的粒度即可，禁止为了细分而细分、禁止在执行中不断往下拆任务。
              ★ 规划方式（小步规划）：
              - plan_init 的 breakdown 顶层任务 = 一级任务（任务总结，1-3 个），挂到任务目标总节点（root）下；不要把具体步骤塞进 plan_init 顶层；
              - 步骤用 plan_breakdown 挂到对应一级任务下：parent_id 传一级任务 ID（或标题），形成 总结→方法步骤→细分 的层级（单批 ≤10 个，建议 3-5 个）；
              - 后续对话新添加的一级任务（包括与之前差异很大的新要求）一律用 plan_breakdown（parent_id 省略或传 root）挂到任务目标总节点（root）之下，作为新的任务总结；不要再创建与 root 平级的独立任务线；
              - 用户是在原本要求上的延续或调整 → 在原有任务架构上调整（plan_breakdown 挂到对应任务下、plan_update、plan_link），不要新建一级任务；
              - plan_update 支持批量：tasks: [{task_id, status}] 一次更新多个任务；
              - task_id 可以用标题引用（唯一匹配自动解析），或先用 plan_find 按标题搜索拿到 ID；
              - plan_review 可加 node_id 只看某子树，大图时按需查看；
              - 用户修改任务图后：必须重新审视计划，必要时用 plan_requirement / plan_breakdown / plan_link 调整，然后继续按新图执行。",
        );
    }
    if let Some(review) = taskmap_review {
        p.push_str("

【当前任务图状态】
");
        p.push_str(review);
    }
    p
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub request_id: String,
    pub tool_name: String,
    pub description: String,
    pub conv_id: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    pub tool_name: String,
    /// 工具是否执行成功（不含“被用户拒绝”的情况）
    pub ok: bool,
    /// 是否被用户拒绝
    pub rejected: bool,
    pub message: String,
    pub conv_id: String,
}

#[derive(Clone, Copy, Default)]
pub struct CacheStats {
    pub hit: i64,
    pub miss: i64,
}

/// 单个会话的运行状态（stop_flag / 授权 / 计划确认 / 缓存统计按会话隔离，
/// 避免多会话并发时互相干扰）
pub struct AgentSession {
    pub stop_flag: AtomicBool,
    pub pending_permissions: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    /// 计划确认等待：AI 修改任务图结构后暂停循环，等待用户同意/拒绝
    /// （所有创建/更改任务都必须用户确认才继续执行；一次只允许一个等待）
    pub pending_plan_confirm: Mutex<Option<tokio::sync::oneshot::Sender<bool>>>,
    pub cache_stats: Mutex<CacheStats>,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self {
            stop_flag: AtomicBool::new(false),
            pending_permissions: Mutex::new(HashMap::new()),
            pending_plan_confirm: Mutex::new(None),
            cache_stats: Mutex::new(CacheStats::default()),
        }
    }
}

/// 授权模式：
/// 0 = ask（逐次询问一切）
/// 1 = smart（只读安全操作自动，写/执行/删除等询问）—— 类似 Codex 的 untrusted
/// 2 = allow_all（全部自动）
/// 3 = none（无监管：所有请求——工具授权、计划确认——都不需要确认）
pub const AUTH_ASK: u8 = 0;
pub const AUTH_SMART: u8 = 1;
pub const AUTH_ALLOW_ALL: u8 = 2;
pub const AUTH_NONE: u8 = 3;

/// 结构变更类任务图工具：AI 调用这些工具后必须用户确认才继续执行
/// （创建/加子任务/调顺序/移动/删除/改需求都需要确认，与授权模式无关；
///   plan_update 仅更新进度/状态，不触发确认）
const STRUCT_TOOLS: &[&str] = &[
    "plan_init",
    "plan_breakdown",
    "plan_link",
    "plan_move",
    "plan_delete",
    "plan_requirement",
];

pub struct AgentState {
    /// conv_id -> 会话状态
    pub sessions: Mutex<HashMap<String, Arc<AgentSession>>>,
    /// 全局授权模式（跨会话生效）
    pub auth_mode: Arc<AtomicU8>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            auth_mode: Arc::new(AtomicU8::new(AUTH_SMART)),
        }
    }
}

/// 只读安全工具：smart 模式下自动执行，无需询问
/// 任务图工具（plan_*）为无副作用的纯内存/DB 操作，工程模式核心流程依赖，
/// 若每次 plan_update/plan_link 都弹授权会打断执行节奏，故除 plan_delete（破坏性删除）
/// 外全部列入自动执行。
pub fn is_safe_tool(name: &str) -> bool {
    matches!(
        name,
        "list_dir" | "read_file" | "read_file_segment" | "search_files" | "grep_search"
            | "glob_search" | "get_file_info" | "project_info" | "diff_file"
            | "git_status" | "git_diff" | "git_log" | "git_branch"
            | "search_conversation_history" | "retrieve_cache_entry" | "get_context_remaining"
            | "todo_list" | "plan_review" | "fetch_webpage" | "search_web" | "check_command"
            | "plan_init" | "plan_breakdown" | "plan_update" | "plan_link" | "plan_requirement"
            | "plan_move" | "plan_export" | "plan_find" | "plan_undo" | "plan_focus"
    )
}

/// 只读命令白名单（smart 模式下 run_command 自动执行；参考 Codex is_known_safe_command）
const SAFE_COMMANDS: &[&str] = &[
    "cat", "cd", "cut", "echo", "expr", "false", "grep", "head", "id", "ls",
    "nl", "paste", "pwd", "rev", "seq", "stat", "tail", "tr", "true", "uname",
    "uniq", "wc", "which", "whoami", "date", "printf", "basename", "dirname",
    "readlink", "sort", "file", "du", "df", "tree", "type", "env",
    "diff", "rg", "man", "find", "base64",
];

/// git 只读子命令白名单
const SAFE_GIT_SUBCOMMANDS: &[&str] = &[
    "status", "diff", "log", "branch", "show", "remote", "tag", "ls-files",
    "rev-parse", "blame", "grep", "shortlog", "stash", "submodule",
];

fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\''))) {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// 判定单条命令（无组合符）是否只读安全
fn is_safe_single_command(seg: &str) -> bool {
    let seg = seg.trim();
    if seg.is_empty() {
        return true;
    }
    let mut tokens = seg.split_whitespace();
    let Some(cmd0) = tokens.next() else {
        return false;
    };
    let cmd = strip_quotes(cmd0)
        .split('/')
        .last()
        .unwrap_or(strip_quotes(cmd0))
        .to_lowercase();

    if cmd == "git" {
        let Some(sub) = tokens.next().map(strip_quotes) else {
            return false; // 裸 git 需要帮助，不自动
        };
        return SAFE_GIT_SUBCOMMANDS.contains(&sub.to_lowercase().as_str());
    }

    if !SAFE_COMMANDS.contains(&cmd.as_str()) {
        return false;
    }

    // 参数级安全检查（参考 Codex）
    match cmd.as_str() {
        "find" => {
            const UNSAFE: &[&str] = &["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0", "-fprintf"];
            !tokens.any(|a| UNSAFE.contains(&a))
        }
        "sed" => !tokens.any(|a| a == "-i" || a == "--in-place" || a.starts_with("-i")),
        "base64" => !tokens.any(|a| a == "-o" || a == "--output" || a.starts_with("--output=") || (a.starts_with("-o") && a != "-o")),
        _ => true,
    }
}

/// 判定整条 shell 命令是否只读安全（允许 && || ; | 组合，拒绝命令替换/写重定向）
pub fn is_safe_command_text(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return true;
    }
    // 拒绝命令替换与写重定向（可能执行任意命令/写文件）
    if cmd.contains("$(") || cmd.contains('`') || cmd.contains('>') {
        return false;
    }
    let normalized = cmd
        .replace("&&", "
")
        .replace("||", "
")
        .replace(';', "
")
        .replace('|', "
");
    normalized.lines().all(|seg| is_safe_single_command(seg.trim()))
}

/// 流式请求 DeepSeek，返回 (content, reasoning, tool_calls)
/// 调试：记录每次请求的指纹（messages+tools 序列化 hash），用于定位缓存命中率骤降
fn log_request_fingerprint(app: &AppHandle, messages: &[ChatMessage], tools: &Value, model: &str) {
    if let Ok(dir) = app.path().app_data_dir() {
        let path = dir.join("request_fingerprints.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let msgs_json = serde_json::to_string(messages).unwrap_or_default();
            let tools_json = serde_json::to_string(tools).unwrap_or_default();
            // FNV-1a 双 hash
            let mut h1: u64 = 0xcbf29ce484222325;
            let mut h2: u64 = 0x84222325cbf29ce4;
            for b in msgs_json.bytes().chain(tools_json.bytes()).chain(model.bytes()) {
                h1 ^= b as u64;
                h1 = h1.wrapping_mul(0x100000001b3);
                h2 ^= b as u64;
                h2 = h2.wrapping_mul(0x100000001b3);
            }
            // tools 与 system 的独立 hash（排查缓存 miss 是否来自这两处）
            fn fnv(data: &[u8]) -> u64 {
                let mut h: u64 = 0xcbf29ce484222325;
                for b in data {
                    h ^= *b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                h
            }
            let tools_hash = fnv(tools_json.as_bytes());
            let sys_full = messages
                .first()
                .map(|m| format!("{}|{}", m.role, m.content))
                .unwrap_or_default();
            let sys_hash = fnv(sys_full.as_bytes());
            let _ = writeln!(
                f,
                "--- req msgs={} tools={}B thash={:016x} syshash={:016x} hash={:016x}{:016x} model={}",
                messages.len(),
                tools_json.len(),
                tools_hash,
                sys_hash,
                h1,
                h2,
                model
            );
            for (i, m) in messages.iter().enumerate() {
                // 指纹覆盖 role + content + reasoning_content + tool_calls（完整字节）
                let mut mh: u64 = 0xcbf29ce484222325;
                let rc = m.reasoning_content.as_deref().unwrap_or("");
                let tc = m
                    .tool_calls
                    .as_ref()
                    .map(|t| serde_json::to_string(t).unwrap_or_default())
                    .unwrap_or_default();
                for b in m.role.bytes().chain(m.content.bytes()).chain(rc.bytes()).chain(tc.bytes()) {
                    mh ^= b as u64;
                    mh = mh.wrapping_mul(0x100000001b3);
                }
                let head: String = m.content.chars().take(24).collect();
                let _ = writeln!(
                    f,
                    "  [{}] {} len={} rc={} tc={} h={:016x} head={}",
                    i,
                    m.role,
                    m.content.len(),
                    rc.len(),
                    tc.len(),
                    mh,
                    head.replace('\n', " ")
                );
            }
        }
    }
}

async fn stream_request(
    app: &AppHandle,
    provider: &ProviderConfig,
    messages: &[ChatMessage],
    tools: &Value,
    stop_flag: &AtomicBool,
    conv_id: &str,
) -> Result<(String, String, Vec<ToolCall>, Option<Value>), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

    // ★ 手动构建 snake_case 请求体（serde camelCase 只用于存储/前端，
    //   OpenAI 兼容 API 要求 role/content/reasoning_content/tool_call_id）
    let api_messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut obj = json!({
                "role": m.role,
                "content": m.content,
            });
            if let Some(rc) = &m.reasoning_content {
                obj["reasoning_content"] = json!(rc);
            } else if m.tool_calls.is_some() {
                // DeepSeek thinking 模式：带 tool_calls 的 assistant 消息必须回传 reasoning_content
                obj["reasoning_content"] = json!("");
            }
            if let Some(tcs) = &m.tool_calls {
                obj["tool_calls"] = json!(tcs.iter().map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.function.name,
                            "arguments": tc.function.arguments
                        }
                    })
                }).collect::<Vec<_>>());
            }
            if let Some(tcid) = &m.tool_call_id {
                obj["tool_call_id"] = json!(tcid);
            }
            if let Some(n) = &m.name {
                obj["name"] = json!(n);
            }
            obj
        })
        .collect();

    log_request_fingerprint(app, messages, tools, &provider.model);

    let mut payload = json!({
        "model": provider.model,
        "messages": api_messages,
        "stream": true,
        "tools": tools,
        "tool_choice": "auto",
    });
    if let Some(thinking) = provider.thinking {
        payload["thinking"] = json!({"type": if thinking { "enabled" } else { "disabled" }});
    }
    if let Some(effort) = &provider.reasoning_effort {
        payload["reasoning_effort"] = json!(effort);
    }

    // 连接阶段重试（偶发网络失败）：最多 3 次，退避 1s/2s
    let mut resp = None;
    for attempt in 0..3 {
        match client
            .post(&url)
            .header("Authorization", format!("Bearer {}", provider.api_key))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(e) => {
                if attempt == 2 {
                    return Err(format!("请求失败: {}", full_error(&e)));
                }
                let delay = 1000 * (attempt as u64 + 1);
                let msg = format!("网络连接失败，{} 秒后重试 ({}/{})", delay / 1000, attempt + 1, 3);
                let _ = app.emit("stream-notice", json!({"message": msg, "convId": conv_id}));
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
    }
    let resp = resp.ok_or("请求失败")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let msg = format!("HTTP {status}: {}", text.chars().take(500).collect::<String>());
        let _ = app.emit("stream-error", json!({"error": msg, "convId": conv_id}));
        return Err(msg);
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut acc: Vec<(String, String, String)> = Vec::new(); // id, name, arguments
    let mut last_usage: Option<Value> = None;
    let mut done = false;

    while let Some(chunk) = stream.next().await {
        if stop_flag.load(Ordering::Relaxed) {
            return Err("已停止".into());
        }
        let bytes = chunk.map_err(|e| format!("流读取失败: {e}"))?;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            if !line.starts_with("data: ") {
                continue;
            }
            let data = &line["data: ".len()..];
            if data == "[DONE]" {
                done = true;
                break;
            }
            let v: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let choices = v.get("choices").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            let mut delta = String::new();
            let mut reas = String::new();
            if let Some(choice) = choices.first() {
                if let Some(d) = choice.get("delta") {
                    delta = d.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                    reas = d
                        .get("reasoning_content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    // 工具调用增量
                    if let Some(tcs) = d.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                            while acc.len() <= idx {
                                acc.push((String::new(), String::new(), String::new()));
                            }
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                acc[idx].0 = id.to_string();
                            }
                            if let Some(f) = tc.get("function") {
                                if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                                    acc[idx].1 = n.to_string();
                                }
                                if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                                    acc[idx].2.push_str(a);
                                }
                            }
                        }
                    }
                }
            }
            content.push_str(&delta);
            reasoning.push_str(&reas);
            let usage = v.get("usage").cloned().filter(|u| !u.is_null());
            if usage.is_some() {
                last_usage = usage.clone();
            }
            if !delta.is_empty() || !reas.is_empty() || usage.is_some() {
                app.emit(
                    "stream-chunk",
                    json!({
                        "delta": delta,
                        "reasoning": if reas.is_empty() { Value::Null } else { Value::String(reas) },
                        "usage": usage,
                        "done": false,
                        "convId": conv_id
                    }),
                )
                .map_err(|e| format!("事件发送失败: {e}"))?;
            }
        }
        if done {
            break;
        }
    }

    let tool_calls: Vec<ToolCall> = acc
        .into_iter()
        .filter(|(id, name, _)| !name.is_empty() || !id.is_empty())
        .map(|(id, name, arguments)| ToolCall {
            id: if id.is_empty() { format!("call_{}", uuid::Uuid::new_v4()) } else { id },
            call_type: "function".into(),
            function: super::types::ToolCallFunction { name, arguments },
        })
        .collect();

    Ok((content, reasoning, tool_calls, last_usage))
}

/// 本地抽取式摘要：从被压缩的消息里提取关键进展（结论、执行过的工具、涉及文件）
/// 生成可用的交接摘要，避免 AI 因丢失上下文而反复检索旧内容
fn local_summarize(dropped: &[ChatMessage]) -> String {
    use std::collections::HashSet;
    let mut user_count = 0usize;
    let mut tool_names: HashSet<String> = HashSet::new();
    let mut file_refs: HashSet<String> = HashSet::new();
    let mut conclusions: Vec<String> = Vec::new();

    for m in dropped {
        match m.role.as_str() {
            "user" => user_count += 1,
            "assistant" => {
                if !m.content.is_empty() {
                    let head: String = m.content.chars().take(200).collect();
                    conclusions.push(head);
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        tool_names.insert(tc.function.name.clone());
                        if let Ok(v) = serde_json::from_str::<Value>(&tc.function.arguments) {
                            if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
                                file_refs.insert(p.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = String::from("[上下文摘要] 早期对话的进展如下：");
    if user_count > 0 {
        out.push_str(&format!("\n- 已讨论过 {} 轮用户需求", user_count));
    }
    if !conclusions.is_empty() {
        out.push_str("\n- 之前得出的结论/回复：");
        for c in conclusions.iter().rev().take(2) {
            out.push_str(&format!("\n  · {}", c));
        }
    }
    if !tool_names.is_empty() {
        out.push_str(&format!(
            "\n- 已执行过的工具：{}",
            tool_names.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if !file_refs.is_empty() {
        let refs: Vec<String> = file_refs.into_iter().take(8).collect();
        out.push_str(&format!("\n- 涉及文件：{}", refs.join(", ")));
    }
    out.push_str(
        "\n\n以上摘要已包含继续工作所需的关键信息，请直接基于摘要和当前上下文继续，不要再检索或寻找已被压缩的历史内容。",
    );
    out
}

/// 上下文压缩（缓存友好）：保留用户消息原文（预算内从新到旧）+ 最近 N 条完整轮次，
/// 丢弃的原文存入缓存表（retrieve_cache_entry 可取回）
/// 上下文档位：按模型上下文窗口选择压缩策略
/// - "1m"：适合 1M 上下文模型（DeepSeek v4），压缩阈值高，压缩频率低
/// - "256k"：适合 256k 上下文模型（OpenAI/Kimi 等），压缩阈值低
#[derive(Clone, Copy)]
pub struct ContextProfile {
    pub max_chars: usize,
    pub user_budget_chars: usize,
    pub keep_recent_chars: usize,
    pub budget_chars: usize,
    pub budget_tokens: usize,
}

pub fn context_profile(db: &Db) -> ContextProfile {
    match db
        .setting_get("context_profile")
        .ok()
        .flatten()
        .as_deref()
    {
        Some("256k") => ContextProfile {
            max_chars: 600_000,        // 约 200k tokens（256k 窗口留足输出空间）
            user_budget_chars: 60_000, // 约 2 万 tokens 用户消息
            keep_recent_chars: 150_000, // 约 5 万 tokens 最近消息（覆盖 4-5 轮工作）
            budget_chars: 520_000,     // 85% 预算提醒
            budget_tokens: 200_000,
        },
        _ => ContextProfile {
            max_chars: 2_400_000,      // 约 800k tokens（1M 窗口，最大化缓存利用）
            user_budget_chars: 120_000, // 约 4 万 tokens 用户消息
            keep_recent_chars: 400_000, // 约 13 万 tokens 最近消息（覆盖 4-5 轮工作）
            budget_chars: 2_100_000,   // 85% 预算提醒
            budget_tokens: 800_000,
        },
    }
}

pub fn compact_history_if_needed(
    db: &Db,
    conv_id: &str,
    history: Vec<ChatMessage>,
) -> Result<Vec<ChatMessage>, String> {
    let profile = context_profile(db);
    let max_chars = profile.max_chars;
    let user_budget_chars = profile.user_budget_chars;
    let keep_recent_chars = profile.keep_recent_chars;

    let total_chars: usize = history
        .iter()
        .map(|m| m.content.len() + m.reasoning_content.as_deref().unwrap_or("").len())
        .sum();
    if total_chars <= max_chars {
        return Ok(history);
    }

    let n = history.len();
    // 最近保留起点：从末尾往前累计字符直到预算，并向前扩展避开孤立的 tool 消息
    let mut keep_chars = 0usize;
    let mut keep_start = n;
    while keep_start > 0 && keep_chars < keep_recent_chars {
        keep_start -= 1;
        keep_chars += history[keep_start].content.len();
    }
    while keep_start > 0 && history[keep_start].role == "tool" {
        keep_start -= 1;
    }

    let mut selected: Vec<ChatMessage> = Vec::new();
    let mut dropped: Vec<ChatMessage> = Vec::new();
    let mut budget = user_budget_chars;

    for (i, m) in history.iter().enumerate() {
        if i >= keep_start {
            selected.push(m.clone());
            continue;
        }
        if m.role == "user" && !m.content.is_empty() {
            let len = m.content.len();
            if len <= budget {
                selected.push(m.clone());
                budget -= len;
            } else if budget > 0 {
                let mut c = m.clone();
                c.content = m.content.chars().take(budget).collect();
                selected.push(c);
                budget = 0;
            } else {
                dropped.push(m.clone());
            }
        } else {
            dropped.push(m.clone());
        }
    }

    if !dropped.is_empty() {
        // ★ 完整对话快照备份：压缩前把完整历史存为一条缓存记录（可搜索、可取回）
        let snapshot = serde_json::to_string(&history).map_err(|e| e.to_string())?;
        let _ = db.cache_add(conv_id, "完整对话快照（压缩时自动备份）", &snapshot);
        // 被压缩掉的原文单独存一份（便于按需取回）
        let data = serde_json::to_string(&dropped).map_err(|e| e.to_string())?;
        let _ = db.cache_add(conv_id, "上下文压缩保存的原始消息", &data);
        let summary = local_summarize(&dropped);
        selected.push(ChatMessage {
            role: "assistant".into(),
            content: summary,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
        // ★ Codex 方式：压缩视图持久化到 DB（替换旧消息），后续直接基于新窗口追加，
        //   不再每轮重新计算压缩 → 压缩后前缀稳定，缓存命中率不受影响
        db.replace_messages(conv_id, &selected)?;
    }

    Ok(selected)
}

/// 完整错误链（reqwest 的 source 里有具体原因：timeout/dns/连接被拒等）
fn full_error(e: &reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = e.source();
    let mut n = 0;
    while let Some(s) = src {
        if n >= 4 {
            break;
        }
        parts.push(s.to_string());
        src = s.source();
        n += 1;
    }
    parts.join(" | ")
}

/// 工具结果截断：约 40k tokens（120000 字符；比 Codex 的 truncation_policy 更宽，
/// 仅在输出极大时截断，保留足够上下文供模型分析）
const MAX_TOOL_RESULT_CHARS: usize = 120_000;

fn truncate_tool_result(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > MAX_TOOL_RESULT_CHARS {
        let cut: String = chars.iter().take(MAX_TOOL_RESULT_CHARS).collect();
        format!(
            "{cut}\n\n⚠️ 输出已截断（原始 {} 字符），如需完整内容请针对性读取/搜索",
            chars.len()
        )
    } else {
        text.to_string()
    }
}

/// 修复消息序列：assistant 带 tool_calls 时，后面必须为每个 tool_call 补齐 tool 消息
/// （防止中断/异常导致的历史残缺，DeepSeek API 严格要求一一对应）
fn fix_tool_sequence(msgs: &mut Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut patches: Vec<ChatMessage> = Vec::new();
    let mut i = 0;
    while i < msgs.len() {
        let need_fix = msgs[i].role == "assistant" && msgs[i].tool_calls.is_some();
        if need_fix {
            let tcs = msgs[i].tool_calls.clone().unwrap_or_default();
            let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut j = i + 1;
            // 收集紧随其后的 tool 消息（连续的 tool 块）
            while j < msgs.len() && msgs[j].role == "tool" {
                if let Some(id) = &msgs[j].tool_call_id {
                    answered.insert(id.clone());
                }
                j += 1;
            }
            let mut insert_at = j;
            for tc in &tcs {
                if !answered.contains(&tc.id) {
                    let patch = ChatMessage::tool_result(
                        &tc.id,
                        &tc.function.name,
                        "（该工具调用未完成，可能被用户中断）",
                    );
                    msgs.insert(insert_at, patch.clone());
                    patches.push(patch);
                    insert_at += 1;
                }
            }
            i = insert_at;
            continue;
        }
        i += 1;
    }
    // tool 后直接接 user 是非法的（API 要求 tool 后必须紧跟 assistant 回复）。
    // 场景：工具执行后被用户中断，然后用户直接发了新消息 → [assistant(tc), tool, user]。
    // 在 tool 与 user 之间插入一条空 assistant 占位，保持序列合法。
    let mut k = 0;
    while k + 1 < msgs.len() {
        if msgs[k].role == "tool" && msgs[k + 1].role == "user" {
            let patch = ChatMessage {
                role: "assistant".into(),
                content: "[工具执行完成]".into(),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            };
            msgs.insert(k + 1, patch.clone());
            patches.push(patch);
            k += 2;
        } else {
            k += 1;
        }
    }
    patches
}

/// 安全追加 system 消息：若末尾是 tool（不能直接跟 system），先补空 assistant 占位
fn push_tail_message(msgs: &mut Vec<ChatMessage>, msg: ChatMessage) {
    if msgs.last().map(|m| m.role == "tool").unwrap_or(false) {
        msgs.push(ChatMessage {
            role: "assistant".into(),
            content: "[工具执行完成]".into(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }
    msgs.push(msg);
}

/// 发起一轮 agent 对话
/// - user_input = Some(text)：以用户身份追加消息后进入循环（普通对话 / 任务设计对话框）
/// - user_input = None：恢复模式（resume），不追加用户消息，注入「继续执行」提示后直接进入循环
/// - plan_only = true：任务设计模式，工具限制为任务图 + 只读，AI 只调整任务图、不执行，
///   调整完成后停止等待用户确认
pub async fn run_agent_turn(
    app: AppHandle,
    db: &Db,
    cmd_registry: &Arc<CmdRegistry>,
    taskmaps: &Arc<TaskMapStore>,
    state: &Arc<AgentState>,
    provider: ProviderConfig,
    conv_id: String,
    user_input: Option<String>,
    plan_only: bool,
) -> Result<(), String> {
    if provider.api_key.is_empty() {
        return Err("请先在设置中配置 API Key".into());
    }
    let mut provider = provider;
    let conv = db
        .get_conversation(&conv_id)?
        .ok_or("会话不存在")?;

    // ★ 任务图内存恢复：应用重启后 store 为空，须从 DB 加载已保存的任务图，
    //   否则 has_map 误判为 false → 工程模式只开放 plan 工具（AI 看不到文件工具）、
    //   且 plan_init 会新建空图覆盖 DB 里的旧图（数据丢失）。
    {
        let mut store = taskmaps.lock().unwrap();
        if !store.contains_key(&conv_id) {
            if let Ok(Some(json)) = db.taskmap_load(&conv_id) {
                if let Ok(data) = serde_json::from_str::<TaskMapData>(&json) {
                    store.insert(conv_id.clone(), TaskMap::from_data(data));
                }
            }
        }
    }
    // 会话级设置覆盖：模型 / 思考强度
    if !conv.model.is_empty() {
        provider.model = conv.model.clone();
    }
    if !conv.reasoning_effort.is_empty() {
        provider.reasoning_effort = Some(conv.reasoning_effort.clone());
    }
    let work_dir = if conv.work_dir.is_empty() {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        conv.work_dir.clone()
    };

    // 获取（或创建）本会话的运行状态；不同会话互不干扰
    let session = {
        let mut sessions = state.sessions.lock().unwrap();
        sessions
            .entry(conv_id.clone())
            .or_insert_with(|| Arc::new(AgentSession::default()))
            .clone()
    };
    session.stop_flag.store(false, Ordering::Relaxed);
    if let Some(text) = &user_input {
        db.append_messages(&conv_id, &[ChatMessage::user(text)])?;
    }

    let mut turn = 0;
    loop {
        turn += 1;
        if session.stop_flag.load(Ordering::Relaxed) {
            break;
        }

        // ★ 本轮开始前的任务图快照（计划确认被拒绝时回滚用）
        let pre_turn_map = taskmaps.lock().unwrap().get(&conv_id).cloned();

        // 1) 组装消息：固定 system + 完整历史（追加式；超限时压缩早期内容）
        let history = compact_history_if_needed(db, &conv_id, db.load_messages(&conv_id)?)?;
        let mut api_messages: Vec<ChatMessage> = Vec::with_capacity(history.len() + 1);

        // 工程模式：无任务图时只开放 plan_init；有图时注入任务图状态
        // ★ system 第一条必须固定（任务图状态改由末尾注入，避免每次变化导致全量缓存失效）
        let has_map = taskmaps.lock().unwrap().contains_key(&conv_id);
        // ★ 工程模式机制始终生效（会话的 engineering_mode 开关只控制前端任务图显示，
        //   不影响 AI 的计划流程与执行纪律）
        api_messages.push(ChatMessage {
            role: "system".into(),
            content: build_system_prompt(true, None),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
        api_messages.extend(history.iter().cloned());

        // 预算提醒：上下文使用接近预算时注入收敛指令（Codex token_budget 机制）
        let used_chars: usize = api_messages
            .iter()
            .map(|m| m.content.len() + m.reasoning_content.as_deref().unwrap_or("").len())
            .sum();
        let profile = context_profile(db);
        if used_chars > profile.budget_chars * 85 / 100 {
            // ★ 预算提醒改为末尾注入（去重），绝不修改 system 第一条（否则 system 变化导致全量缓存失效）
            let budget_marker = "【上下文预算提醒】";
            api_messages.retain(|m| !(m.role == "user" && m.content.starts_with(budget_marker)));
            push_tail_message(&mut api_messages, ChatMessage {
                role: "user".into(),
                content: format!("{budget_marker}（系统注入，非用户消息）上下文使用已接近预算上限，请立即基于已有信息给出最终总结，不要再调用任何工具或读取新文件。"),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }

        // 修复历史中可能存在的残缺工具消息序列（补丁同时写回 DB，持久修复）
        let seq_patches = fix_tool_sequence(&mut api_messages);
        if !seq_patches.is_empty() {
            let _ = db.append_messages(&conv_id, &seq_patches);
        }

        // ★ 空消息防护：带 tool_calls 的空 assistant 给中性占位（避免模型把空消息当作用户输入）
        for m in api_messages.iter_mut() {
            if m.role == "assistant" && m.content.trim().is_empty() && m.tool_calls.is_some() {
                m.content = "[调用工具]".into();
            }
        }

        // ★ 任务图注入（参考旧版 harness 强制机制，全部追加到末尾，缓存友好）
        if has_map {
            // 用户是否刚修改过任务图（前端 sync_memory 置位）→ 注入重规划提示
            let user_modified = {
                let mut store = taskmaps.lock().unwrap();
                if let Some(tm) = store.get_mut(&conv_id) {
                    let m = tm.user_modified;
                    tm.user_modified = false;
                    m
                } else {
                    false
                }
            };
            // 3a) 每轮注入最新任务图状态（精简版；去重后追加）
            let state_marker = "【任务图（最新状态）】";
            api_messages.retain(|m| !(m.role == "user" && m.content.starts_with(state_marker)));
            let review = taskmaps.lock().unwrap().get(&conv_id).map(|tm| tm.review_summary_compact()).unwrap_or_default();
            push_tail_message(&mut api_messages, ChatMessage {
                role: "user".into(),
                content: format!("（系统注入，非用户消息）{state_marker}\n{review}"),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
            // 3b) 用户刚修改了任务图 → 强制重规划提示
            if user_modified {
                let mod_marker = "【任务图已变更】";
                api_messages.retain(|m| !(m.role == "user" && m.content.starts_with(mod_marker)));
                push_tail_message(&mut api_messages, ChatMessage {
                    role: "user".into(),
                    content: format!("{mod_marker}（这是系统自动注入的提示，不是用户消息）用户刚在任务图上做了修改（增删任务/改状态/调依赖/改需求）。请先 plan_review 审视变更后的计划，必要时用 plan_requirement / plan_breakdown / plan_link 调整任务图，然后严格按新图继续执行。"),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            // 3c) 用户新提问（第一轮）引导纳入任务图（resume 时跳过，由「继续执行」提示接管）
            if turn == 1 && user_input.is_some() {
                let session_marker = "【任务图会话检查】";
                api_messages.retain(|m| !(m.role == "user" && m.content.starts_with(session_marker)));
                push_tail_message(&mut api_messages, ChatMessage {
                    role: "user".into(),
                    content: format!("{session_marker}（这是系统自动注入的提示，不是用户消息）用户发起了新的提问。先 plan_review 查看既有任务图：如果用户是在原本要求上的延续或调整，就在原有任务架构上调整（plan_breakdown 挂到对应任务下补充子任务、plan_update 更新状态、plan_link 调整顺序），不要新建一级任务；只有当新要求与现有任务差异很大、属于新的独立目标时，才用 plan_breakdown（parent_id 省略或传 root）在任务目标总节点（root）下新建一级任务（任务总结），再按计划推进。"),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
        }

        // 3d) 模式提示注入（resume / 任务设计规划模式）：追加到末尾，保持 system 第一条固定
        if user_input.is_none() {
            let resume_marker = "【继续执行】";
            api_messages.retain(|m| !(m.role == "user" && m.content.starts_with(resume_marker)));
            push_tail_message(&mut api_messages, ChatMessage {
                role: "user".into(),
                content: format!("{resume_marker}（这是系统自动注入的提示，不是用户消息）用户点击了「继续」。请从上次中断的位置继续执行当前任务图：先 plan_review 查看最新任务图状态，继续推进尚未完成的任务，不要重新规划、不要重复已完成的工作。"),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        if plan_only {
            let plan_marker = "【任务设计模式】";
            api_messages.retain(|m| !(m.role == "user" && m.content.starts_with(plan_marker)));
            push_tail_message(&mut api_messages, ChatMessage {
                role: "user".into(),
                content: format!("{plan_marker}（这是系统自动注入的提示，不是用户消息）最新用户消息是用户通过任务图对话框发送的「任务设计/创建指令」（以【任务图对话框】开头）。这是高优先级的任务规划需求，请重点处理，不要当作普通对话随意回复。要求：\n1. 先把该指令作为当前唯一要处理的目标：无任务图时用 plan_init 创建任务图（任务目标总节点 → 一级=任务总结 → 二级=方法步骤 → 三级=细分）；有任务图时先 plan_review 审视现状，再按指令调整任务结构（增删/层级/顺序）；\n2. 只允许使用任务图工具（plan_*）和只读探索工具；不要执行任何文件写入、命令执行等有副作用的操作（此类工具当前不可用）；\n3. 调整完成后用文字简要说明做了哪些调整、当前计划如何，然后停止，等待用户确认；\n4. 绝对不要开始执行任务（用户同意后才会继续）；\n5. 遵循层级语义：任务目标节点（root）是唯一总节点、高于一级；一级任务=任务总结，挂在 root 下；二级=实施方法步骤；三级=进一步细分，细分到可执行即可、不要过度细分；用户延续原需求就在原架构上调整，差异大的新需求也作为新一级任务挂到 root 下，不再建独立任务线；\n6. 任务编号=父任务自身编号段+本级字母+本级数字（如 a1 → a1b2 → b2c1），同一字母层级数字全局不重复；调整后引用任务一律用编号（ID）。"),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }

        // ★ 工具列表：任务图工具始终提供（AI 可主动建图/更新）
        // 规划模式（plan_only）或 尚无任务图：只开放 plan 工具 + 只读探索工具
        // （写文件/执行命令等有副作用工具等建图后由执行焦点校验管控；只读工具无副作用，
        //   允许 AI 先了解项目再规划，避免出现"没有文件系统工具"的尴尬。
        //   注意：工程模式机制始终生效，不依赖会话 engineering_mode 开关）
        const READONLY_TOOLS: &[&str] = &[
            "list_dir", "read_file", "read_file_segment", "search_files",
            "grep_search", "glob_search", "get_file_info", "project_info",
            "diff_file", "git_status", "git_diff", "git_log", "git_branch",
            "search_web", "fetch_webpage", "check_command",
            "search_conversation_history", "get_context_remaining",
        ];
        let all_tools = {
            let base = tools::tool_definitions();
            let plan = tools::taskmap_tools();
            let mut merged = base.as_array().cloned().unwrap_or_default();
            if let Some(p) = plan.as_array() {
                merged.extend(p.iter().cloned());
            }
            serde_json::Value::Array(merged)
        };
        let tools: Value = if plan_only || !has_map {
            let plan = tools::taskmap_tools();
            let mut merged = plan.as_array().cloned().unwrap_or_default();
            if let Some(base) = tools::tool_definitions().as_array() {
                for t in base {
                    let name = t
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if READONLY_TOOLS.contains(&name) {
                        merged.push(t.clone());
                    }
                }
            }
            serde_json::Value::Array(merged)
        } else {
            all_tools
        };

        // 2) 流式请求
        let (content, reasoning, tool_calls, usage) = match stream_request(
            &app,
            &provider,
            &api_messages,
            &tools,
            &session.stop_flag,
            &conv_id,
        )
        .await
        {

            Ok(r) => r,
            Err(e) => {
                // 停止不算错误
                if session.stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let _ = app.emit("stream-error", json!({"error": e, "convId": conv_id}));
                return Err(e);
            }
        };

        // ★ 每轮请求的 usage 都累计（含真实数据日志，用于排查缓存命中率）
        if let Some(u) = &usage {
            let hit = u
                .get("prompt_cache_hit_tokens")
                .and_then(|v| v.as_i64())
                .or_else(|| {
                    u.get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_i64())
                })
                .unwrap_or(0);
            let miss = u
                .get("prompt_cache_miss_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if hit > 0 || miss > 0 {
                let mut stats = session.cache_stats.lock().unwrap();
                stats.hit += hit;
                stats.miss += miss;
            }
            if let Ok(dir) = app.path().app_data_dir() {
                let path = dir.join("request_fingerprints.log");
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    use std::io::Write;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let _ = writeln!(
                        f,
                        "USAGE t={} hit={} miss={} total={}",
                        now,
                        hit,
                        miss,
                        hit + miss
                    );
                }
            }
        }

        // 3) 追加 assistant 消息（含 tool_calls）
        let assistant_msg = ChatMessage {
            role: "assistant".into(),
            content,
            reasoning_content: if reasoning.is_empty() { None } else { Some(reasoning) },
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls.clone()) },
            tool_call_id: None,
            name: None,
        };
        db.append_messages(&conv_id, &[assistant_msg])?;

        if tool_calls.is_empty() {
            break;
        }

        // 4) 先通知前端本轮工具调用（创建卡片），再逐个执行
        let _ = app.emit(
            "agent-round-end",
            json!({"round": turn, "toolCalls": tool_calls, "convId": conv_id}),
        );
        let mut results: Vec<ChatMessage> = Vec::new();
        let mut executed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for tc in &tool_calls {
            if session.stop_flag.load(Ordering::Relaxed) {
                break;
            }
            let tool_name = &tc.function.name;
            let args_text = &tc.function.arguments;
            let desc = format!("{tool_name} {args_text}");

            // 授权（四档模式：ask / smart / allow_all / none）
            let allowed = match state.auth_mode.load(Ordering::Relaxed) {
                // 无监管：所有请求都不需要确认
                AUTH_NONE => true,
                AUTH_ALLOW_ALL => true,
                AUTH_SMART if is_safe_tool(tool_name) => true,
                // smart 模式下 run_command 走只读命令白名单
                AUTH_SMART if tool_name == "run_command" => {
                    let safe = serde_json::from_str::<Value>(args_text)
                        .ok()
                        .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(String::from))
                        .map(|c| is_safe_command_text(&c))
                        .unwrap_or(false);
                    if safe {
                        true
                    } else {
                        // 不安全命令仍需授权
                        let request_id = uuid::Uuid::new_v4().to_string();
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        session.pending_permissions.lock().unwrap().insert(request_id.clone(), tx);
                        let _ = app.emit("permission-request", PermissionRequest {
                            request_id: request_id.clone(),
                            tool_name: tool_name.clone(),
                            description: desc.clone(),
                            conv_id: conv_id.clone(),
                        });
                        let allow = tokio::select! {
                            r = rx => r.unwrap_or(false),
                            _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {
                                session.pending_permissions.lock().unwrap().remove(&request_id);
                                false
                            }
                        };
                        if session.stop_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        allow
                    }
                }
                _ => {
                let request_id = uuid::Uuid::new_v4().to_string();
                let (tx, rx) = tokio::sync::oneshot::channel();
                session
                    .pending_permissions
                    .lock()
                    .unwrap()
                    .insert(request_id.clone(), tx);
                let _ = app.emit(
                    "permission-request",
                    PermissionRequest {
                        request_id: request_id.clone(),
                        tool_name: tool_name.clone(),
                        description: desc.clone(),
                        conv_id: conv_id.clone(),
                    },
                );
                // 等待用户响应；停止时也解除等待
                let allow = tokio::select! {
                    r = rx => r.unwrap_or(false),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {
                        // 超时：从等待表移除自己，避免残留
                        session.pending_permissions.lock().unwrap().remove(&request_id);
                        false
                    }
                };
                if session.stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                allow
                }
            };

            let (result, ok): (String, bool) = if !allowed {
                (format!("用户拒绝了该工具调用（{desc}）"), false)
            } else if tool_name == "get_context_remaining" {
                // 上下文感知工具：估算当前使用量与剩余空间
                let used_chars: usize = api_messages
                    .iter()
                    .map(|m| m.content.len() + m.reasoning_content.as_deref().unwrap_or("").len())
                    .sum();
                let used_tokens = used_chars / 3;
                let profile = context_profile(db);
                let budget_tokens = profile.budget_tokens;
                let remaining = budget_tokens.saturating_sub(used_tokens);
                (
                    format!(
                        "当前上下文约使用 {} tokens（预算约 {} tokens），剩余约 {} tokens。\n建议：剩余充足可继续读取文件；剩余不足时应优先总结已有信息。",
                        used_tokens, budget_tokens, remaining
                    ),
                    true,
                )
            } else {
                // 解析参数并执行
                let args: Value = serde_json::from_str(args_text).unwrap_or_else(|_| json!({}));
                match tools::execute_tool(tool_name, &args, &work_dir, db, cmd_registry, taskmaps, &conv_id).await {
                    Ok(text) => (truncate_tool_result(&text), true),
                    Err(e) => (format!("工具执行失败: {e}"), false),
                }
            };

            let _ = app.emit(
                "tool-result",
                ToolResultEvent {
                    tool_name: tool_name.clone(),
                    ok,
                    rejected: !allowed,
                    message: result.clone(),
                    conv_id: conv_id.clone(),
                },
            );
            results.push(ChatMessage::tool_result(&tc.id, tool_name, result));
            executed_ids.insert(tc.id.clone());
        }
        // ★ 中断兜底：为所有未执行的 tool_call 补齐 tool 消息，保证历史序列合法
        for tc in &tool_calls {
            if !executed_ids.contains(&tc.id) {
                results.push(ChatMessage::tool_result(
                    &tc.id,
                    &tc.function.name,
                    "（用户中断了任务，该工具调用未执行）",
                ));
            }
        }
        if !results.is_empty() {
            db.append_messages(&conv_id, &results)?;
        }

        // ★ 计划确认：非任务设计模式（plan_only）且非无监管模式（AUTH_NONE）下，
        //   AI 本轮若修改了任务图结构（plan_init/plan_breakdown/plan_link/plan_move/
        //   plan_delete/plan_requirement），暂停循环等待用户确认——创建/更改任务结构
        //   必须用户确认才继续执行（与 ask/smart/allow_all 授权模式无关）；
        //   plan_update 仅更新进度/状态，不触发确认；无监管模式下所有请求都不确认。
        if !plan_only && state.auth_mode.load(Ordering::Relaxed) != AUTH_NONE {
            let struct_touched = tool_calls
                .iter()
                .any(|tc| STRUCT_TOOLS.contains(&tc.function.name.as_str()));
            if struct_touched {
                // 发送确认事件（含当前任务图摘要与是否新建）
                let (has_map, summary) = {
                    let store = taskmaps.lock().unwrap();
                    let has = store.contains_key(&conv_id);
                    let s = store
                        .get(&conv_id)
                        .map(|tm| tm.review_summary_compact())
                        .unwrap_or_default();
                    (has, s)
                };
                let _ = app.emit(
                    "plan-confirm",
                    json!({"convId": conv_id, "hasMap": has_map, "summary": summary}),
                );
                // 等待用户确认（超时视为拒绝；打断时由 stop_agent 解除等待并视为拒绝）
                let (tx, rx) = tokio::sync::oneshot::channel();
                session.pending_plan_confirm.lock().unwrap().replace(tx);
                let allowed = tokio::select! {
                    r = rx => r.unwrap_or(false),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {
                        session.pending_plan_confirm.lock().unwrap().take();
                        false
                    }
                };
                // 通知前端确认结果
                let _ = app.emit(
                    "plan-confirm-result",
                    json!({"convId": conv_id, "allowed": allowed}),
                );
                if !allowed {
                    // 拒绝：回滚本轮任务图修改（恢复到本轮开始前状态），并结束循环
                    let mut store = taskmaps.lock().unwrap();
                    match &pre_turn_map {
                        Some(tm) => { store.insert(conv_id.clone(), tm.clone()); }
                        None => { store.remove(&conv_id); }
                    }
                    drop(store);
                    break;
                }
            }
        }

        // 5) 若本轮被停止，结束循环
        if session.stop_flag.load(Ordering::Relaxed) {
            break;
        }
    }

    // 任务图内存状态同步到 DB（前端也可读写）
    {
        let store = taskmaps.lock().unwrap();
        if let Some(tm) = store.get(&conv_id) {
            let _ = db.taskmap_save(&conv_id, &serde_json::to_string(&tm.data).unwrap_or_default());
        }
    }

    // 缓存命中统计事件通知（每轮已在循环内累计；这里只发送）
    {
        let stats = session.cache_stats.lock().unwrap();
        let total = stats.hit + stats.miss;
        if total > 0 {
            let rate = stats.hit as f64 * 100.0 / total as f64;
            let _ = app.emit(
                "cache-stats",
                json!({"hit": stats.hit, "miss": stats.miss, "rate": rate, "convId": conv_id}),
            );
        }
    }

    let _ = app.emit(
        "stream-chunk",
        json!({"delta": "", "reasoning": Value::Null, "usage": Value::Null, "done": true, "convId": conv_id}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ToolCall;

    #[test]
    fn fix_missing_tool_results() {
        let tc = ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: super::super::types::ToolCallFunction {
                name: "list_dir".into(),
                arguments: "{}".into(),
            },
        };
        let mut msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                reasoning_content: None,
                tool_calls: Some(vec![tc.clone()]),
                tool_call_id: None,
                name: None,
            },
            // 缺少对应的 tool 消息（模拟中断）
        ];
        let _ = fix_tool_sequence(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].role, "tool");
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_1"));
        assert!(msgs[2].content.contains("未完成"));
    }

    #[test]
    fn keeps_valid_sequence() {
        let tc = ToolCall {
            id: "call_2".into(),
            call_type: "function".into(),
            function: super::super::types::ToolCallFunction {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        };
        let mut msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                reasoning_content: None,
                tool_calls: Some(vec![tc.clone()]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage::tool_result("call_2", "read_file", "ok"),
        ];
        let _ = fix_tool_sequence(&mut msgs);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn fixes_tool_then_user_sequence() {
        // 工具结果后直接跟用户新消息（中断后继续对话）→ 插入 assistant 占位
        let mut msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                reasoning_content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    call_type: "function".into(),
                    function: super::super::types::ToolCallFunction {
                        name: "list_dir".into(),
                        arguments: "{}".into(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage::tool_result("call_1", "list_dir", "ok"),
            ChatMessage::user("继续"),
        ];
        let _ = fix_tool_sequence(&mut msgs);
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[3].role, "assistant");
        assert_eq!(msgs[3].tool_calls, None);
        assert_eq!(msgs[4].role, "user");
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn truncate_long_output() {
        let long = "x".repeat(150_000);
        let cut = truncate_tool_result(&long);
        assert!(cut.contains("输出已截断"));
        assert!(cut.chars().count() < 120_500);
    }

    #[test]
    fn keeps_short_output() {
        let short = "hello";
        assert_eq!(truncate_tool_result(short), short);
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    #[test]
    fn summarize_extracts_progress() {
        let dropped = vec![
            ChatMessage::user("帮我优化这个项目"),
            ChatMessage {
                role: "assistant".into(),
                content: "我发现 main.py 有内存泄漏。".into(),
                reasoning_content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: super::super::types::ToolCallFunction {
                        name: "read_file".into(),
                        arguments: "{\"path\":\"main.py\"}".into(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage::tool_result("c1", "read_file", "内容..."),
        ];
        let summary = local_summarize(&dropped);
        assert!(summary.contains("内存泄漏"), "应提取结论: {summary}");
        assert!(summary.contains("read_file"), "应包含工具名: {summary}");
        assert!(summary.contains("main.py"), "应包含文件路径: {summary}");
        assert!(summary.contains("不要再检索"), "应劝阻反复检索: {summary}");
    }

    #[test]
    fn summarize_empty_is_safe() {
        let summary = local_summarize(&[]);
        assert!(summary.contains("进展如下"));
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    use crate::core::db::Db;
    use std::path::PathBuf;

    fn test_db() -> Db {
        let dir = std::env::temp_dir().join(format!("canlow-archive-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        db
    }

    #[test]
    fn compression_creates_snapshot_and_searchable_archive() {
        let db = test_db();
        let conv = db.create_conversation("t", "", "", "", "high").unwrap();
        // 造一条超长历史触发压缩
        let mut history = vec![
            ChatMessage::user("帮我分析 main.py 的性能问题"),
            ChatMessage::assistant("我找到了内存泄漏点。"),
        ];
        let filler = "这是一个用于撑大上下文的重复填充文本，包含足够多的字符。".repeat(2000); // ~4.4 万字符
        for i in 0..60 {
            history.push(ChatMessage {
                role: "assistant".into(),
                content: format!("第 {} 轮：{}", i, filler),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        db.append_messages(&conv.id, &history).unwrap();
        let loaded = db.load_messages(&conv.id).unwrap();
        let compacted = compact_history_if_needed(&db, &conv.id, loaded).unwrap();
        assert!(compacted.len() < history.len(), "应发生压缩");
        // 完整快照应已入库（可搜索）
        let hits = db.cache_search(&conv.id, "内存泄漏", 5).unwrap();
        assert!(!hits.is_empty(), "归档应可搜到旧内容: {:?}", hits);
        // 摘要应包含关键信息且带约束
        let has_summary = compacted.iter().any(|m| m.content.contains("[上下文摘要]"));
        assert!(has_summary);
        // ★ Codex 式持久化：压缩后 DB 里就是压缩视图，再次加载与压缩结果一致
        let reloaded = db.load_messages(&conv.id).unwrap();
        assert_eq!(reloaded.len(), compacted.len(), "DB 应持久化压缩视图");
        // 再次压缩不应重复触发（视图已小于阈值）
        let before = db.setting_get("cache_count_probe").ok();
        let _ = before;
        let count_before = db.cache_search(&conv.id, "填充文本", 100).unwrap().len();
        let again = compact_history_if_needed(&db, &conv.id, reloaded).unwrap();
        assert_eq!(again.len(), compacted.len(), "压缩视图不应再次被压缩");
        let count_after = db.cache_search(&conv.id, "填充文本", 100).unwrap().len();
        assert!(count_after >= count_before, "不应产生新的重复归档");
    }
}

#[cfg(test)]
mod auth_mode_tests {
    use super::*;

    #[test]
    fn safe_tools_auto_approved() {
        for t in ["list_dir", "read_file", "grep_search", "git_status", "project_info", "fetch_webpage", "search_web", "check_command", "get_context_remaining"] {
            assert!(is_safe_tool(t), "{t} 应为只读安全工具");
        }
        // 任务图工具：无文件副作用，工程模式高频调用，应自动执行（plan_delete 除外）
        for t in ["plan_init", "plan_breakdown", "plan_update", "plan_link", "plan_review", "plan_requirement", "plan_move", "plan_export", "plan_find", "plan_undo", "plan_focus"] {
            assert!(is_safe_tool(t), "{t} 应自动执行");
        }
    }

    #[test]
    fn dangerous_tools_require_approval() {
        for t in ["write_file", "replace_in_file", "delete_file", "run_command", "terminate_command", "git_commit", "todo_create", "plan_delete"] {
            assert!(!is_safe_tool(t), "{t} 应需要授权");
        }
    }
}

#[cfg(test)]
mod safe_command_tests {
    use super::*;

    #[test]
    fn safe_readonly_commands_auto() {
        assert!(is_safe_command_text("ls -la"));
        assert!(is_safe_command_text("cat main.py"));
        assert!(is_safe_command_text("pwd"));
        assert!(is_safe_command_text("grep TODO src/"));
        assert!(is_safe_command_text("git status"));
        assert!(is_safe_command_text("git diff"));
        assert!(is_safe_command_text("git log --oneline -5"));
        assert!(is_safe_command_text("ls | grep py && head -5 x.py"));
        assert!(is_safe_command_text("find . -name '*.rs'"));
        assert!(is_safe_command_text("wc -l file.txt"));
    }

    #[test]
    fn dangerous_commands_require_approval() {
        assert!(!is_safe_command_text("rm -rf /"));
        assert!(!is_safe_command_text("echo x > file.txt"));
        assert!(!is_safe_command_text("python3 script.py"));
        assert!(!is_safe_command_text("git push"));
        assert!(!is_safe_command_text("sed -i s/a/b/ file"));
        assert!(!is_safe_command_text("find . -delete"));
        assert!(!is_safe_command_text("base64 -o out.txt"));
        assert!(!is_safe_command_text("cat $(ls)"));
        assert!(!is_safe_command_text("touch newfile"));
    }
}

#[cfg(test)]
mod compact_sequence_tests {
    use super::*;
    use crate::core::db::Db;

    fn test_db() -> Db {
        let dir = std::env::temp_dir().join(format!("canlow-seq-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        db
    }

    fn tc(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: super::super::types::ToolCallFunction {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    /// 压缩后序列合法性：
    /// 1) 任何 assistant(tool_calls) 的每个 id 在紧随的 tool 块中都有结果
    /// 2) 任何 tool 消息都能回溯到所属 assistant（带对应 tool_calls）
    fn assert_sequence_valid(msgs: &[ChatMessage]) {
        let mut i = 0;
        while i < msgs.len() {
            if msgs[i].role == "assistant" {
                if let Some(tcs) = &msgs[i].tool_calls {
                    let ids: std::collections::HashSet<String> =
                        tcs.iter().map(|t| t.id.clone()).collect();
                    let mut j = i + 1;
                    let mut answered = std::collections::HashSet::new();
                    while j < msgs.len() && msgs[j].role == "tool" {
                        if let Some(id) = &msgs[j].tool_call_id {
                            answered.insert(id.clone());
                        }
                        j += 1;
                    }
                    for id in &ids {
                        assert!(answered.contains(id), "tool_call {id} 缺少结果");
                    }
                }
            }
            i += 1;
        }
        // 反向检查：每个 tool 消息都能找到所属 assistant（最近的 assistant 带 tool_calls 且包含该 id）
        let mut last_assistant: Option<&ChatMessage> = None;
        for m in msgs {
            if m.role == "assistant" {
                last_assistant = Some(m);
            } else if m.role == "tool" {
                let asst = last_assistant.expect("tool 消息前必须有 assistant");
                let tcs = asst.tool_calls.as_ref().expect("所属 assistant 必须带 tool_calls");
                if let Some(id) = &m.tool_call_id {
                    assert!(
                        tcs.iter().any(|t| &t.id == id),
                        "tool_call_id {id} 不属于前置 assistant"
                    );
                }
            }
        }
    }

    #[test]
    fn compact_keeps_tool_sequence_complete() {
        let db = test_db();
        let conv = db.create_conversation("t", "", "", "", "256k").unwrap();
        let filler = "填充文本用于撑大上下文。".repeat(1500); // ~1.2 万字符
        let mut history = vec![ChatMessage::user("帮我做项目")];
        // 20 轮工具调用（assistant + 2 个 tool 结果），每轮末尾带大内容
        for i in 0..20 {
            let id1 = format!("c{i}_a");
            let id2 = format!("c{i}_b");
            history.push(ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                reasoning_content: Some("思考".into()),
                tool_calls: Some(vec![tc(&id1, "read_file"), tc(&id2, "grep_search")]),
                tool_call_id: None,
                name: None,
            });
            history.push(ChatMessage::tool_result(&id1, "read_file", &filler));
            history.push(ChatMessage::tool_result(&id2, "grep_search", "匹配: x"));
        }
        history.push(ChatMessage::assistant("分析完成，开始修复"));
        // 写入并强制压缩（256k 档阈值 36 万字符，历史应超过）
        db.append_messages(&conv.id, &history).unwrap();
        let loaded = db.load_messages(&conv.id).unwrap();
        let total: usize = loaded.iter().map(|m| m.content.len()).sum();
        assert!(total > 360_000, "测试历史应超过压缩阈值: {total}");
        let compacted = compact_history_if_needed(&db, &conv.id, loaded).unwrap();
        // 压缩后视图必须序列合法（含摘要 assistant 与保留消息）
        assert_sequence_valid(&compacted);
        // 压缩已持久化，重新加载也应合法
        let reloaded = db.load_messages(&conv.id).unwrap();
        assert_sequence_valid(&reloaded);
    }
}
