// SQLite 存储：会话 + 消息（参考 Codex 的 state 库设计）
// - 会话元数据一张表，消息一张表（追加式，seq 保证顺序）
// - 所有路径参数都从应用数据目录解析，禁止外部注入
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::Mutex;

use super::types::{ChatMessage, ConversationMeta, ToolCall};

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(data_dir: &PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        let conn = Connection::open(data_dir.join("canlow.db")).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                work_dir TEXT NOT NULL DEFAULT '',
                provider TEXT NOT NULL DEFAULT 'DeepSeek',
                model TEXT NOT NULL DEFAULT '',
                engineering_mode INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conv_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                reasoning_content TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                name TEXT,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(conv_id) REFERENCES conversations(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conv_id, seq);
            CREATE TABLE IF NOT EXISTS taskmaps (
                conv_id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                FOREIGN KEY(conv_id) REFERENCES conversations(id) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conv_id TEXT NOT NULL,
                title TEXT NOT NULL,
                done INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS cache_entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conv_id TEXT NOT NULL,
                summary TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS file_backups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conv_id TEXT NOT NULL,
                path TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| e.to_string())?;
        // 迁移：为旧库添加 reasoning_effort 列（建表之后执行）
        let has_effort: bool = conn
            .prepare("PRAGMA table_info(conversations)")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .any(|name| name == "reasoning_effort");
        if !has_effort {
            conn.execute(
                "ALTER TABLE conversations ADD COLUMN reasoning_effort TEXT NOT NULL DEFAULT 'high'",
                [],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    pub fn create_conversation(
        &self,
        title: &str,
        work_dir: &str,
        provider: &str,
        model: &str,
        reasoning_effort: &str,
    ) -> Result<ConversationMeta, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Self::now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conversations (id, title, work_dir, provider, model, reasoning_effort, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![id, title, work_dir, provider, model, reasoning_effort, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(ConversationMeta {
            id,
            title: title.to_string(),
            work_dir: work_dir.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            reasoning_effort: reasoning_effort.to_string(),
            engineering_mode: false,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn list_conversations(&self) -> Result<Vec<ConversationMeta>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, title, work_dir, provider, model, reasoning_effort, engineering_mode, created_at, updated_at
                 FROM conversations ORDER BY updated_at DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ConversationMeta {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    work_dir: r.get(2)?,
                    provider: r.get(3)?,
                    model: r.get(4)?,
                    reasoning_effort: r.get(5)?,
                    engineering_mode: r.get::<_, i64>(6)? != 0,
                    created_at: r.get(7)?,
                    updated_at: r.get(8)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn get_conversation(&self, id: &str) -> Result<Option<ConversationMeta>, String> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id, title, work_dir, provider, model, reasoning_effort, engineering_mode, created_at, updated_at
                 FROM conversations WHERE id = ?1",
                params![id],
                |r| {
                    Ok(ConversationMeta {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        work_dir: r.get(2)?,
                        provider: r.get(3)?,
                        model: r.get(4)?,
                        reasoning_effort: r.get(5)?,
                        engineering_mode: r.get::<_, i64>(6)? != 0,
                        created_at: r.get(7)?,
                        updated_at: r.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row)
    }

    pub fn rename_conversation(&self, id: &str, title: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET title = ?1, updated_at = ?2 WHERE id = ?3",
            params![title, Self::now(), id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn update_conversation(
        &self,
        id: &str,
        work_dir: Option<&str>,
        provider: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        engineering_mode: Option<bool>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        if let Some(w) = work_dir {
            conn.execute(
                "UPDATE conversations SET work_dir = ?1, updated_at = ?2 WHERE id = ?3",
                params![w, Self::now(), id],
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(p) = provider {
            conn.execute(
                "UPDATE conversations SET provider = ?1, updated_at = ?2 WHERE id = ?3",
                params![p, Self::now(), id],
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(m) = model {
            conn.execute(
                "UPDATE conversations SET model = ?1, updated_at = ?2 WHERE id = ?3",
                params![m, Self::now(), id],
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(re) = reasoning_effort {
            conn.execute(
                "UPDATE conversations SET reasoning_effort = ?1, updated_at = ?2 WHERE id = ?3",
                params![re, Self::now(), id],
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(e) = engineering_mode {
            conn.execute(
                "UPDATE conversations SET engineering_mode = ?1, updated_at = ?2 WHERE id = ?3",
                params![e as i64, Self::now(), id],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn delete_conversation(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM conversations WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_messages(&self, conv_id: &str) -> Result<Vec<ChatMessage>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT role, content, reasoning_content, tool_calls, tool_call_id, name
                 FROM messages WHERE conv_id = ?1 ORDER BY seq",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![conv_id], |r| {
                let tool_calls_json: Option<String> = r.get(3)?;
                let tool_calls = tool_calls_json
                    .and_then(|j| serde_json::from_str::<Vec<ToolCall>>(&j).ok());
                Ok(ChatMessage {
                    role: r.get(0)?,
                    content: r.get(1)?,
                    reasoning_content: r.get(2)?,
                    tool_calls,
                    tool_call_id: r.get(4)?,
                    name: r.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn taskmap_load(&self, conv_id: &str) -> Result<Option<String>, String> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT data FROM taskmaps WHERE conv_id = ?1",
                params![conv_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row)
    }

    pub fn taskmap_save(&self, conv_id: &str, data: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO taskmaps (conv_id, data, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(conv_id) DO UPDATE SET data = ?2, updated_at = ?3",
            params![conv_id, data, Self::now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn taskmap_delete(&self, conv_id: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM taskmaps WHERE conv_id = ?1", params![conv_id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn todo_create(&self, conv_id: &str, title: &str) -> Result<i64, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO todos (conv_id, title, created_at) VALUES (?1, ?2, ?3)",
            params![conv_id, title, Self::now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    pub fn todo_list(&self, conv_id: &str) -> Result<Vec<(i64, String, bool)>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, title, done FROM todos WHERE conv_id = ?1 ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![conv_id], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn todo_update(&self, id: i64, done: Option<bool>, title: Option<&str>) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        if let Some(d) = done {
            conn.execute(
                "UPDATE todos SET done = ?1 WHERE id = ?2",
                params![d as i64, id],
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(t) = title {
            conn.execute(
                "UPDATE todos SET title = ?1 WHERE id = ?2",
                params![t, id],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    // 上下文缓存条目
    pub fn cache_add(&self, conv_id: &str, summary: &str, data: &str) -> Result<i64, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO cache_entries (conv_id, summary, data, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![conv_id, summary, data, Self::now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    pub fn cache_get(&self, entry_id: i64) -> Result<Option<(String, String)>, String> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT summary, data FROM cache_entries WHERE id = ?1",
                params![entry_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row)
    }

    // 文件备份（undo_file）
    pub fn backup_add(&self, conv_id: &str, path: &str, data: &str) -> Result<i64, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO file_backups (conv_id, path, data, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![conv_id, path, data, Self::now()],
        )
        .map_err(|e| e.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    pub fn backup_latest(&self, conv_id: &str, path: &str) -> Result<Option<String>, String> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT data FROM file_backups WHERE conv_id = ?1 AND path = ?2 ORDER BY id DESC LIMIT 1",
                params![conv_id, path],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row)
    }

    // 归档搜索：在缓存条目（完整快照/压缩原文）里搜索关键词
    pub fn cache_search(&self, conv_id: &str, keyword: &str, limit: usize) -> Result<Vec<(String, String)>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT summary, data FROM cache_entries WHERE conv_id = ?1 ORDER BY id DESC LIMIT 50",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![conv_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        let mut hits: Vec<(String, String)> = Vec::new();
        let kw = keyword.to_lowercase();
        for row in rows {
            let (summary, data) = row.map_err(|e| e.to_string())?;
            if let Ok(msgs) = serde_json::from_str::<Vec<serde_json::Value>>(&data) {
                for m in msgs.iter().rev() {
                    if hits.len() >= limit {
                        break;
                    }
                    let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
                    let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    if !content.is_empty() && content.to_lowercase().contains(&kw) {
                        let snippet: String = content.chars().take(300).collect();
                        hits.push((
                            format!("[归档:{}] {role}", summary.chars().take(16).collect::<String>()),
                            snippet,
                        ));
                    }
                }
            }
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    // 消息搜索
    pub fn search_messages(&self, conv_id: &str, keyword: &str, limit: usize) -> Result<Vec<(String, String)>, String> {
        let conn = self.conn.lock().unwrap();
        let like = format!("%{}%", keyword);
        let mut stmt = conn
            .prepare(
                "SELECT role, substr(content, 1, 300) FROM messages
                 WHERE conv_id = ?1 AND content LIKE ?2 ORDER BY seq DESC LIMIT ?3",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![conv_id, like, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn setting_get(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row)
    }

    pub fn setting_set(&self, key: &str, value: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 替换会话全部消息（压缩持久化：删除旧消息并以压缩视图重写）
    pub fn replace_messages(&self, conv_id: &str, msgs: &[ChatMessage]) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM messages WHERE conv_id = ?1", params![conv_id])
            .map_err(|e| e.to_string())?;
        let mut seq: i64 = 0;
        for m in msgs {
            seq += 1;
            let tool_calls_json = m
                .tool_calls
                .as_ref()
                .map(|tc| serde_json::to_string(tc).unwrap_or_default());
            tx.execute(
                "INSERT INTO messages (conv_id, seq, role, content, reasoning_content, tool_calls, tool_call_id, name, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    conv_id,
                    seq,
                    m.role,
                    m.content,
                    m.reasoning_content,
                    tool_calls_json,
                    m.tool_call_id,
                    m.name,
                    Self::now()
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            params![Self::now(), conv_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 追加一批消息（agent 循环与 UI 共用）；返回这批消息的最大 seq
    pub fn append_messages(&self, conv_id: &str, msgs: &[ChatMessage]) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        for m in msgs {
            let seq: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE conv_id = ?1",
                    params![conv_id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            let tool_calls_json = m
                .tool_calls
                .as_ref()
                .map(|tc| serde_json::to_string(tc).unwrap_or_default());
            tx.execute(
                "INSERT INTO messages (conv_id, seq, role, content, reasoning_content, tool_calls, tool_call_id, name, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    conv_id,
                    seq,
                    m.role,
                    m.content,
                    m.reasoning_content,
                    tool_calls_json,
                    m.tool_call_id,
                    m.name,
                    Self::now()
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            params![Self::now(), conv_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile_lite::tempdir;

    fn test_db() -> (Db, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().to_path_buf()).unwrap();
        (db, dir.path().to_path_buf())
    }

    #[test]
    fn session_crud() {
        let (db, _d) = test_db();
        let c = db.create_conversation("测试", "/tmp", "DeepSeek", "deepseek-v4-flash", "high").unwrap();
        assert_eq!(c.title, "测试");
        let list = db.list_conversations().unwrap();
        assert_eq!(list.len(), 1);
        db.rename_conversation(&c.id, "改名").unwrap();
        assert_eq!(db.get_conversation(&c.id).unwrap().unwrap().title, "改名");
        db.append_messages(&c.id, &[ChatMessage::user("你好")]).unwrap();
        let msgs = db.load_messages(&c.id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "你好");
        db.delete_conversation(&c.id).unwrap();
        assert_eq!(db.list_conversations().unwrap().len(), 0);
    }

    #[test]
    fn append_order() {
        let (db, _d) = test_db();
        let c = db.create_conversation("t", "", "", "", "high").unwrap();
        db.append_messages(&c.id, &[ChatMessage::user("a"), ChatMessage::user("b")]).unwrap();
        db.append_messages(&c.id, &[ChatMessage::user("c")]).unwrap();
        let msgs = db.load_messages(&c.id).unwrap();
        let seq: Vec<&str> = msgs.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(seq, vec!["a", "b", "c"]);
    }
}

mod tempfile_lite {
    use std::path::PathBuf;
    pub struct TempDir(PathBuf);
    pub fn tempdir() -> std::io::Result<TempDir> {
        let p = std::env::temp_dir().join(format!("canlow-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p)?;
        Ok(TempDir(p))
    }
    impl TempDir {
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
