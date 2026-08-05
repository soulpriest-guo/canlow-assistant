// Canlow Next Rust 核心入口
mod commands;
mod core;

use std::sync::Arc;
use tauri::Manager;

use core::agent::AgentState;
use core::db::Db;
use core::taskmap::TaskMapStore;
use core::tools::CmdRegistry;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            std::fs::create_dir_all(&data_dir).ok();
            let db = Db::open(&data_dir).expect("初始化数据库失败");
            app.manage(db);
            app.manage(Arc::new(CmdRegistry::default()));
            app.manage(Arc::new(AgentState::default()));
            app.manage(Arc::new(TaskMapStore::default()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::session_list,
            commands::session_create,
            commands::session_delete,
            commands::session_rename,
            commands::session_update,
            commands::session_messages,
            commands::agent_turn,
            commands::agent_resume,
            commands::providers_list,
            commands::provider_save_key,
            commands::provider_set_models,
            commands::provider_key_status,
            commands::provider_test,
            commands::custom_provider_add,
            commands::custom_provider_remove,
            commands::context_profile_get,
            commands::context_profile_set,
            commands::taskmap_get,
            commands::taskmap_save,
            commands::taskmap_delete,
            commands::taskmap_sync_memory,
            commands::respond_permission,
            commands::respond_plan_confirm,
            commands::stop_agent,
            commands::set_auth_mode,
        ])
        .run(tauri::generate_context!())
        .expect("Canlow Next 启动失败");
}
