// Tauri 命令层（薄壳）：会话 CRUD、工具控制、agent 控制
use std::sync::Arc;
use tauri::State;

use crate::core::agent::{is_internal_injected_message, run_agent_turn, AgentState};
use crate::core::db::Db;
use crate::core::providers::{all_providers_with_overrides, resolve_provider_config, ProviderDef};
use crate::core::taskmap::{TaskMap, TaskMapData, TaskMapStore};
use crate::core::tools::CmdRegistry;
use crate::core::types::{ChatMessage, ConversationMeta};
use serde_json::{json, Value};

// ---------- 会话 ----------

#[tauri::command]
pub fn session_list(db: State<Arc<Db>>) -> Result<Vec<ConversationMeta>, String> {
    db.list_conversations()
}

#[tauri::command]
pub fn session_create(
    db: State<Arc<Db>>,
    title: String,
    work_dir: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
) -> Result<ConversationMeta, String> {
    db.create_conversation(
        &title,
        &work_dir.unwrap_or_default(),
        &provider.unwrap_or_else(|| "DeepSeek".into()),
        &model.unwrap_or_default(),
        &reasoning_effort.unwrap_or_else(|| "high".into()),
        true,
    )
}

#[tauri::command]
pub fn session_delete(db: State<Arc<Db>>, id: String) -> Result<(), String> {
    db.delete_conversation(&id)
}

#[tauri::command]
pub fn session_rename(db: State<Arc<Db>>, id: String, title: String) -> Result<(), String> {
    db.rename_conversation(&id, &title)
}

#[tauri::command]
pub fn session_update(
    db: State<Arc<Db>>,
    id: String,
    work_dir: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    engineering_mode: Option<bool>,
) -> Result<(), String> {
    db.update_conversation(
        &id,
        work_dir.as_deref(),
        provider.as_deref(),
        model.as_deref(),
        reasoning_effort.as_deref(),
        engineering_mode,
    )
}

#[tauri::command]
pub fn session_messages(db: State<Arc<Db>>, id: String) -> Result<Vec<ChatMessage>, String> {
    Ok(db
        .load_messages(&id)?
        .into_iter()
        .filter(|m| !is_internal_injected_message(m))
        .collect())
}

// ---------- 会话导出（Session Log） ----------

/// 毫秒时间戳 → 可读时间（无 chrono 依赖的简易格式化）
fn ts_display(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 公历日（Howard Hinnant 的 days_from_civil 逆运算）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// JSONL 会话日志（对齐 DSH session log 语义：一行一条事件，完整字段）
fn render_export_jsonl(
    conv: &ConversationMeta,
    messages: &[(i64, ChatMessage, i64)],
    taskmap: Option<&str>,
) -> String {
    let mut out = String::new();
    let header = json!({
        "type": "session",
        "version": 1,
        "id": conv.id,
        "title": conv.title,
        "provider": conv.provider,
        "model": conv.model,
        "workDir": conv.work_dir,
        "reasoningEffort": conv.reasoning_effort,
        "engineeringMode": conv.engineering_mode,
        "createdAt": conv.created_at,
        "updatedAt": conv.updated_at,
    });
    out.push_str(&header.to_string());
    out.push('\n');
    for (seq, m, created_at) in messages {
        let mut obj = json!({
            "type": "message",
            "seq": seq,
            "role": m.role,
            "content": m.content,
            "createdAt": created_at,
        });
        if let Some(rc) = &m.reasoning_content {
            obj["reasoningContent"] = json!(rc);
        }
        if let Some(tcs) = &m.tool_calls {
            obj["toolCalls"] = json!(tcs);
        }
        if let Some(tcid) = &m.tool_call_id {
            obj["toolCallId"] = json!(tcid);
        }
        if let Some(n) = &m.name {
            obj["toolName"] = json!(n);
        }
        out.push_str(&obj.to_string());
        out.push('\n');
    }
    if let Some(tm) = taskmap {
        if let Ok(v) = serde_json::from_str::<Value>(tm) {
            out.push_str(&json!({ "type": "taskmap", "data": v }).to_string());
            out.push('\n');
        }
    }
    out
}

/// Markdown 会话转写（人类可读，适合阅读/分享）
fn render_export_markdown(
    conv: &ConversationMeta,
    messages: &[(i64, ChatMessage, i64)],
    taskmap: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# 会话：{}\n\n", conv.title));
    out.push_str(&format!("- 提供商：{} / 模型：{}\n", conv.provider, conv.model));
    out.push_str(&format!("- 工作目录：{}\n", conv.work_dir));
    out.push_str(&format!(
        "- 创建：{} ｜ 更新：{}\n",
        ts_display(conv.created_at),
        ts_display(conv.updated_at)
    ));
    out.push_str("\n---\n\n");
    for (_seq, m, _created) in messages {
        match m.role.as_str() {
            "user" => {
                out.push_str(&format!("**用户**\n\n{}\n\n", m.content));
            }
            "assistant" => {
                if let Some(rc) = &m.reasoning_content {
                    if !rc.is_empty() {
                        out.push_str(&format!("> 🧠 推理：\n\n{}\n\n", rc));
                    }
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        out.push_str(&format!(
                            "**工具调用**：`{}`\n\n```json\n{}\n```\n\n",
                            tc.function.name,
                            tc.function.arguments
                        ));
                    }
                }
                if !m.content.is_empty() {
                    out.push_str(&format!("**助手**\n\n{}\n\n", m.content));
                }
            }
            "tool" => {
                out.push_str(&format!(
                    "**工具结果**（`{}`）\n\n```\n{}\n```\n\n",
                    m.name.clone().unwrap_or_default(),
                    m.content
                ));
            }
            _ => {}
        }
    }
    if let Some(tm) = taskmap {
        if let Ok(v) = serde_json::from_str::<Value>(tm) {
            out.push_str("\n---\n\n## 任务图\n\n```json\n");
            out.push_str(&serde_json::to_string_pretty(&v).unwrap_or_default());
            out.push_str("\n```\n");
        }
    }
    out
}

/// 导出会话日志：format = jsonl（默认，完整事件日志）/ markdown（人类可读转写）
/// 前端先用 save 对话框拿到保存路径，再调用本命令写入。
#[tauri::command]
pub fn session_export(
    db: State<Arc<Db>>,
    conv_id: String,
    path: String,
    format: Option<String>,
) -> Result<String, String> {
    let fmt = format.unwrap_or_else(|| "jsonl".into());
    let conv = db.get_conversation(&conv_id)?.ok_or("会话不存在")?;
    let messages: Vec<(i64, ChatMessage, i64)> = db
        .load_messages_export(&conv_id)?
        .into_iter()
        .filter(|(_, m, _)| !is_internal_injected_message(m))
        .collect();
    let taskmap = db.taskmap_load(&conv_id)?;
    let content = match fmt.as_str() {
        "markdown" => render_export_markdown(&conv, &messages, taskmap.as_deref()),
        _ => render_export_jsonl(&conv, &messages, taskmap.as_deref()),
    };
    std::fs::write(&path, content).map_err(|e| format!("写入失败: {e}"))?;
    Ok(path)
}

// ---------- 提供商 ----------

#[tauri::command]
pub fn providers_list(db: State<Arc<Db>>) -> Result<Vec<ProviderDef>, String> {
    let custom = db.setting_get("custom_providers")?;
    let models = db.setting_get("provider_models")?;
    Ok(all_providers_with_overrides(custom.as_deref(), models.as_deref()))
}

/// 设置某厂商的可用模型列表（覆盖内置默认）
#[tauri::command]
pub fn provider_set_models(db: State<Arc<Db>>, name: String, models: Vec<String>) -> Result<(), String> {
    let models: Vec<String> = models.into_iter().filter(|m| !m.trim().is_empty()).collect();
    if models.is_empty() {
        return Err("模型列表不能为空".into());
    }
    let models_json = db.setting_get("provider_models")?;
    let mut all: std::collections::HashMap<String, Vec<String>> = models_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    all.insert(name, models);
    db.setting_set("provider_models", &serde_json::to_string(&all).map_err(|e| e.to_string())?)
}

#[tauri::command]
pub fn provider_save_key(db: State<Arc<Db>>, name: String, key: String) -> Result<(), String> {
    let keys_json = db.setting_get("provider_keys")?;
    let mut keys: serde_json::Map<String, serde_json::Value> = keys_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    keys.insert(name.clone(), serde_json::Value::String(key.trim().to_string()));
    db.setting_set("provider_keys", &serde_json::Value::Object(keys).to_string())
}

/// 查询提供商 Key 配置状态：已配置返回掩码（sk-****abcd），未配置返回 None
#[tauri::command]
pub fn provider_key_status(db: State<Arc<Db>>, name: String) -> Result<Option<String>, String> {
    let keys_json = db.setting_get("provider_keys")?;
    let keys: std::collections::HashMap<String, String> = keys_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    Ok(keys.get(&name).filter(|k| !k.trim().is_empty()).map(|k| {
        let k = k.trim();
        let chars: Vec<char> = k.chars().collect();
        if chars.len() <= 8 {
            "已配置".to_string()
        } else {
            let head: String = chars.iter().take(4).collect();
            let tail: String = chars.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            format!("{head}****{tail}")
        }
    }))
}

/// 测试连接：向提供商发一个最小请求
#[tauri::command]
pub async fn provider_test(db: State<'_, Arc<Db>>, name: String) -> Result<String, String> {
    let provider = resolve_provider_config(&db, &name, "", None, None)?;
    if provider.api_key.is_empty() {
        return Err("尚未设置 API Key".into());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&serde_json::json!({
            "model": provider.model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 5,
            "stream": false
        }))
        .send()
        .await
        .map_err(|e| format!("连接失败: {e}"))?;
    if resp.status().is_success() {
        Ok(format!("✅ 连接成功（模型: {}）", provider.model))
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()))
    }
}

#[tauri::command]
pub fn custom_provider_add(
    db: State<Arc<Db>>,
    name: String,
    base_url: String,
    models: Vec<String>,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() || base_url.trim().is_empty() {
        return Err("名称和接口地址不能为空".into());
    }
    let models: Vec<String> = models.into_iter().filter(|m| !m.trim().is_empty()).collect();
    if models.is_empty() {
        return Err("至少需要一个模型".into());
    }
    let custom_json = db.setting_get("custom_providers")?;
    let mut custom: Vec<ProviderDef> = custom_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    // 同名替换
    custom.retain(|p| p.name != name);
    custom.push(ProviderDef {
        name: name.clone(),
        base_url: base_url.trim().to_string(),
        models,
        supports_thinking: true,
        context_window: 256_000,
    });
    db.setting_set(
        "custom_providers",
        &serde_json::to_string(&custom).map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
pub fn custom_provider_remove(db: State<Arc<Db>>, name: String) -> Result<(), String> {
    let custom_json = db.setting_get("custom_providers")?;
    let mut custom: Vec<ProviderDef> = custom_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    custom.retain(|p| p.name != name);
    db.setting_set(
        "custom_providers",
        &serde_json::to_string(&custom).map_err(|e| e.to_string())?,
    )
}

// ---------- 上下文档位 ----------

#[tauri::command]
pub fn context_profile_get(db: State<Arc<Db>>) -> Result<String, String> {
    Ok(db.setting_get("context_profile")?.unwrap_or_else(|| "1m".into()))
}

#[tauri::command]
pub fn context_profile_set(db: State<Arc<Db>>, profile: String) -> Result<(), String> {
    if profile != "1m" && profile != "256k" {
        return Err("档位只能是 1m 或 256k".into());
    }
    db.setting_set("context_profile", &profile)
}

// ---------- 技能管理 ----------

/// 技能列表：(名称, 描述, 来源, 路径)
#[tauri::command]
pub fn skills_list(work_dir: String) -> Result<Vec<(String, String, String, String)>, String> {
    Ok(crate::core::tools::skill_infos(&work_dir))
}

/// 创建技能（work_dir 非空写项目级，否则写用户级）；返回写入路径
#[tauri::command]
pub fn skill_create(
    work_dir: String,
    name: String,
    description: String,
    content: String,
) -> Result<String, String> {
    let (proj, user) = crate::core::tools::skill_dirs(&work_dir);
    let dir = if work_dir.trim().is_empty() { user } else { proj };
    crate::core::tools::skill_create(&dir, &name, &description, &content)
}

/// 打开技能目录（target: project / user），用系统文件管理器
#[tauri::command]
pub fn skill_open_dir(work_dir: String, target: String) -> Result<String, String> {
    let (proj, user) = crate::core::tools::skill_dirs(&work_dir);
    let dir = if target == "user" { user } else { proj };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener).arg(&dir).spawn().map_err(|e| format!("无法打开目录: {e}"))?;
    Ok(dir.to_string_lossy().to_string())
}

// ---------- 工具 ----------

// 工具执行统一走 agent 循环（agent.rs），不再暴露独立命令，
// 避免前端绕过授权直接执行工具。

// ---------- 任务图 ----------

#[tauri::command]
pub fn taskmap_get(
    db: State<Arc<Db>>,
    taskmaps: State<'_, Arc<TaskMapStore>>,
    id: String,
) -> Result<Option<serde_json::Value>, String> {
    let data = db.taskmap_load(&id)?;
    // 内存 store 未加载时顺带恢复（应用重启后 store 为空，避免 AI 侧 has_map 误判、
    // plan_* 工具读不到旧图）
    if let Some(json) = &data {
        let mut store = taskmaps.lock().unwrap();
        if !store.contains_key(&id) {
            if let Ok(d) = serde_json::from_str::<TaskMapData>(json) {
                store.insert(id.clone(), TaskMap::from_data(d));
            }
        }
    }
    Ok(data
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok()))
}

#[tauri::command]
pub fn taskmap_save(db: State<Arc<Db>>, id: String, data: serde_json::Value) -> Result<(), String> {
    db.taskmap_save(&id, &data.to_string())
}

#[tauri::command]
pub fn taskmap_delete(db: State<Arc<Db>>, id: String) -> Result<(), String> {
    db.taskmap_delete(&id)
}

/// 前端保存布局/结构后同步到内存 store（供 agent 工具读取）
/// 对比 changelog/requirement 变化，标记 user_modified（agent 下一轮注入重规划提示）
#[tauri::command]
pub fn taskmap_sync_memory(
    taskmaps: State<'_, Arc<TaskMapStore>>,
    id: String,
    data: serde_json::Value,
) -> Result<(), String> {
    let parsed: TaskMapData = serde_json::from_value(data).map_err(|e| e.to_string())?;
    let mut store = taskmaps.lock().unwrap();
    let user_modified = match store.get(&id) {
        Some(old) => {
            old.data.changelog != parsed.changelog
                || old.data.requirement != parsed.requirement
                || old.data.nodes.len() != parsed.nodes.len()
        }
        None => true,
    };
    let mut tm = TaskMap::from_data(parsed);
    tm.user_modified = user_modified;
    store.insert(id, tm);
    Ok(())
}

// ---------- Agent ----------

#[tauri::command]
pub async fn agent_turn(
    app: tauri::AppHandle,
    db: State<'_, Arc<Db>>,
    registry: State<'_, Arc<CmdRegistry>>,
    taskmaps: State<'_, Arc<TaskMapStore>>,
    agent: State<'_, Arc<AgentState>>,
    conv_id: String,
    text: String,
    plan_only: Option<bool>,
) -> Result<(), String> {
    let conv = db.get_conversation(&conv_id)?.ok_or("会话不存在")?;
    let provider = resolve_provider_config(
        &db,
        &conv.provider,
        &conv.model,
        Some(&conv.reasoning_effort),
        None,
    )?;
    run_agent_turn(
        app,
        &db,
        &registry,
        &taskmaps,
        &agent,
        provider,
        conv_id,
        Some(text),
        plan_only.unwrap_or(false),
    )
    .await
}

/// 继续被中断的 agent 进程：不追加用户消息，注入「继续执行」提示后恢复循环
#[tauri::command]
pub async fn agent_resume(
    app: tauri::AppHandle,
    db: State<'_, Arc<Db>>,
    registry: State<'_, Arc<CmdRegistry>>,
    taskmaps: State<'_, Arc<TaskMapStore>>,
    agent: State<'_, Arc<AgentState>>,
    conv_id: String,
) -> Result<(), String> {
    let conv = db.get_conversation(&conv_id)?.ok_or("会话不存在")?;
    let provider = resolve_provider_config(
        &db,
        &conv.provider,
        &conv.model,
        Some(&conv.reasoning_effort),
        None,
    )?;
    run_agent_turn(app, &db, &registry, &taskmaps, &agent, provider, conv_id, None, false).await
}

#[tauri::command]
pub fn respond_permission(
    agent: State<'_, Arc<AgentState>>,
    request_id: String,
    allow: bool,
) -> Result<(), String> {
    // request_id 全局唯一（uuid），在全部会话与子代理的等待表中查找
    let sessions = agent.sessions.lock().unwrap();
    for s in sessions.values() {
        let mut pending = s.pending_permissions.lock().unwrap();
        if let Some(tx) = pending.remove(&request_id) {
            let _ = tx.send(allow);
            return Ok(());
        }
    }
    drop(sessions);
    let subagents = agent.subagents.lock().unwrap();
    for s in subagents.values() {
        let mut pending = s.pending_permissions.lock().unwrap();
        if let Some(tx) = pending.remove(&request_id) {
            let _ = tx.send(allow);
            return Ok(());
        }
    }
    Ok(())
}

/// 响应计划确认：允许/拒绝 AI 对任务图结构的修改
/// （同意 → 继续执行；拒绝 → 回滚本轮修改并停止，与授权模式无关）
#[tauri::command]
pub fn respond_plan_confirm(
    agent: State<'_, Arc<AgentState>>,
    conv_id: String,
    allow: bool,
) -> Result<(), String> {
    let sessions = agent.sessions.lock().unwrap();
    if let Some(s) = sessions.get(&conv_id) {
        let mut pending = s.pending_plan_confirm.lock().unwrap();
        if let Some(tx) = pending.take() {
            let _ = tx.send(allow);
            return Ok(());
        }
    }
    Ok(())
}

#[tauri::command]
pub fn stop_agent(
    agent: State<'_, Arc<AgentState>>,
    conv_id: String,
) -> Result<(), String> {
    // 只停止指定会话；其它会话不受影响
    let sessions = agent.sessions.lock().unwrap();
    if let Some(s) = sessions.get(&conv_id) {
        s.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        // 解除该会话所有等待中的授权
        let mut pending = s.pending_permissions.lock().unwrap();
        for (_, tx) in pending.drain() {
            let _ = tx.send(false);
        }
        // 解除计划确认等待（打断视为拒绝，agent 侧回滚本轮修改）
        let mut pconf = s.pending_plan_confirm.lock().unwrap();
        if let Some(tx) = pconf.take() {
            let _ = tx.send(false);
        }
    }
    drop(sessions);
    // ★ 同时停止该会话派生的所有子代理（含其等待中的授权）
    let subagents = agent.subagents.lock().unwrap();
    for s in subagents.values() {
        if s.parent_conv_id == conv_id {
            s.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut pending = s.pending_permissions.lock().unwrap();
            for (_, tx) in pending.drain() {
                let _ = tx.send(false);
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn set_auth_mode(agent: State<'_, Arc<AgentState>>, mode: String) -> Result<(), String> {
    let v: u8 = match mode.as_str() {
        "ask" => crate::core::agent::AUTH_ASK,
        "smart" => crate::core::agent::AUTH_SMART,
        "allow_all" => crate::core::agent::AUTH_ALLOW_ALL,
        "none" => crate::core::agent::AUTH_NONE,
        _ => return Err("未知授权模式（ask / smart / allow_all / none）".into()),
    };
    agent
        .auth_mode
        .store(v, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use crate::core::types::ToolCall;

    fn sample_conv() -> ConversationMeta {
        ConversationMeta {
            id: "c1".into(),
            title: "测试会话".into(),
            work_dir: "/tmp/proj".into(),
            provider: "DeepSeek".into(),
            model: "deepseek-v4-flash".into(),
            reasoning_effort: "high".into(),
            engineering_mode: false,
            deepseek_mode: true,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_100,
        }
    }

    fn sample_messages() -> Vec<(i64, ChatMessage, i64)> {
        vec![
            (0, ChatMessage::user("你好"), 1_700_000_000_010),
            (
                1,
                ChatMessage {
                    role: "assistant".into(),
                    content: "你好！".into(),
                    reasoning_content: Some("思考中".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        call_type: "function".into(),
                        function: crate::core::types::ToolCallFunction {
                            name: "list_dir".into(),
                            arguments: "{}".into(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
                1_700_000_000_020,
            ),
            (
                2,
                ChatMessage::tool_result("call_1", "list_dir", "[F] main.rs"),
                1_700_000_000_030,
            ),
        ]
    }

    #[test]
    fn jsonl_export_contains_all_event_fields() {
        let out = render_export_jsonl(&sample_conv(), &sample_messages(), None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "header + 3 消息: {out}");
        let header: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["title"], "测试会话");
        let msg1: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(msg1["role"], "user");
        assert_eq!(msg1["seq"], 0);
        let msg2: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(msg2["reasoningContent"], "思考中");
        assert_eq!(msg2["toolCalls"][0]["function"]["name"], "list_dir");
        let msg3: Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(msg3["toolCallId"], "call_1");
    }

    #[test]
    fn markdown_export_is_readable() {
        let out = render_export_markdown(&sample_conv(), &sample_messages(), None);
        assert!(out.contains("# 会话：测试会话"));
        assert!(out.contains("**用户**"));
        assert!(out.contains("🧠 推理"));
        assert!(out.contains("`list_dir`"));
        assert!(out.contains("[F] main.rs"));
        assert!(out.contains("deepseek-v4-flash"));
    }

    #[test]
    fn ts_display_formats_readable() {
        // 2023-11-14 22:13:20 UTC
        assert_eq!(ts_display(1_700_000_000_000), "2023-11-14 22:13:20");
    }
}
