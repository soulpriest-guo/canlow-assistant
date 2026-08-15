// 多提供商支持：内置 6 家 + 自定义（移植自旧版 API_PROVIDERS）
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::db::Db;
use super::types::ProviderConfig;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDef {
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub supports_thinking: bool,
    pub context_window: i64,
}

pub fn default_providers() -> Vec<ProviderDef> {
    vec![
        ProviderDef {
            name: "DeepSeek".into(),
            base_url: "https://api.deepseek.com".into(),
            models: vec!["deepseek-v4-pro".into(), "deepseek-v4-flash".into(), "deepseek-chat".into(), "deepseek-reasoner".into()],
            supports_thinking: true,
            context_window: 1_000_000,
        },
        ProviderDef {
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            models: vec![
                "gpt-5.6".into(), "gpt-5.6-sol".into(), "gpt-5.6-terra".into(),
                "gpt-5.5".into(), "gpt-5.4".into(), "gpt-5.4-mini".into(),
                "gpt-5.3-codex".into(), "gpt-5.1-codex-mini".into(),
            ],
            supports_thinking: false,
            context_window: 256_000,
        },
        ProviderDef {
            name: "智谱AI (Zhipu)".into(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            models: vec!["glm-5.2".into(), "glm-5.1".into(), "glm-5".into(), "glm-4.7".into(), "glm-4.7-flashx".into()],
            supports_thinking: true,
            context_window: 1_000_000,
        },
        ProviderDef {
            name: "MiniMax".into(),
            base_url: "https://api.minimaxi.com/v1".into(),
            models: vec!["MiniMax-M3".into(), "MiniMax-M2.7".into(), "MiniMax-M2.7-highspeed".into(), "MiniMax-M2.5".into()],
            supports_thinking: false,
            context_window: 1_000_000,
        },
        ProviderDef {
            name: "小米 (Xiaomi MiMo)".into(),
            base_url: "https://api.xiaomimimo.com/v1".into(),
            models: vec!["mimo-v2.5-pro".into(), "mimo-v2.5".into(), "mimo-v2-pro".into(), "mimo-v2-flash".into()],
            supports_thinking: true,
            context_window: 256_000,
        },
        ProviderDef {
            name: "Kimi (Moonshot)".into(),
            base_url: "https://api.moonshot.cn/v1".into(),
            models: vec!["kimi-k2.7-code".into(), "kimi-k2.6".into(), "kimi-k2.5".into(), "moonshot-v1-128k".into(), "moonshot-v1-32k".into()],
            supports_thinking: false,
            context_window: 256_000,
        },
    ]
}

/// 默认 + 自定义合并 + 用户模型覆盖（settings 里的 provider_models）
pub fn all_providers_with_overrides(
    custom_json: Option<&str>,
    models_json: Option<&str>,
) -> Vec<ProviderDef> {
    let mut out = all_providers(custom_json);
    if let Some(json) = models_json {
        if let Ok(overrides) = serde_json::from_str::<HashMap<String, Vec<String>>>(json) {
            for p in &mut out {
                if let Some(ms) = overrides.get(&p.name) {
                    if !ms.is_empty() {
                        p.models = ms.clone();
                    }
                }
            }
        }
    }
    out
}

/// 默认 + 自定义合并
pub fn all_providers(custom_json: Option<&str>) -> Vec<ProviderDef> {
    let mut out = default_providers();
    if let Some(json) = custom_json {
        if let Ok(custom) = serde_json::from_str::<Vec<ProviderDef>>(json) {
            out.extend(custom);
        }
    }
    out
}

/// 组装某提供商的完整配置（base_url + api_key + model），供 agent 请求使用
pub fn resolve_provider_config(
    db: &Db,
    name: &str,
    model: &str,
    effort: Option<&str>,
    thinking: Option<bool>,
) -> Result<ProviderConfig, String> {
    let custom = db.setting_get("custom_providers")?;
    let all = all_providers(custom.as_deref());
    let def = all
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("未知提供商: {name}"))?;
    let keys_json = db.setting_get("provider_keys")?;
    let keys: HashMap<String, String> = keys_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    let api_key = keys.get(name).cloned().unwrap_or_default();
    Ok(ProviderConfig {
        base_url: def.base_url.clone(),
        api_key,
        model: if model.is_empty() {
            def.models.first().cloned().unwrap_or_default()
        } else {
            model.to_string()
        },
        reasoning_effort: effort.map(String::from),
        thinking,
        supports_thinking: def.supports_thinking,
    })
}
