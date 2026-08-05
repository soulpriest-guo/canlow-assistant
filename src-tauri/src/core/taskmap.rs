// 任务图（思维图谱）核心：TaskMap 模型 + plan_* 工具派发
// 移植自旧版 taskmap.py，数据模型 camelCase 序列化供前端 React Flow 直接消费
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskNode {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub status: String, // todo / in_progress / done / blocked
    pub progress: f64,  // 0..100
    pub note: String,
    pub parent_id: String,
    pub deps: Vec<String>,
    pub pos: [f64; 2],
    pub created: i64,
    /// 开始执行时间戳（plan_update 置 in_progress 时记录，仅首次）
    #[serde(default)]
    pub started_at: Option<i64>,
    /// 完成时间戳（plan_update 置 done 时记录）
    #[serde(default)]
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskMapData {
    pub version: i32,
    pub requirement: String,
    pub root_id: String,
    pub nodes: HashMap<String, TaskNode>,
    pub changelog: Vec<String>,
    pub created: i64,
    pub updated: i64,
}

#[derive(Clone, Debug)]
pub struct TaskMap {
    pub data: TaskMapData,
    /// 层级编号计数器：字母（层级）-> 该层已用最大数字（全局递增不重复）
    id_seq: HashMap<char, i64>,
    /// 最近一次 add_children 的 key -> node_id 映射（用于向 AI 返回引用关系）
    last_keys: HashMap<String, String>,
    /// 用户是否通过前端修改了任务图（taskmap_sync_memory 时对比 changelog 置位，
    /// agent 下一轮读取后清除并注入"用户改了图，请重新审视"）
    pub user_modified: bool,
    /// 历史快照栈（AI 工具修改前 push，plan_undo 回滚用），最多 20 条
    history: Vec<TaskMapData>,
    /// 当前执行焦点任务（AI 正在执行的任务；plan_update in_progress 自动设置，
    /// plan_focus 显式设置；工作类工具校验依赖此状态）
    pub active_focus: Option<String>,
}

/// 会话级任务图内存存储（conv_id -> TaskMap），agent 工具与前端共用
pub type TaskMapStore = std::sync::Mutex<std::collections::HashMap<String, TaskMap>>;

/// 状态流转合法性（AI 侧 plan_update 与内部调用统一约束；
/// 前端用户操作走 taskmap_sync_memory 全量覆盖，不受此限制）
fn is_valid_transition(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        ("todo", "in_progress")
            | ("todo", "blocked")
            | ("in_progress", "done")
            | ("in_progress", "blocked")
            | ("blocked", "todo")
            | ("blocked", "in_progress")
            | ("done", "todo")
    )
}

impl TaskMap {
    pub fn from_data(data: TaskMapData) -> Self {
        let mut tm = Self {
            data,
            id_seq: HashMap::new(),
            last_keys: HashMap::new(),
            user_modified: false,
            history: Vec::new(),
            active_focus: None,
        };
        // 旧数据（n1/n2 格式）自动迁移为层级编号
        tm.migrate_legacy_ids();
        // root 统一为「任务目标」总节点：标题固定，说明放完整需求（兼容旧数据各种标题）
        {
            let req = tm.data.requirement.clone();
            if let Some(root) = tm.data.nodes.get_mut(&tm.data.root_id) {
                root.title = "任务目标".to_string();
                root.detail = req;
            }
        }
        // 从现有编号恢复各层级计数器（字母 -> 最大数字）
        tm.rebuild_id_seq();
        tm
    }

    /// 解析编号的一段「字母+数字」：返回 (层级字母, 数字)。如 "a1b2" 的最后一段 → ('b', 2)
    fn parse_segment(id: &str) -> Option<(char, i64)> {
        let chars: Vec<char> = id.chars().collect();
        let mut i = chars.len();
        while i > 0 {
            i -= 1;
            if chars[i].is_ascii_alphabetic() {
                let letter = chars[i];
                let num_str: String = chars[i + 1..].iter().collect();
                let num = num_str.parse::<i64>().unwrap_or(0);
                return Some((letter, num));
            }
        }
        None
    }

    /// 编号的自身段字符串（最后一段「字母+数字」）。如 "a1b2" → "b2"
    fn segment_str(id: &str) -> String {
        Self::parse_segment(id)
            .map(|(l, n)| format!("{l}{n}"))
            .unwrap_or_else(|| id.to_string())
    }

    /// 生成下一个层级编号：
    /// - 一级任务（父为 root 或空）：字母 a 起始，无前缀 → a1, a2...
    /// - 子任务：父自身段 + 本级字母（父层级字母+1）+ 本级数字（该层全局递增不重复）
    ///   例：a1 的子任务 a1b1/a1b2；a1b2 的子任务 b2c1；b2c1 的子任务 c1d5
    fn next_id(&mut self, parent_id: &str) -> String {
        let (prefix, level_char) = if parent_id.is_empty() || parent_id == self.data.root_id {
            (String::new(), 'a')
        } else {
            let (p_letter, _) = Self::parse_segment(parent_id).unwrap_or(('a', 0));
            let prefix = Self::segment_str(parent_id);
            let next_letter = if p_letter == 'z' { 'z' } else { (p_letter as u8 + 1) as char };
            (prefix, next_letter)
        };
        let n = self.id_seq.entry(level_char).or_insert(0);
        *n += 1;
        format!("{prefix}{level_char}{n}")
    }

    /// 从现有节点编号恢复各层级计数器（字母 -> 该层已用最大数字）
    fn rebuild_id_seq(&mut self) {
        self.id_seq.clear();
        for n in self.data.nodes.values() {
            if n.id == self.data.root_id {
                continue;
            }
            let chars: Vec<char> = n.id.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if chars[i].is_ascii_alphabetic() {
                    let letter = chars[i];
                    i += 1;
                    let mut num = 0i64;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        num = num * 10 + (chars[i] as i64 - '0' as i64);
                        i += 1;
                    }
                    let e = self.id_seq.entry(letter).or_insert(0);
                    if num > *e {
                        *e = num;
                    }
                } else {
                    i += 1;
                }
            }
        }
    }

    /// 旧数据迁移：把 n1/n2 格式的编号（或混合）整体重编号为层级规则。
    /// 规则：一级任务（root 直属 + parent_id=="" 的独立任务线）→ a1/a2...，
    /// 子任务 = 父自身段 + 下一字母 + 全局数字。重建 nodes 的 key，
    /// 并同步更新 parent_id / deps 引用。
    fn migrate_legacy_ids(&mut self) {
        // 仅当存在旧格式编号（n 开头 + 纯数字，且非 root）时才迁移
        let has_legacy = self.data.nodes.values().any(|n| {
            n.id != self.data.root_id
                && n.id.starts_with('n')
                && n.id[1..].chars().all(|c| c.is_ascii_digit())
        });
        if !has_legacy {
            return;
        }
        let old_root = self.data.root_id.clone();
        let mut id_seq: HashMap<char, i64> = HashMap::new();
        let mut new_ids: HashMap<String, String> = HashMap::new(); // old -> new
        new_ids.insert(old_root.clone(), "root".to_string());
        // 一级任务：root 直属子任务 + parent_id == "" 的独立任务线，按创建时间排序
        let mut first_level: Vec<String> = self
            .data
            .nodes
            .values()
            .filter(|n| n.id != old_root && (n.parent_id == old_root || n.parent_id.is_empty()))
            .map(|n| n.id.clone())
            .collect();
        first_level.sort_by_key(|id| self.data.nodes.get(id).map(|n| n.created).unwrap_or(0));
        for cid in &first_level {
            let n = id_seq.entry('a').or_insert(0);
            *n += 1;
            new_ids.insert(cid.clone(), format!("a{n}"));
        }
        // 逐层 BFS：为每个任务的子任务分配下一层编号
        let mut queue: Vec<String> = first_level;
        let mut idx = 0;
        while idx < queue.len() {
            let pid = queue[idx].clone();
            idx += 1;
            let mut children: Vec<String> = self
                .data
                .nodes
                .values()
                .filter(|n| n.parent_id == pid)
                .map(|n| n.id.clone())
                .collect();
            children.sort_by_key(|id| self.data.nodes.get(id).map(|n| n.created).unwrap_or(0));
            let new_pid = new_ids.get(&pid).cloned().unwrap_or_else(|| pid.clone());
            let (p_letter, _) = Self::parse_segment(&new_pid).unwrap_or(('a', 0));
            let level_char = if p_letter == 'z' { 'z' } else { (p_letter as u8 + 1) as char };
            let prefix = Self::segment_str(&new_pid);
            for cid in children {
                let n = id_seq.entry(level_char).or_insert(0);
                *n += 1;
                new_ids.insert(cid.clone(), format!("{prefix}{level_char}{n}"));
                queue.push(cid);
            }
        }
        // 重建 nodes（key 替换 + 引用更新）
        let mut nodes: HashMap<String, TaskNode> = HashMap::new();
        for (old_id, node) in self.data.nodes.iter() {
            let new_id = new_ids.get(old_id).cloned().unwrap_or_else(|| old_id.clone());
            let mut nn = node.clone();
            nn.id = new_id.clone();
            nn.parent_id = new_ids
                .get(&node.parent_id)
                .cloned()
                .unwrap_or_else(|| node.parent_id.clone());
            nn.deps = node
                .deps
                .iter()
                .map(|d| new_ids.get(d).cloned().unwrap_or_else(|| d.clone()))
                .collect();
            nodes.insert(new_id, nn);
        }
        self.data.nodes = nodes;
        self.data.root_id = "root".to_string();
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    pub fn create(requirement: &str, breakdown: &Value) -> Result<Self, String> {
        let req = requirement.trim().to_string();
        if req.is_empty() {
            return Err("需求不能为空".into());
        }
        let now = Self::now();
        let mut tm = Self {
            data: TaskMapData {
                version: 1,
                requirement: req.clone(),
                root_id: "root".into(),
                nodes: HashMap::new(),
                changelog: vec![format!("创建任务图，原始需求：{req}")],
                created: now,
                updated: now,
            },
            id_seq: HashMap::new(),
            last_keys: HashMap::new(),
            user_modified: false,
            history: Vec::new(),
            active_focus: None,
        };
        // root 固定 ID "root"（不参与编号，仅作任务目标总节点）
        // 标题固定「任务目标」（唯一总节点标识，高于一级任务），说明放完整需求
        let root = TaskNode {
            id: "root".into(),
            title: "任务目标".into(),
            detail: req.clone(),
            status: "todo".into(),
            progress: 0.0,
            note: String::new(),
            parent_id: String::new(),
            deps: Vec::new(),
            pos: [0.0, 0.0],
            created: now,
            started_at: None,
            finished_at: None,
        };
        tm.data.root_id = root.id.clone();
        tm.data.nodes.insert(root.id.clone(), root);
        if !breakdown.is_null() {
            let tasks: Vec<Value> = breakdown.as_array().cloned().unwrap_or_default();
            // ★ breakdown 顶层任务创建为 root 直属的一级任务（目标层）：
            //   层级语义：一级=需求目标、二级=实现目标的大体步骤、三级=进一步细分步骤；
            //   避免把所有步骤平铺成 parent_id=="" 的独立一级任务线（任务全是一级的问题）。
            //   多轮对话中差异大的新需求仍可用 plan_breakdown(parent_id=top) 建独立任务线。
            let root_id = tm.data.root_id.clone();
            let ids = tm.add_children(&root_id, &tasks)?;
            // 递归后为顶层任务建立父子顺序依赖（可选，简化：不自动加依赖）
            let _ = ids;
        }
        Ok(tm)
    }

    fn make_node(&mut self, title: &str, detail: &str, parent: Option<&str>) -> TaskNode {
        let id = self.next_id(parent.unwrap_or(""));
        TaskNode {
            id: id.clone(),
            title: title.to_string(),
            detail: detail.to_string(),
            status: "todo".into(),
            progress: 0.0,
            note: String::new(),
            parent_id: parent.unwrap_or("").to_string(),
            deps: Vec::new(),
            pos: [0.0, 0.0],
            created: Self::now(),
            started_at: None,
            finished_at: None,
        }
    }

    pub fn get_node(&self, id: &str) -> Result<&TaskNode, String> {
        self.data.nodes.get(id).ok_or_else(|| format!("任务不存在: {id}"))
    }

    pub fn add_children(&mut self, parent_id: &str, tasks: &[Value]) -> Result<Vec<String>, String> {
        // 允许 parent_id == ""：创建与 root 平级的独立一级任务（新任务线）
        if !parent_id.is_empty() && !self.data.nodes.contains_key(parent_id) {
            return Err(format!("父任务不存在: {parent_id}"));
        }
        // key -> node_id 映射（本次插入范围），供 tasks 的 deps 字段引用
        let mut key_map: HashMap<String, String> = HashMap::new();
        // 收集 (node_id, deps 引用列表)，全部插入完成后统一解析建链
        let mut pending_deps: Vec<(String, Vec<String>)> = Vec::new();

        fn insert_rec(
            tm: &mut TaskMap,
            parent_id: &str,
            tasks: &[Value],
            key_map: &mut HashMap<String, String>,
            pending_deps: &mut Vec<(String, Vec<String>)>,
        ) -> Result<Vec<String>, String> {
            let mut ids = Vec::new();
            for t in tasks {
                let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("未命名任务").to_string();
                let detail = t.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let node = tm.make_node(&title, &detail, Some(parent_id));
                let id = node.id.clone();
                tm.data.nodes.insert(id.clone(), node);
                // 注册 key（可选，供 deps 引用）
                if let Some(k) = t.get("key").and_then(|v| v.as_str()) {
                    let k = k.trim();
                    if !k.is_empty() {
                        if key_map.contains_key(k) {
                            return Err(format!("任务 key 重复: {k}"));
                        }
                        key_map.insert(k.to_string(), id.clone());
                    }
                }
                // 收集 deps 引用（key 或已有节点 ID）
                if let Some(deps) = t.get("deps").and_then(|d| d.as_array()) {
                    let refs: Vec<String> = deps
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect();
                    if !refs.is_empty() {
                        pending_deps.push((id.clone(), refs));
                    }
                }
                ids.push(id.clone());
                if let Some(children) = t.get("children").and_then(|c| c.as_array()) {
                    insert_rec(tm, &id, children, key_map, pending_deps)?;
                }
            }
            Ok(ids)
        }

        let ids = insert_rec(self, parent_id, tasks, &mut key_map, &mut pending_deps)?;

        // 解析 deps：优先按 key 查找，其次按节点 ID；set_deps 内含环检测
        for (node_id, refs) in pending_deps {
            let mut resolved: Vec<String> = Vec::new();
            for r in &refs {
                if let Some(id) = key_map.get(r) {
                    resolved.push(id.clone());
                } else if self.data.nodes.contains_key(r) {
                    resolved.push(r.clone());
                } else {
                    return Err(format!(
                        "任务「{}」的 deps 引用了不存在的 key/ID: {r}（可引用本次 tasks 中的 key 或已有节点 ID）",
                        self.data.nodes.get(&node_id).map(|n| n.title.as_str()).unwrap_or("")
                    ));
                }
            }
            self.set_deps(&node_id, &resolved)?;
        }

        // 记录本次 key 映射（供 plan_init/plan_breakdown 返回给 AI 使用）
        self.last_keys = key_map;
        self.touch();
        Ok(ids)
    }

    /// 最近一次 add_children 的 key -> node_id 映射
    pub fn last_key_map(&self) -> &HashMap<String, String> {
        &self.last_keys
    }

    pub fn update_node(
        &mut self,
        task_id: &str,
        status: Option<&str>,
        note: Option<&str>,
        progress: Option<f64>,
    ) -> Result<(), String> {
        let node = self
            .data
            .nodes
            .get_mut(task_id)
            .ok_or_else(|| format!("任务不存在: {task_id}"))?;
        if let Some(s) = status {
            let s = s.to_lowercase();
            if !["todo", "in_progress", "done", "blocked"].contains(&s.as_str()) {
                return Err(format!("非法状态: {s}"));
            }
            let now = Self::now();
            // 状态流转约束（AI 侧；前端直接操作不走此路径）
            if !is_valid_transition(&node.status, &s) {
                return Err(format!(
                    "非法状态流转: {} → {}。允许的流转：todo→in_progress/todo→blocked、in_progress→done/in_progress→blocked、blocked→todo/blocked→in_progress、done→todo（重做）。如需强制改状态请先把任务置回 todo。",
                    node.status, s
                ));
            }
            // 执行时间戳：首次进入 in_progress 记录开始时间；done 记录完成时间；
            // 从 done 退回时清空完成时间
            if s == "in_progress" && node.status != "in_progress" {
                if node.started_at.is_none() {
                    node.started_at = Some(now);
                }
                node.finished_at = None;
                // 自动设为执行焦点
                self.active_focus = Some(task_id.to_string());
            }
            if s == "done" {
                node.progress = 100.0;
                node.finished_at = Some(now);
                // 焦点任务完成后清空焦点（保持与任务图一致）
                if self.active_focus.as_deref() == Some(task_id) {
                    self.active_focus = None;
                }
            }
            if (s == "todo" || s == "blocked") && node.status == "done" {
                node.finished_at = None;
            }
            node.status = s.clone();
        }
        if let Some(n) = note {
            node.note = n.to_string();
        }
        if let Some(p) = progress {
            node.progress = p.clamp(0.0, 100.0);
        }
        self.touch();
        Ok(())
    }

    /// 修改前压入历史快照（plan_undo 回滚用）
    pub fn push_history(&mut self) {
        self.history.push(self.data.clone());
        if self.history.len() > 20 {
            self.history.remove(0);
        }
    }

    /// 回滚到最近一次快照；返回是否成功
    pub fn undo(&mut self) -> Option<TaskMapData> {
        let snap = self.history.pop()?;
        self.restore(snap.clone());
        Some(snap)
    }

    /// 当前执行焦点任务（in_progress 中最早开始的那个，无则 None）
    pub fn focus_task(&self) -> Option<&TaskNode> {
        if let Some(id) = &self.active_focus {
            if let Some(n) = self.data.nodes.get(id) {
                if n.status == "in_progress" {
                    return Some(n);
                }
            }
        }
        // 兜底：任意 in_progress 任务
        self.data
            .nodes
            .values()
            .filter(|n| n.status == "in_progress")
            .min_by_key(|n| n.started_at.unwrap_or(n.created))
    }

    /// 是否存在正在执行的任务
    pub fn has_in_progress(&self) -> bool {
        self.data.nodes.values().any(|n| n.status == "in_progress")
    }

    /// 按 ID 或标题解析任务：ID 精确 → 标题完全匹配 → 标题包含匹配（唯一）
    /// 返回 (节点ID, 是否按标题解析)
    pub fn resolve_task_id(&self, id_or_title: &str) -> Result<(String, bool), String> {
        let q = id_or_title.trim();
        if q.is_empty() {
            return Err("缺少参数：task_id 不能为空".into());
        }
        if self.data.nodes.contains_key(q) {
            return Ok((q.to_string(), false));
        }
        // 完全匹配标题
        let exact: Vec<&TaskNode> = self.data.nodes.values().filter(|n| n.title == q).collect();
        if exact.len() == 1 {
            return Ok((exact[0].id.clone(), true));
        }
        // 包含匹配（标题包含关键词，或关键词包含标题）
        let fuzzy: Vec<&TaskNode> = self
            .data
            .nodes
            .values()
            .filter(|n| n.title.contains(q) || q.contains(&n.title))
            .collect();
        if fuzzy.len() == 1 {
            return Ok((fuzzy[0].id.clone(), true));
        }
        if fuzzy.len() > 1 {
            let candidates: Vec<String> = fuzzy
                .iter()
                .take(5)
                .map(|n| format!("{}（{}）", n.title, n.id))
                .collect();
            return Err(format!(
                "「{q}」匹配到多个任务：{}。请用 plan_find 确认精确 ID 后重试",
                candidates.join("、")
            ));
        }
        Err(format!("任务不存在: {q}（ID 或标题均未匹配，可用 plan_find 搜索）"))
    }

    /// 按标题搜索节点，返回匹配列表
    pub fn find_by_title(&self, keyword: &str, max: usize) -> Vec<(String, String, String)> {
        let kw = keyword.trim();
        if kw.is_empty() {
            return Vec::new();
        }
        self.data
            .nodes
            .values()
            .filter(|n| n.title.contains(kw))
            .take(max)
            .map(|n| (n.id.clone(), n.title.clone(), n.status.clone()))
            .collect()
    }

    /// 子树节点 ID 集合（含自身）
    pub fn subtree_ids(&self, root: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut stack = vec![root.to_string()];
        while let Some(nid) = stack.pop() {
            if !out.insert(nid.clone()) {
                continue;
            }
            let children: Vec<String> = self
                .data
                .nodes
                .values()
                .filter(|n| n.parent_id == nid)
                .map(|n| n.id.clone())
                .collect();
            stack.extend(children);
        }
        out
    }

    /// 一级任务（顶层任务线）ID 列表，按创建时间排序。
    /// 一级任务判定（兼容新旧数据）：
    /// - parent_id == "" 的独立顶层任务（与 root 平级，plan_init breakdown / plan_breakdown "top" 创建）
    /// - parent_id == root_id 的 root 直属子任务（旧版任务图结构）
    /// root 自身不算一级任务。
    pub fn top_level_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .data
            .nodes
            .values()
            .filter(|n| {
                n.id != self.data.root_id
                    && (n.parent_id.is_empty() || n.parent_id == self.data.root_id)
            })
            .map(|n| n.id.clone())
            .collect();
        ids.sort_by_key(|id| self.data.nodes.get(id).map(|n| n.created).unwrap_or(0));
        ids
    }

    /// 完成校验：返回该任务下尚未完成的子任务标题（空=无未完成子任务）
    pub fn pending_children(&self, task_id: &str) -> Vec<String> {
        self.data
            .nodes
            .values()
            .filter(|n| n.parent_id == task_id && n.status != "done")
            .map(|n| n.title.clone())
            .collect()
    }

    pub fn set_deps(&mut self, task_id: &str, depends_on: &[String]) -> Result<(), String> {
        // 依赖必须存在且不能形成环
        let mut deps = Vec::new();
        for d in depends_on {
            if !self.data.nodes.contains_key(d) {
                return Err(format!("依赖任务不存在: {d}"));
            }
            if d == task_id {
                return Err("任务不能依赖自己".into());
            }
            deps.push(d.clone());
        }
        // 环检测：从每个新依赖出发沿其 deps 链遍历，若到达 task_id 则成环
        for d in &deps {
            let mut stack = vec![d.clone()];
            let mut visited: HashSet<String> = HashSet::new();
            while let Some(nid) = stack.pop() {
                if nid == task_id {
                    return Err("设置依赖会形成环".into());
                }
                if !visited.insert(nid.clone()) {
                    continue;
                }
                let node = self.data.nodes.get(&nid).unwrap();
                stack.extend(node.deps.iter().cloned());
            }
        }
        if let Some(node) = self.data.nodes.get_mut(task_id) {
            node.deps = deps;
        }
        self.touch();
        Ok(())
    }

    pub fn move_node(&mut self, task_id: &str, new_parent_id: &str) -> Result<(), String> {
        // new_parent_id == ""：移动到顶层，成为独立一级任务（与 root 平级）
        if !new_parent_id.is_empty() && !self.data.nodes.contains_key(new_parent_id) {
            return Err(format!("目标父节点不存在: {new_parent_id}"));
        }
        if task_id == new_parent_id {
            return Err("不能移动到自身".into());
        }
        // 防环：新父节点不能是 task_id 的子树
        // （new_parent_id 为空 = 顶层，无父链可查，天然不成环）
        let mut stack = vec![new_parent_id.to_string()];
        let mut seen = HashSet::new();
        while let Some(nid) = stack.pop() {
            if nid == task_id {
                return Err("不能移动到自己的子树下".into());
            }
            if !seen.insert(nid.clone()) {
                continue;
            }
            if let Some(node) = self.data.nodes.get(&nid) {
                stack.push(node.parent_id.clone());
            }
        }
        if let Some(node) = self.data.nodes.get_mut(task_id) {
            node.parent_id = new_parent_id.to_string();
        }
        self.touch();
        Ok(())
    }

    pub fn delete_node(&mut self, task_id: &str) -> Result<(), String> {
        if task_id == self.data.root_id {
            return Err("不能删除根节点".into());
        }
        // 收集子树
        let mut to_delete = vec![task_id.to_string()];
        let mut stack = vec![task_id.to_string()];
        while let Some(nid) = stack.pop() {
            let children: Vec<String> = self
                .data
                .nodes
                .values()
                .filter(|n| n.parent_id == nid)
                .map(|n| n.id.clone())
                .collect();
            for c in children {
                to_delete.push(c.clone());
                stack.push(c);
            }
        }
        for id in &to_delete {
            self.data.nodes.remove(id);
        }
        // 清理其它节点的依赖引用
        for node in self.data.nodes.values_mut() {
            node.deps.retain(|d| !to_delete.contains(d));
        }
        self.touch();
        Ok(())
    }

    pub fn change_requirement(&mut self, new_req: &str) -> Result<(), String> {
        let req = new_req.trim().to_string();
        if req.is_empty() {
            return Err("需求不能为空".into());
        }
        self.data.changelog.push(format!("需求更新：{req}"));
        self.data.requirement = req.clone();
        // 同步 root 说明为完整需求（标题保持「任务目标」总节点标识）
        if let Some(root) = self.data.nodes.get_mut(&self.data.root_id) {
            root.title = "任务目标".to_string();
            root.detail = req.clone();
        }
        self.touch();
        Ok(())
    }

    /// 可执行任务：状态未完成且所有依赖已完成
    pub fn next_tasks(&self, max: usize) -> Vec<String> {
        let mut out = Vec::new();
        for node in self.data.nodes.values() {
            if node.id == self.data.root_id || node.status == "done" {
                continue;
            }
            let deps_done = node.deps.iter().all(|d| {
                self.data
                    .nodes
                    .get(d)
                    .map(|n| n.status == "done")
                    .unwrap_or(false)
            });
            if deps_done {
                out.push(node.id.clone());
            }
        }
        out.sort_by_key(|id| self.data.nodes.get(id).map(|n| n.created).unwrap_or(0));
        out.truncate(max);
        out
    }

    /// 拓扑排序：按依赖关系计算执行顺序（Kahn 算法，参考旧版 taskmap.py）
    /// 返回 HashMap<node_id, 序号>，序号从 1 开始
    pub fn topo_order(&self) -> std::collections::HashMap<String, usize> {
        let nodes = &self.data.nodes;
        let mut rev: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for nid in nodes.keys() {
            rev.entry(nid.clone()).or_default();
        }
        for (nid, n) in nodes.iter() {
            for d in &n.deps {
                if nodes.contains_key(d) {
                    rev.entry(d.clone()).or_default().push(nid.clone());
                }
            }
        }
        let mut indeg: std::collections::HashMap<String, usize> = nodes
            .iter()
            .map(|(nid, n)| (nid.clone(), n.deps.iter().filter(|d| nodes.contains_key(*d)).count()))
            .collect();
        let mut ready: Vec<String> = nodes
            .keys()
            .filter(|nid| **nid != self.data.root_id && indeg.get(*nid).copied().unwrap_or(1) == 0)
            .cloned()
            .collect();
        ready.sort_by_key(|nid| nodes.get(nid).map(|n| n.created).unwrap_or(0));
        let mut order: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut counter = 1usize;
        while let Some(nid) = ready.first().cloned() {
            ready.remove(0);
            order.insert(nid.clone(), counter);
            counter += 1;
            if let Some(dependents) = rev.get(&nid).cloned() {
                for dep in dependents {
                    if order.contains_key(&dep) {
                        continue;
                    }
                    let e = indeg.entry(dep.clone()).or_insert(0);
                    if *e > 0 {
                        *e -= 1;
                    }
                    if *e == 0 {
                        ready.push(dep.clone());
                    }
                }
            }
            ready.sort_by_key(|nid| nodes.get(nid).map(|n| n.created).unwrap_or(0));
        }
        // 兜底：环内节点按创建时间补序
        let mut fallback: Vec<&String> = nodes.keys().filter(|nid| !order.contains_key(*nid)).collect();
        fallback.sort_by_key(|nid| nodes.get(*nid).map(|n| n.created).unwrap_or(0));
        for nid in fallback {
            if *nid != self.data.root_id {
                order.insert(nid.clone(), counter);
                counter += 1;
            }
        }
        order
    }

    /// 执行链：按拓扑顺序返回参与依赖链的任务 [(id, 序号)]
    pub fn execution_chain(&self) -> Vec<(String, usize)> {
        let order = self.topo_order();
        let mut chain: std::collections::HashSet<String> = std::collections::HashSet::new();
        for n in self.data.nodes.values() {
            if !n.deps.is_empty() {
                chain.insert(n.id.clone());
                chain.extend(n.deps.iter().cloned());
            }
        }
        let mut out: Vec<(String, usize)> = order
            .iter()
            .filter(|(id, _)| chain.contains(*id))
            .map(|(id, seq)| (id.clone(), *seq))
            .collect();
        out.sort_by_key(|(_, seq)| *seq);
        out
    }

    /// 阻塞节点：有未完成依赖的任务
    pub fn blocked_nodes(&self) -> Vec<String> {
        self.data
            .nodes
            .values()
            .filter(|n| n.status != "done")
            .filter(|n| {
                n.deps.iter().any(|d| {
                    self.data
                        .nodes
                        .get(d)
                        .map(|dn| dn.status != "done")
                        .unwrap_or(false)
                })
            })
            .map(|n| n.id.clone())
            .collect()
    }

    /// 阻塞原因：返回 (被阻塞任务 id, 未完成依赖 id 列表)
    pub fn blocked_reasons(&self) -> Vec<(String, Vec<String>)> {
        self.data
            .nodes
            .values()
            .filter(|n| n.status != "done")
            .map(|n| {
                let pending: Vec<String> = n
                    .deps
                    .iter()
                    .filter(|d| {
                        self.data
                            .nodes
                            .get(*d)
                            .map(|dn| dn.status != "done")
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                (n.id.clone(), pending)
            })
            .filter(|(_, p)| !p.is_empty())
            .collect()
    }

    /// 关键路径：最长的依赖链（串行链上节点最多），按拓扑顺序返回节点 id。
    /// 空结果表示不存在任何依赖关系（全部任务可并行）。
    pub fn critical_path(&self) -> Vec<String> {
        let nodes = &self.data.nodes;
        if nodes.is_empty() {
            return Vec::new();
        }
        // 无任何依赖关系 → 无关键路径
        if nodes.values().all(|n| n.deps.is_empty()) {
            return Vec::new();
        }
        // 依赖方向：A.deps=[B] 表示 B 先完成 A 才能开始；DP 沿 deps 反向累积
        let order = self.topo_order(); // id -> 序号
        let mut sorted: Vec<&String> = nodes.keys().collect();
        sorted.sort_by_key(|id| order.get(*id).copied().unwrap_or(usize::MAX));
        let mut dist: HashMap<String, usize> = HashMap::new();
        let mut prev: HashMap<String, String> = HashMap::new();
        for id in &sorted {
            let node = &nodes[*id];
            let mut best = 0usize;
            let mut best_dep: Option<String> = None;
            for d in &node.deps {
                if let Some(dv) = dist.get(d) {
                    if *dv > best {
                        best = *dv;
                        best_dep = Some(d.clone());
                    }
                }
            }
            dist.insert((*id).clone(), best + 1);
            if let Some(d) = best_dep {
                prev.insert((*id).clone(), d);
            }
        }
        // 取 dist 最大的非根节点作为链尾
        let mut end: Option<String> = None;
        let mut maxd = 0usize;
        for (id, v) in &dist {
            if *id == self.data.root_id {
                continue;
            }
            if *v > maxd {
                maxd = *v;
                end = Some(id.clone());
            }
        }
        let mut path: Vec<String> = Vec::new();
        if let Some(mut cur) = end {
            loop {
                path.push(cur.clone());
                match prev.get(&cur) {
                    Some(p) => cur = p.clone(),
                    None => break,
                }
            }
            path.reverse();
        }
        path
    }

    pub fn stats(&self) -> (usize, usize, usize, usize) {
        let total = self.data.nodes.len();
        let done = self.data.nodes.values().filter(|n| n.status == "done").count();
        let in_progress = self.data.nodes.values().filter(|n| n.status == "in_progress").count();
        let blocked = self.data.nodes.values().filter(|n| n.status == "blocked").count();
        (total, done, in_progress, blocked)
    }

    pub fn review_summary(&self) -> String {
        let (total, done, ip, blk) = self.stats();
        let ready: Vec<String> = self
            .next_tasks(10)
            .into_iter()
            .map(|id| {
                let n = self.data.nodes.get(&id).unwrap();
                format!("- {}（{}）", n.title, n.id)
            })
            .collect();
        // 关键路径（最长串行依赖链）
        let cp = self.critical_path();
        let cp_summary = if cp.is_empty() {
            "（无顺序声明，默认按排列顺序依次执行）".to_string()
        } else {
            let names: Vec<String> = cp
                .iter()
                .map(|id| self.data.nodes.get(id).map(|n| n.title.clone()).unwrap_or_default())
                .collect();
            format!("{}（{} 步串行链）", names.join(" → "), cp.len())
        };
        // 并行机会：就绪任务数
        let ready_all = self.next_tasks(usize::MAX);
        let parallel_summary = if ready_all.len() > 1 {
            let names: Vec<String> = ready_all
                .iter()
                .take(5)
                .map(|id| self.data.nodes.get(id).map(|n| n.title.clone()).unwrap_or_default())
                .collect();
            format!("- 就绪任务：{} 个（当前无子代理，按图从上到下依次执行）：{}", ready_all.len(), names.join("、"))
        } else if ready_all.len() == 1 {
            "- 就绪任务：当前仅 1 个，串行推进".to_string()
        } else {
            "- 就绪任务：（无就绪任务，需先完成前序任务）".to_string()
        };
        // 执行顺序（拓扑排序摘要）
        let chain = self.execution_chain();
        let order_summary = if chain.is_empty() {
            "  （无顺序声明，默认按排列顺序依次执行）".to_string()
        } else {
            let parts: Vec<String> = chain
                .iter()
                .take(10)
                .map(|(id, seq)| {
                    let n = self.data.nodes.get(id).unwrap();
                    format!("{seq}. {}", n.title)
                })
                .collect();
            parts.join(" → ")
        };
        // 阻塞原因（被阻塞任务 + 等待的依赖）
        let blocked = self.blocked_reasons();
        let blocked_summary = if blocked.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = blocked
                .iter()
                .take(5)
                .map(|(id, deps)| {
                    let deps_names: Vec<String> = deps
                        .iter()
                        .map(|d| self.data.nodes.get(d).map(|x| x.title.clone()).unwrap_or_default())
                        .collect();
                    format!(
                        "{}（等待前序 {} 完成）",
                        self.data.nodes.get(id).map(|x| x.title.as_str()).unwrap_or_default(),
                        deps_names.join("、")
                    )
                })
                .collect();
            format!("\n- 阻塞中：{}", parts.join("；"))
        };
        // 一级任务（任务线）列表
        let top_level: Vec<String> = self
            .top_level_ids()
            .iter()
            .take(8)
            .map(|id| self.data.nodes.get(id).map(|n| n.title.clone()).unwrap_or_default())
            .collect();
        let top_summary = if top_level.is_empty() {
            String::new()
        } else {
            format!("- 一级任务（任务线）：{}\n", top_level.join("、"))
        };
        format!(
            "任务图状态：\n- 需求：{}\n- 节点：{} ｜ 完成：{} ｜ 进行中：{} ｜ 阻塞：{}\n{}- 关键路径：{}\n{}\n- 执行顺序：{}\n- 当前可执行任务：\n{}{}",
            self.data.requirement,
            total,
            done,
            ip,
            blk,
            top_summary,
            cp_summary,
            parallel_summary,
            order_summary,
            if ready.is_empty() { "  （无）".to_string() } else { ready.join("\n") },
            blocked_summary
        )
    }

    /// 精简版任务图状态（agent 每轮注入用，控制上下文占用）：
    /// 只保留需求、统计、当前执行中、下一步可执行任务，去掉冗长的关键路径/顺序描述
    pub fn review_summary_compact(&self) -> String {
        let (total, done, ip, blk) = self.stats();
        let ready: Vec<String> = self
            .next_tasks(8)
            .into_iter()
            .map(|id| {
                let n = self.data.nodes.get(&id).unwrap();
                format!("- {}（{}）", n.title, n.id)
            })
            .collect();
        let in_progress: Vec<String> = self
            .data
            .nodes
            .values()
            .filter(|n| n.status == "in_progress")
            .map(|n| n.title.clone())
            .collect();
        let ip_summary = if in_progress.is_empty() {
            "无".to_string()
        } else {
            in_progress.join("、")
        };
        // 执行焦点（若与 in_progress 不同则额外标注）
        let focus_summary = match self.focus_task() {
            Some(n) => format!("当前焦点：{}（{}）", n.title, n.id),
            None => String::new(),
        };
        // 一级任务（任务线）列表
        let top_level: Vec<String> = self
            .top_level_ids()
            .iter()
            .take(8)
            .map(|id| self.data.nodes.get(id).map(|n| n.title.clone()).unwrap_or_default())
            .collect();
        let top_summary = if top_level.is_empty() {
            String::new()
        } else {
            format!("一级任务：{}\n", top_level.join("、"))
        };
        format!(
            "需求：{}\n节点：{} ｜ 完成：{} ｜ 进行中：{} ｜ 阻塞：{}\n{}\n当前执行中：{}\n{}\n下一步可执行：\n{}",
            self.data.requirement,
            total,
            done,
            ip,
            blk,
            top_summary,
            ip_summary,
            focus_summary,
            if ready.is_empty() { "  （无，需先完成前序任务）".to_string() } else { ready.join("\n") },
        )
    }

    pub fn to_markdown(&self) -> String {
        let mut lines = vec![
            format!("# 任务图：{}", self.data.requirement),
            String::new(),
        ];
        let root = self.data.nodes.get(&self.data.root_id);
        let root_id = root.map(|r| r.id.as_str()).unwrap_or("");
        let mut children: HashMap<String, Vec<&TaskNode>> = HashMap::new();
        for node in self.data.nodes.values() {
            children
                .entry(node.parent_id.clone())
                .or_default()
                .push(node);
        }
        fn walk(
            nodes: &HashMap<String, Vec<&TaskNode>>,
            nid: &str,
            depth: usize,
            lines: &mut Vec<String>,
        ) {
            if let Some(children) = nodes.get(nid) {
                for n in children {
                    let indent = "  ".repeat(depth);
                    let icon = match n.status.as_str() {
                        "done" => "✅",
                        "in_progress" => "🔵",
                        "blocked" => "⛔",
                        _ => "⬜",
                    };
                    let dep = if n.deps.is_empty() {
                        String::new()
                    } else {
                        format!("（前序: {}）", n.deps.join(", "))
                    };
                    lines.push(format!("{indent}- {icon} {}{dep}", n.title));
                    if !n.detail.is_empty() {
                        lines.push(format!("{indent}  {}", n.detail));
                    }
                    walk(nodes, &n.id, depth + 1, lines);
                }
            }
        }
        walk(&children, root_id, 0, &mut lines);
        // 独立一级任务（parent_id == ""，与 root 平级的新任务线）单独输出
        let mut top_ids: Vec<String> = self
            .data
            .nodes
            .values()
            .filter(|n| n.parent_id.is_empty() && n.id != self.data.root_id)
            .map(|n| n.id.clone())
            .collect();
        top_ids.sort_by_key(|id| self.data.nodes.get(id).map(|n| n.created).unwrap_or(0));
        for tid in top_ids {
            walk(&children, &tid, 0, &mut lines);
        }
        lines.join("\n")
    }

    /// 树形布局：按层级分配 y，同层按顺序均匀分配 x
    pub fn auto_layout(&mut self) {
        let mut depths: HashMap<String, usize> = HashMap::new();
        let mut queue = vec![self.data.root_id.clone()];
        let mut order: Vec<String> = Vec::new();
        while let Some(nid) = queue.pop() {
            let d = depths.get(&nid).copied().unwrap_or(0);
            order.push(nid.clone());
            let mut children: Vec<String> = self
                .data
                .nodes
                .values()
                .filter(|n| n.parent_id == nid)
                .map(|n| n.id.clone())
                .collect();
            children.sort_by_key(|id| self.data.nodes.get(id).map(|n| n.created).unwrap_or(0));
            for c in children {
                depths.insert(c.clone(), d + 1);
                queue.push(c);
            }
        }
        // 同层节点计数
        let mut level_count: HashMap<usize, usize> = HashMap::new();
        for nid in &order {
            let d = depths.get(nid).copied().unwrap_or(0);
            *level_count.entry(d).or_insert(0) += 1;
        }
        let mut level_idx: HashMap<usize, usize> = HashMap::new();
        for nid in &order {
            let d = depths.get(nid).copied().unwrap_or(0);
            let idx = level_idx.entry(d).or_insert(0);
            let count = level_count.get(&d).copied().unwrap_or(1).max(1);
            let x = (*idx * 260) as f64 - ((count - 1) * 130) as f64;
            let y = (d * 130) as f64;
            *idx += 1;
            if let Some(n) = self.data.nodes.get_mut(nid) {
                n.pos = [x, y];
            }
        }
        self.touch();
    }

    pub fn snapshot(&self) -> TaskMapData {
        self.data.clone()
    }

    pub fn restore(&mut self, snap: TaskMapData) {
        self.data = snap;
        self.last_keys.clear();
        self.user_modified = true;
        self.active_focus = None;
        // 从恢复的数据重建层级编号计数器（快照可能来自不同状态）
        self.rebuild_id_seq();
    }

    fn touch(&mut self) {
        self.data.updated = Self::now();
    }
}

// ---------------- plan_* 工具派发 ----------------

/// 生成最近一次插入任务的 key → ID 映射提示（供 AI 后续 plan_link/plan_update 引用）
fn key_map_hint(tm: &TaskMap) -> String {
    let km = tm.last_key_map();
    if km.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = km.iter().map(|(k, id)| (k.clone(), id.clone())).collect();
    pairs.sort();
    let parts: Vec<String> = pairs.iter().map(|(k, id)| format!("{k} → {id}")).collect();
    format!("\n\n本次任务 key → ID 映射：{}", parts.join("，"))
}

/// 执行 plan_* 工具；返回给模型的结果文本
pub fn plan_dispatch(tm: &mut Option<TaskMap>, name: &str, args: &Value) -> Result<String, String> {
    let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match name {
        "plan_init" => {
            if tm.is_some() {
                return Err("已存在任务图，如需修改需求请使用 plan_requirement".into());
            }
            let requirement = get("requirement");
            let breakdown = args.get("breakdown").cloned().unwrap_or(json!([]));
            // ★ 小步规划约束：顶层任务最多 5 个，避免一次生成大树导致 JSON 出错
            if let Some(arr) = breakdown.as_array() {
                if arr.len() > 5 {
                    return Err(format!(
                        "plan_init 的顶层任务不能超过 5 个（当前 {} 个）。请先规划少量顶层任务创建骨架，再用 plan_breakdown 逐层细化。",
                        arr.len()
                    ));
                }
            }
            let new_tm = TaskMap::create(&requirement, &breakdown)?;
            let summary = new_tm.review_summary();
            let key_hint = key_map_hint(&new_tm);
            *tm = Some(new_tm);
            Ok(format!("✅ 任务图已创建（可继续用 plan_breakdown 细化）\n\n{summary}{key_hint}"))
        }
        _ => {
            let task_map = tm.as_mut().ok_or("尚未创建任务图，请先调用 plan_init（参数 requirement）")?;
            match name {
                "plan_breakdown" => {
                    let parent_id = {
                        let p = get("parent_id");
                        if p.is_empty() || p == "root" {
                            // 省略/root：添加到 root 下 → 新的一级任务（兼容旧行为）
                            task_map.data.root_id.clone()
                        } else if p == "top" || p == "top_level" || p == "顶层" {
                            // top：创建与 root 平级的独立一级任务线（多一级任务）
                            String::new()
                        } else {
                            p
                        }
                    };
                    let tasks: Vec<Value> = args.get("tasks").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                    if tasks.is_empty() {
                        return Err("缺少参数：tasks（[{title, detail, children?}, ...]）".into());
                    }
                    // 单批任务同样限制，避免一次插入过多节点（小步规划）
                    if tasks.len() > 10 {
                        return Err(format!(
                            "plan_breakdown 单批任务不能超过 10 个（当前 {} 个）。请分批添加，每批 3-5 个为宜。",
                            tasks.len()
                        ));
                    }
                    task_map.push_history();
                    let ids = task_map.add_children(&parent_id, &tasks)?;
                    let names: Vec<String> = ids
                        .iter()
                        .map(|id| task_map.get_node(id).map(|n| n.title.clone()).unwrap_or_default())
                        .collect();
                    let key_hint = key_map_hint(task_map);
                    Ok(format!("✅ 已新增 {} 个任务：{}\n\n{}", names.len(), names.join("、"), task_map.review_summary_compact() + &key_hint))
                }
                "plan_update" => {
                    // ★ 批量更新：tasks: [{task_id, status?, note?, progress?}]；兼容单任务参数
                    let tasks_arg = args.get("tasks").and_then(|t| t.as_array()).cloned();
                    let mut updates: Vec<Value> = Vec::new();
                    if let Some(list) = tasks_arg {
                        updates = list;
                    } else {
                        let tid = get("task_id");
                        if tid.is_empty() {
                            return Err("缺少参数：task_id（或 tasks 数组）".into());
                        }
                        let mut item = json!({"task_id": tid});
                        if let Some(s) = args.get("status") { item["status"] = s.clone(); }
                        if let Some(n) = args.get("note") { item["note"] = n.clone(); }
                        if let Some(p) = args.get("progress") { item["progress"] = p.clone(); }
                        updates.push(item);
                    }
                    if updates.is_empty() {
                        return Err("缺少参数：tasks 数组不能为空".into());
                    }
                    task_map.push_history();
                    let mut done_lines: Vec<String> = Vec::new();
                    let mut warns: Vec<String> = Vec::new();
                    for item in &updates {
                        let tid_raw = item.get("task_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if tid_raw.is_empty() {
                            return Err("tasks 中某项缺少 task_id".into());
                        }
                        // ★ 标题解析：task_id 可以是 ID 或标题（唯一匹配自动解析）
                        let (tid, by_title) = task_map.resolve_task_id(&tid_raw)?;
                        let status = item.get("status").and_then(|v| v.as_str());
                        let note = item.get("note").and_then(|v| v.as_str());
                        let progress = item.get("progress").and_then(|v| v.as_f64());
                        // 完成校验：标记 done 前检查是否有未完成的子任务
                        if status == Some("done") {
                            let pending = task_map.pending_children(&tid);
                            if !pending.is_empty() {
                                warns.push(format!("「{}」仍有未完成子任务：{}", task_map.get_node(&tid)?.title, pending.join("、")));
                            }
                        }
                        task_map.update_node(&tid, status, note, progress)?;
                        let title = task_map.get_node(&tid)?.title.clone();
                        let st = task_map.get_node(&tid)?.status.clone();
                        let tag = if by_title { format!("（按标题匹配 {tid}）") } else { String::new() };
                        done_lines.push(format!("「{title}」-> {st}{tag}"));
                    }
                    let warn_text = if warns.is_empty() { String::new() } else {
                        format!("\n⚠️ {}", warns.join("；"))
                    };
                    // ★ 返回值精简：一行确认即可，完整状态每轮已自动注入
                    Ok(format!("✅ 已更新 {} 个任务：{}{}", done_lines.len(), done_lines.join("；"), warn_text))
                }
                "plan_link" => {
                    let tid_raw = get("task_id");
                    if tid_raw.is_empty() {
                        return Err("缺少参数：task_id".into());
                    }
                    // 标题解析
                    let (tid, _) = task_map.resolve_task_id(&tid_raw)?;
                    let deps: Vec<String> = args
                        .get("depends_on")
                        .and_then(|d| d.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .map(|s| s.to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    // 依赖也可以按标题引用
                    let mut resolved_deps: Vec<String> = Vec::new();
                    for d in &deps {
                        let (did, _) = task_map.resolve_task_id(d)?;
                        resolved_deps.push(did);
                    }
                    task_map.push_history();
                    task_map.set_deps(&tid, &resolved_deps)?;
                    let dep_names: Vec<String> = resolved_deps
                        .iter()
                        .map(|d| task_map.get_node(d).map(|n| n.title.clone()).unwrap_or_default())
                        .collect();
                    if resolved_deps.is_empty() {
                        Ok(format!("✅ 已清除「{}」的全部顺序声明", task_map.get_node(&tid)?.title))
                    } else {
                        Ok(format!("✅ 已设置「{}」的执行顺序：先执行 {}", task_map.get_node(&tid)?.title, dep_names.join("、")))
                    }
                }
                "plan_review" => {
                    // ★ 支持按节点查看子树：node_id 省略或 root 时查看全图
                    let node_id = get("node_id");
                    if node_id.is_empty() || node_id == "root" {
                        Ok(task_map.review_summary())
                    } else {
                        let (nid, _) = task_map.resolve_task_id(&node_id)?;
                        Ok(subtree_review(task_map, &nid))
                    }
                }
                "plan_find" => {
                    let keyword = get("keyword");
                    if keyword.is_empty() {
                        return Err("缺少参数：keyword（按标题关键词搜索任务）".into());
                    }
                    let hits = task_map.find_by_title(&keyword, 10);
                    if hits.is_empty() {
                        return Ok(format!("未找到标题包含「{keyword}」的任务。"));
                    }
                    let lines: Vec<String> = hits
                        .iter()
                        .map(|(id, title, status)| {
                            let icon = match status.as_str() {
                                "done" => "✅",
                                "in_progress" => "🔵",
                                "blocked" => "⛔",
                                _ => "⬜",
                            };
                            format!("{icon} {title}（{id}）")
                        })
                        .collect();
                    Ok(format!("找到 {} 个匹配任务：\n{}", hits.len(), lines.join("\n")))
                }
                "plan_undo" => {
                    match task_map.undo() {
                        Some(_) => Ok("↩ 已回滚到最近一次修改前的任务图状态。".to_string()),
                        None => Err("没有可回滚的历史（任务图未发生过 AI 修改，或已全部回滚）".into()),
                    }
                }
                "plan_focus" => {
                    let tid_raw = get("task_id");
                    if tid_raw.is_empty() || tid_raw == "none" || tid_raw == "clear" {
                        task_map.active_focus = None;
                        return Ok("已清除执行焦点。".into());
                    }
                    let (tid, _) = task_map.resolve_task_id(&tid_raw)?;
                    let title = task_map.get_node(&tid)?.title.clone();
                    // 显式声明焦点：自动置为 in_progress（若还在 todo/blocked）
                    let status = task_map.get_node(&tid)?.status.clone();
                    if status == "todo" || status == "blocked" {
                        task_map.push_history();
                        task_map.update_node(&tid, Some("in_progress"), None, None)?;
                        return Ok(format!("🎯 已声明执行焦点：{tid}「{title}」（已自动置为 in_progress）"));
                    }
                    task_map.active_focus = Some(tid.clone());
                    Ok(format!("🎯 已声明执行焦点：{tid}「{title}」"))
                }
                "plan_requirement" => {
                    let new_req = get("new_requirement");
                    if new_req.is_empty() {
                        return Err("缺少参数：new_requirement".into());
                    }
                    task_map.push_history();
                    task_map.change_requirement(&new_req)?;
                    Ok(format!("✅ 需求已更新：{new_req}"))
                }
                "plan_move" => {
                    let tid_raw = get("task_id");
                    let new_parent_raw = {
                        let p = get("new_parent_id");
                        if p.is_empty() || p == "root" {
                            task_map.data.root_id.clone()
                        } else if p == "top" || p == "top_level" || p == "顶层" {
                            // 移动到顶层：成为独立一级任务（与 root 平级）
                            String::new()
                        } else {
                            p
                        }
                    };
                    if tid_raw.is_empty() {
                        return Err("缺少参数：task_id 和 new_parent_id".into());
                    }
                    let (tid, _) = task_map.resolve_task_id(&tid_raw)?;
                    let title = task_map.get_node(&tid)?.title.clone();
                    let parent_title = if new_parent_raw.is_empty() {
                        "顶层（一级任务）".to_string()
                    } else {
                        let (new_parent, _) = task_map.resolve_task_id(&new_parent_raw)?;
                        task_map.get_node(&new_parent)?.title.clone()
                    };
                    task_map.push_history();
                    task_map.move_node(&tid, &new_parent_raw)?;
                    Ok(format!("✅ 已移动「{title}」到「{parent_title}」下"))
                }
                "plan_delete" => {
                    let tid_raw = get("task_id");
                    if tid_raw.is_empty() {
                        return Err("缺少参数：task_id".into());
                    }
                    let (tid, _) = task_map.resolve_task_id(&tid_raw)?;
                    let title = task_map.get_node(&tid)?.title.clone();
                    task_map.push_history();
                    task_map.delete_node(&tid)?;
                    Ok(format!("✅ 已删除任务「{title}」（含子任务）；如需恢复可用 plan_undo"))
                }
                "plan_export" => Ok(task_map.to_markdown()),
                _ => Err(format!("未知任务图工具: {name}")),
            }
        }
    }
}

/// 生成以某节点为根的子树视图：需求 + 子树统计 + 树形列表 + 外部依赖
fn subtree_review(tm: &TaskMap, root_id: &str) -> String {
    let root = tm.data.nodes.get(root_id);
    let Some(root) = root else {
        return format!("任务不存在: {root_id}");
    };
    let ids = tm.subtree_ids(root_id);
    let total = ids.len();
    let done = ids.iter().filter(|id| tm.data.nodes.get(*id).map(|n| n.status == "done").unwrap_or(false)).count();
    let ip = ids.iter().filter(|id| tm.data.nodes.get(*id).map(|n| n.status == "in_progress").unwrap_or(false)).count();
    // 树形列表
    let mut children: HashMap<String, Vec<&TaskNode>> = HashMap::new();
    for id in &ids {
        if let Some(n) = tm.data.nodes.get(id) {
            children.entry(n.parent_id.clone()).or_default().push(n);
        }
    }
    for v in children.values_mut() {
        v.sort_by_key(|n| n.created);
    }
    fn walk(
        nodes: &HashMap<String, Vec<&TaskNode>>,
        nid: &str,
        depth: usize,
        lines: &mut Vec<String>,
    ) {
        if let Some(list) = nodes.get(nid) {
            for n in list {
                let icon = match n.status.as_str() {
                    "done" => "✅",
                    "in_progress" => "🔵",
                    "blocked" => "⛔",
                    _ => "⬜",
                };
                let dep = if n.deps.is_empty() {
                    String::new()
                } else {
                    format!("（依赖: {}）", n.deps.join(", "))
                };
                lines.push(format!("{}{} {}{}（{}）", "  ".repeat(depth), icon, n.title, dep, n.id));
                if !n.detail.is_empty() {
                    lines.push(format!("{}  {}", "  ".repeat(depth), n.detail));
                }
                walk(nodes, &n.id, depth + 1, lines);
            }
        }
    }
    let mut lines = vec![
        format!("📌 子树视图：{}（{}）", root.title, root_id),
        format!("📊 节点：{} ｜ 完成：{} ｜ 进行中：{}", total, done, ip),
        String::new(),
    ];
    walk(&children, root_id, 0, &mut lines);
    // 外部依赖：子树内节点依赖子树外的节点（可能阻塞）
    let ext_deps: Vec<String> = ids
        .iter()
        .filter_map(|id| tm.data.nodes.get(id))
        .flat_map(|n| n.deps.iter().map(move |d| (n.id.clone(), d.clone())))
        .filter(|(_, d)| !ids.contains(d))
        .map(|(nid, d)| {
            let t = tm.data.nodes.get(&d).map(|x| x.title.clone()).unwrap_or_default();
            format!("「{}」等待外部任务「{}」（{}）", tm.data.nodes.get(&nid).map(|x| x.title.as_str()).unwrap_or(""), t, d)
        })
        .collect();
    if !ext_deps.is_empty() {
        lines.push(String::new());
        lines.push("外部依赖：".to_string());
        lines.extend(ext_deps);
    }
    lines.join("\n")
}

/// plan 工具定义
pub fn plan_tool_definitions() -> Value {
    let f = |name: &str, desc: &str, props: Value, required: Vec<&str>| {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {
                    "type": "object",
                    "properties": props,
                    "required": required
                }
            }
        })
    };
    let s = |d: &str| json!({"type": "string", "description": d});
    // 任务项 schema：key 供 deps 引用；deps 声明执行顺序（引 key 或已有节点 ID）
    let task_item = |extra: Value| {
        let mut props = json!({
            "title": s("任务标题"),
            "detail": s("任务详情"),
            "key": s("任务标识（可选）：供本批 tasks 的 deps 引用，如 a/b/c"),
            "deps": json!({"type": "array", "items": {"type": "string"}, "description": "执行顺序声明：本任务须在列出的任务之后执行（引用本批 key 或已有节点 ID/标题）。不填表示按创建顺序与同层级任务依次竖排执行。图的上下排列就是实际执行顺序指令"}),
            "children": {"type": "array"}
        });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                props[k] = v.clone();
            }
        }
        json!({"type": "array", "items": {"type": "object", "properties": props}})
    };
    json!([
        f("plan_init", "创建任务图（工程模式第一步，只能调用一次）。★ 层级语义：任务目标节点（root）是唯一总节点（高于一级，只在一开始创建时存在）；一级任务=任务总结（对整个任务的总结/目标），挂到任务目标总节点下；二级=完成一级任务的方法步骤；三级=进一步细分，以此类推。★ breakdown 顶层任务 = 一级任务（任务总结，1-3 个），不要把具体步骤塞进顶层导致平铺。★ 任务编号规则：编号=父任务自身编号段+本级字母+本级数字（如 a1 → a1b2 → b2c1），同一字母层级数字全局不重复。★ 小步规划：顶层任务最多 5 个。★ 执行顺序语义：图排列=实际执行顺序指令（默认从上到下串行执行）；需先后执行的用 deps 声明（先执行者作为后执行者的 deps，形成串行链 → 上下排列）；当前无子代理，同级任务一律竖排，不设横向并排", json!({
            "requirement": s("用户需求描述"),
            "breakdown": task_item(json!({}))
        }), vec!["requirement"]),
        f("plan_breakdown", "在节点下添加子任务（单批最多 10 个，建议 3-5 个分批添加）。★ 一级任务（任务总结）添加：parent_id 省略或 root 时在任务目标总节点（root）下新增一级任务（任务总结）——包括多轮对话中差异大的新需求也这样添加；其它 parent_id = 在对应任务下添加子任务（方法步骤/细分）。同级任务需先后执行的用 deps 声明顺序（先执行者作为后执行者的 deps，引用本批 key 或已有节点 ID/标题）；当前无子代理，同级任务一律竖排依次执行，不设横向并行", json!({
            "parent_id": s("父任务ID：省略或 root = 在任务目标总节点下新增一级任务（任务总结）；其它 = 已有节点 ID/标题（添加方法步骤/细分）"),
            "tasks": task_item(json!({}))
        }), vec!["tasks"]),
        f("plan_update", "更新任务状态/备注/进度。★ 支持批量：tasks: [{task_id, status?, note?, progress?}]；task_id 可用节点 ID 或标题（唯一匹配自动解析）。状态流转约束：todo→in_progress→done / blocked，禁止跳转（如 todo 直接 done）", json!({
            "task_id": s("任务ID 或标题（批量时用 tasks）"),
            "status": s("todo / in_progress / done / blocked"),
            "note": s("备注"),
            "progress": json!({"type": "number", "description": "进度 0-100"}),
            "tasks": json!({"type": "array", "items": json!({"type": "object", "properties": {
                "task_id": s("任务ID 或标题"),
                "status": s("todo / in_progress / done / blocked"),
                "note": s("备注"),
                "progress": json!({"type": "number", "description": "进度 0-100"})
            }, "required": ["task_id"]}), "description": "批量更新列表（可选，使用后忽略 task_id/status 等单任务参数）"})
        }), vec![]),
        f("plan_link", "设置执行顺序：depends_on 中的任务须在本任务之前执行（决定图的上/下排列，即执行指令）。depends_on 传空数组 [] 可清除全部顺序声明；task_id 与依赖项都可用标题。当前无子代理：同级任务默认竖排依次执行，depends_on 用于调整先后顺序", json!({
            "task_id": s("任务ID 或标题"),
            "depends_on": json!({"type": "array", "items": {"type": "string"}, "description": "本任务之前执行的任务ID或标题列表（可为空数组清除全部）"})
        }), vec!["task_id"]),
        f("plan_review", "查看任务图当前状态（含关键路径、并行机会、阻塞原因、可执行任务；输出含任务层级编号，如 a1/a1b2/b2c1）。支持 node_id 参数只看某节点子树", json!({
            "node_id": s("可选：只看该节点子树（ID 或标题），省略或 root 查看全图")
        }), vec![]),
        f("plan_find", "按标题关键词搜索任务，返回匹配的节点 ID（供 plan_update/plan_link 等引用）", json!({
            "keyword": s("标题关键词")
        }), vec!["keyword"]),
        f("plan_undo", "回滚到最近一次修改前的任务图状态（AI 工具误操作可恢复，最多 20 步）", json!({}), vec![]),
        f("plan_focus", "声明当前正在执行的任务（执行焦点）。工作类工具（写文件/执行命令等）在工程模式下要求存在执行焦点；也可用于在多个任务间切换", json!({
            "task_id": s("任务ID 或标题；传 none/clear 清除焦点")
        }), vec![]),
        f("plan_requirement", "更新任务图需求", json!({"new_requirement": s("新需求")}), vec!["new_requirement"]),
        f("plan_move", "移动任务到新父节点；new_parent_id 传 top/top_level 可移动到顶层成为独立一级任务", json!({"task_id": s("任务ID 或标题"), "new_parent_id": s("新父任务ID 或标题；top/top_level = 顶层一级任务")}), vec!["task_id", "new_parent_id"]),
        f("plan_delete", "删除任务（含子任务）。误删可用 plan_undo 恢复", json!({"task_id": s("任务ID 或标题")}), vec!["task_id"]),
        f("plan_export", "导出任务图为 Markdown", json!({}), vec![]),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tm() -> TaskMap {
        let breakdown = json!([
            {"title": "分析需求", "detail": "读懂用户需求", "children": [{"title": "阅读文档", "detail": ""}]},
            {"title": "实现功能", "detail": ""}
        ]);
        TaskMap::create("做一个任务图系统", &breakdown).unwrap()
    }

    #[test]
    fn create_and_breakdown() {
        let t = tm();
        let (total, _, _, _) = t.stats();
        assert_eq!(total, 4); // root + 2 顶层 + 1 子任务
        assert_eq!(t.data.requirement, "做一个任务图系统");
        // root = 任务目标总节点：标题固定「任务目标」，说明放完整需求
        let root = t.data.nodes.get(&t.data.root_id).unwrap();
        assert_eq!(root.title, "任务目标");
        assert_eq!(root.detail, "做一个任务图系统");
    }

    #[test]
    fn plan_requirement_syncs_root_title() {
        let mut opt = None;
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "旧需求", "breakdown": []})).unwrap();
        plan_dispatch(&mut opt, "plan_requirement", &json!({"new_requirement": "新任务目标：优化任务图界面"})).unwrap();
        let t = opt.as_ref().unwrap();
        let root = t.data.nodes.get(&t.data.root_id).unwrap();
        assert_eq!(t.data.requirement, "新任务目标：优化任务图界面");
        assert_eq!(root.title, "任务目标", "plan_requirement 后 root 标题保持「任务目标」总节点标识");
        assert_eq!(root.detail, "新任务目标：优化任务图界面", "plan_requirement 应同步 root 说明为完整需求");
    }

    #[test]
    fn legacy_root_title_fixed_on_load() {
        // 旧数据 root 各种标题：from_data 加载时应统一为「任务目标」总节点 + 完整需求说明
        let now = 1700000000000i64;
        let mut data = TaskMapData {
            version: 1,
            requirement: "旧任务目标描述".into(),
            root_id: "root".into(),
            nodes: HashMap::new(),
            changelog: vec![],
            created: now,
            updated: now,
        };
        data.nodes.insert(
            "root".into(),
            TaskNode {
                id: "root".into(),
                title: "旧的任务目标摘要".into(),
                detail: "围绕用户需求的总目标".into(),
                status: "todo".into(),
                progress: 0.0,
                note: String::new(),
                parent_id: String::new(),
                deps: vec![],
                pos: [0.0, 0.0],
                created: now,
                started_at: None,
                finished_at: None,
            },
        );
        let t = TaskMap::from_data(data);
        let root = t.data.nodes.get(&t.data.root_id).unwrap();
        assert_eq!(root.title, "任务目标", "旧数据 root 标题应统一为「任务目标」总节点");
        assert_eq!(root.detail, "旧任务目标描述", "旧数据 root 说明应同步为完整需求");
    }

    #[test]
    fn plan_lifecycle() {
        let mut t = tm();
        // 找"实现功能"节点
        let target = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        let target2 = t.data.nodes.values().find(|n| n.title == "阅读文档").unwrap().id.clone();
        // 更新状态
        t.update_node(&target, Some("in_progress"), Some("开始"), Some(30.0)).unwrap();
        assert_eq!(t.get_node(&target).unwrap().status, "in_progress");
        assert_eq!(t.get_node(&target).unwrap().progress, 30.0);
        // 设置依赖：实现功能依赖阅读文档
        t.set_deps(&target, &[target2.clone()]).unwrap();
        assert_eq!(t.get_node(&target).unwrap().deps, vec![target2.clone()]);
        // 环检测
        assert!(t.set_deps(&target2, &[target.clone()]).is_err());
        // 删除
        t.delete_node(&target).unwrap();
        assert!(t.get_node(&target).is_err());
    }

    #[test]
    fn snapshot_restore() {
        let mut t = tm();
        let snap = t.snapshot();
        let target = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        t.update_node(&target, Some("in_progress"), None, None).unwrap();
        t.update_node(&target, Some("done"), None, None).unwrap();
        t.restore(snap);
        assert_eq!(t.get_node(&target).unwrap().status, "todo");
    }

    #[test]
    fn next_tasks_ready() {
        let mut t = tm();
        let analyze = t.data.nodes.values().find(|n| n.title == "分析需求").unwrap().id.clone();
        let read = t.data.nodes.values().find(|n| n.title == "阅读文档").unwrap().id.clone();
        let implement = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        // 依赖链：阅读文档 <- 分析需求；实现功能 <- 阅读文档
        t.set_deps(&read, &[analyze.clone()]).unwrap();
        t.set_deps(&implement, &[read.clone()]).unwrap();
        // 初始只有"分析需求"就绪
        let ready = t.next_tasks(10);
        assert!(ready.contains(&analyze));
        assert!(!ready.contains(&implement));
        // 完成分析需求 → 阅读文档就绪
        t.update_node(&analyze, Some("in_progress"), None, None).unwrap();
        t.update_node(&analyze, Some("done"), None, None).unwrap();
        let ready2 = t.next_tasks(10);
        assert!(ready2.contains(&read));
        // 完成阅读文档 → 实现功能就绪
        t.update_node(&read, Some("in_progress"), None, None).unwrap();
        t.update_node(&read, Some("done"), None, None).unwrap();
        let ready3 = t.next_tasks(10);
        assert!(ready3.contains(&implement));
    }

    // ---------- deps 声明 / 关键路径 / review 增强 ----------

    #[test]
    fn breakdown_with_deps_creates_links() {
        // 用 key + deps 在创建时直接声明依赖：
        // 阅读文档 依赖 分析需求；实现功能 依赖 阅读文档 → 3 步串行链
        let breakdown = json!([
            {"key": "analyze", "title": "分析需求", "children": [{"key": "read", "title": "阅读文档", "deps": ["analyze"]}]},
            {"key": "implement", "title": "实现功能", "deps": ["read"]}
        ]);
        let t = TaskMap::create("做一个任务图系统", &breakdown).unwrap();
        let impl_node = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap();
        let analyze = t.data.nodes.values().find(|n| n.title == "分析需求").unwrap();
        let read = t.data.nodes.values().find(|n| n.title == "阅读文档").unwrap();
        assert_eq!(impl_node.deps, vec![read.id.clone()]);
        assert_eq!(read.deps, vec![analyze.id.clone()]);
        // key 映射已记录（供 AI 后续引用）
        assert_eq!(t.last_key_map().get("implement").map(|s| s.as_str()), Some(impl_node.id.as_str()));
        assert_eq!(t.last_key_map().get("analyze").map(|s| s.as_str()), Some(analyze.id.as_str()));
        // 关键路径 = 分析需求 → 阅读文档 → 实现功能（3 步串行链）
        let cp = t.critical_path();
        assert_eq!(cp.len(), 3);
        assert_eq!(cp[0], analyze.id);
        assert_eq!(cp[1], read.id);
        assert_eq!(cp[2], impl_node.id);
    }

    #[test]
    fn duplicate_key_rejected() {
        let breakdown = json!([
            {"key": "a", "title": "任务A"},
            {"key": "a", "title": "任务B"}
        ]);
        let err = TaskMap::create("测试", &breakdown).unwrap_err();
        assert!(err.contains("key 重复"));
    }

    #[test]
    fn deps_referencing_unknown_key_rejected() {
        let breakdown = json!([
            {"key": "a", "title": "任务A", "deps": ["ghost"]}
        ]);
        let err = TaskMap::create("测试", &breakdown).unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[test]
    fn deps_cycle_detected() {
        let breakdown = json!([
            {"key": "a", "title": "任务A", "deps": ["b"]},
            {"key": "b", "title": "任务B", "deps": ["a"]}
        ]);
        let err = TaskMap::create("测试", &breakdown).unwrap_err();
        assert!(err.contains("环"));
    }

    #[test]
    fn critical_path_returns_longest_chain() {
        let mut t = tm();
        let analyze = t.data.nodes.values().find(|n| n.title == "分析需求").unwrap().id.clone();
        let read = t.data.nodes.values().find(|n| n.title == "阅读文档").unwrap().id.clone();
        let implement = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        // 长链：分析需求 → 阅读文档 → 实现功能（3 步）
        t.set_deps(&read, &[analyze.clone()]).unwrap();
        t.set_deps(&implement, &[read.clone()]).unwrap();
        let cp = t.critical_path();
        assert_eq!(cp, vec![analyze.clone(), read.clone(), implement.clone()]);
        // 无依赖时为空（全部可并行）
        let t2 = tm();
        assert!(t2.critical_path().is_empty());
    }

    #[test]
    fn review_summary_contains_path_and_parallel() {
        let mut t = tm();
        let analyze = t.data.nodes.values().find(|n| n.title == "分析需求").unwrap().id.clone();
        let read = t.data.nodes.values().find(|n| n.title == "阅读文档").unwrap().id.clone();
        let implement = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        t.set_deps(&read, &[analyze.clone()]).unwrap();
        t.set_deps(&implement, &[read.clone()]).unwrap();
        let s = t.review_summary();
        assert!(s.contains("关键路径"), "summary 应包含关键路径: {s}");
        assert!(s.contains("就绪任务"), "summary 应包含就绪任务: {s}");
        assert!(s.contains("分析需求"), "关键路径应含任务名: {s}");
        // 阻塞原因：实现功能 等待 阅读文档 完成
        let blocked = t.blocked_reasons();
        assert!(!blocked.is_empty());
    }

    #[test]
    fn blocked_reasons_lists_pending_deps() {
        let mut t = tm();
        let analyze = t.data.nodes.values().find(|n| n.title == "分析需求").unwrap().id.clone();
        let read = t.data.nodes.values().find(|n| n.title == "阅读文档").unwrap().id.clone();
        let implement = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        t.set_deps(&read, &[analyze.clone()]).unwrap();
        t.set_deps(&implement, &[read.clone()]).unwrap();
        let reasons = t.blocked_reasons();
        // 阅读文档 与 实现功能 都被阻塞
        assert!(reasons.iter().any(|(id, deps)| *id == read && deps.contains(&analyze)));
        assert!(reasons.iter().any(|(id, deps)| *id == implement && deps.contains(&read)));
    }

    // ---------- 执行时间戳 / 完成校验 / 精简 review ----------

    #[test]
    fn update_node_records_timestamps() {
        let mut t = tm();
        let node_id = t.data.nodes.values().find(|n| n.title == "分析需求").unwrap().id.clone();
        // in_progress 记录开始时间
        t.update_node(&node_id, Some("in_progress"), None, None).unwrap();
        let n = t.data.nodes.get(&node_id).unwrap();
        assert!(n.started_at.is_some(), "in_progress 应记录 started_at");
        assert!(n.finished_at.is_none());
        // done 记录完成时间，进度自动 100
        t.update_node(&node_id, Some("done"), None, None).unwrap();
        let n = t.data.nodes.get(&node_id).unwrap();
        assert!(n.finished_at.is_some(), "done 应记录 finished_at");
        assert_eq!(n.progress, 100.0);
        assert!(n.started_at.is_some(), "started_at 首次进入后保留");
        // 从 done 退回 todo：清空完成时间
        t.update_node(&node_id, Some("todo"), None, None).unwrap();
        let n = t.data.nodes.get(&node_id).unwrap();
        assert!(n.finished_at.is_none(), "退回后应清空 finished_at");
    }

    #[test]
    fn pending_children_lists_unfinished_subtasks() {
        let mut t = tm();
        // "实现功能"无子节点；"分析需求"有子节点"阅读文档"
        let parent = t.data.nodes.values().find(|n| n.title == "分析需求").unwrap().id.clone();
        let leaf = t.data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        // 叶子节点无未完成子任务
        assert!(t.pending_children(&leaf).is_empty());
        // 父任务有未完成的子任务（阅读文档 todo）
        let pending = t.pending_children(&parent);
        assert_eq!(pending, vec!["阅读文档".to_string()]);
        // 完成后不再列出
        let child = t.data.nodes.values().find(|n| n.title == "阅读文档").unwrap().id.clone();
        t.update_node(&child, Some("in_progress"), None, None).unwrap();
        t.update_node(&child, Some("done"), None, None).unwrap();
        assert!(t.pending_children(&parent).is_empty());
    }

    #[test]
    fn compact_review_contains_essentials() {
        let t = tm();
        let s = t.review_summary_compact();
        assert!(s.contains("需求"));
        assert!(s.contains("下一步可执行"));
        assert!(s.contains("当前执行中"));
        // 精简版不应包含冗长的关键路径/执行顺序段
        assert!(!s.contains("关键路径"));
    }

    #[test]
    fn from_data_marks_user_modified_false() {
        let t = tm();
        // 前端同步的全新数据 → user_modified 由 sync_memory 显式控制，from_data 本身为 false
        assert!(!t.user_modified);
        // restore 后应标记 user_modified（视为用户改动恢复）
        let mut t2 = tm();
        let snap = t2.snapshot();
        t2.restore(snap);
        assert!(t2.user_modified, "restore 后应标记 user_modified");
    }

    // ---------- P0/P1/P2/P3 深度适配 ----------

    #[test]
    fn update_batch_and_compact_return() {
        let mut opt = None;
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "批量测试", "breakdown": [
            {"key": "a", "title": "分析需求"},
            {"key": "b", "title": "实现功能"}
        ]})).unwrap();
        // 从任务图中取节点 ID（key 不是 ID，须从 last_keys/节点查找）
        let ids: Vec<String> = {
            let t = opt.as_ref().unwrap();
            vec![t.data.root_id.clone()] // 占位
        };
        let a_id = opt.as_ref().unwrap().data.nodes.values().find(|n| n.title == "分析需求").unwrap().id.clone();
        let b_id = opt.as_ref().unwrap().data.nodes.values().find(|n| n.title == "实现功能").unwrap().id.clone();
        let _ = ids;
        // 批量更新两个任务
        let args = json!({
            "tasks": [
                {"task_id": a_id, "status": "in_progress"},
                {"task_id": b_id, "status": "blocked"}
            ]
        });
        let out = plan_dispatch(&mut opt, "plan_update", &args).unwrap();
        // 返回值精简：一行确认，不含完整 review（无"关键路径"）
        assert!(out.contains("已更新 2 个任务"));
        assert!(!out.contains("关键路径"), "批量更新返回应精简: {out}");
        let tm = opt.as_ref().unwrap();
        assert!(tm.data.nodes.get(&a_id).unwrap().status == "in_progress");
        assert!(tm.data.nodes.get(&b_id).unwrap().status == "blocked");
    }

    #[test]
    fn plan_init_limits_top_level_tasks() {
        let mut opt = None;
        let breakdown: Vec<Value> = (0..6).map(|i| json!({"title": format!("任务{i}")})).collect();
        let err = plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "x", "breakdown": breakdown})).unwrap_err();
        assert!(err.contains("不能超过 5 个"), "应提示顶层任务上限: {err}");
        assert!(opt.is_none(), "失败时不应创建任务图");
    }

    #[test]
    fn invalid_state_transition_rejected() {
        let mut opt = None;
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "x", "breakdown": [{"title": "任务A"}]})).unwrap();
        let id = opt.as_ref().unwrap().data.nodes.values().find(|n| n.title == "任务A").unwrap().id.clone();
        // todo → done 直接跳转：非法
        let err = plan_dispatch(&mut opt, "plan_update", &json!({"task_id": id, "status": "done"})).unwrap_err();
        assert!(err.contains("非法状态流转"), "应拒绝跳转: {err}");
        // todo → in_progress → done：合法
        plan_dispatch(&mut opt, "plan_update", &json!({"task_id": id, "status": "in_progress"})).unwrap();
        plan_dispatch(&mut opt, "plan_update", &json!({"task_id": id, "status": "done"})).unwrap();
        assert_eq!(opt.as_ref().unwrap().data.nodes.get(&id).unwrap().status, "done");
        // done → todo（重做）：合法
        plan_dispatch(&mut opt, "plan_update", &json!({"task_id": id, "status": "todo"})).unwrap();
    }

    #[test]
    fn resolve_task_id_by_title() {
        let t = tm();
        let (id, by_title) = t.resolve_task_id("分析需求").unwrap();
        assert!(by_title, "应按标题解析");
        assert_eq!(t.data.nodes.get(&id).unwrap().title, "分析需求");
        // ID 直接解析
        let (id2, by_title2) = t.resolve_task_id(&id).unwrap();
        assert!(!by_title2);
        assert_eq!(id2, id);
        // 包含匹配多个 → 报错（"需求"匹配"分析需求"和"读懂用户需求"detail 不含，只匹配标题，应为唯一）
        assert!(t.resolve_task_id("不存在").is_err());
        // 多匹配：标题包含"阅读文档"的唯一，但"需求"仅匹配"分析需求"
        let (id3, _) = t.resolve_task_id("阅读").unwrap();
        assert_eq!(t.data.nodes.get(&id3).unwrap().title, "阅读文档");
    }

    #[test]
    fn resolve_title_ambiguous_reports_candidates() {
        // 构造两个含"任务"的标题 → 应报多个候选
        let breakdown = json!([
            {"title": "任务A"},
            {"title": "任务B"}
        ]);
        let t = TaskMap::create("x", &breakdown).unwrap();
        let err = t.resolve_task_id("任务").unwrap_err();
        assert!(err.contains("匹配到多个任务"), "应报多候选: {err}");
    }

    #[test]
    fn plan_find_searches_by_title() {
        let mut opt = None;
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "x", "breakdown": [
            {"title": "前端开发"},
            {"title": "后端开发"}
        ]})).unwrap();
        let out = plan_dispatch(&mut opt, "plan_find", &json!({"keyword": "开发"})).unwrap();
        assert!(out.contains("前端开发"), "应找到前端开发: {out}");
        assert!(out.contains("后端开发"), "应找到后端开发: {out}");
        assert!(out.contains("a1"), "应包含层级编号 a1: {out}");
        assert!(out.contains("a2"), "应包含层级编号 a2: {out}");
    }

    // ---------- 层级编号 / 旧数据迁移 ----------

    #[test]
    fn hierarchical_ids_generated_by_level() {
        let mut opt = None;
        // 一级任务：a1/a2（root 直属目标层）
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "x", "breakdown": [
            {"title": "目标A"},
            {"title": "目标B"}
        ]})).unwrap();
        let t = opt.as_ref().unwrap();
        let a = t.data.nodes.values().find(|n| n.title == "目标A").unwrap();
        let b = t.data.nodes.values().find(|n| n.title == "目标B").unwrap();
        assert_eq!(a.id, "a1", "第一个一级任务应为 a1");
        assert_eq!(b.id, "a2", "第二个一级任务应为 a2");
        // 二级任务：父自身段 + 本级字母 b + 全局递增数字
        plan_dispatch(&mut opt, "plan_breakdown", &json!({"parent_id": "a1", "tasks": [
            {"title": "步骤A1"},
            {"title": "步骤A2"}
        ]})).unwrap();
        plan_dispatch(&mut opt, "plan_breakdown", &json!({"parent_id": "a2", "tasks": [
            {"title": "步骤B1"}
        ]})).unwrap();
        let t = opt.as_ref().unwrap();
        let s1 = t.data.nodes.values().find(|n| n.title == "步骤A1").unwrap();
        let s2 = t.data.nodes.values().find(|n| n.title == "步骤A2").unwrap();
        let s3 = t.data.nodes.values().find(|n| n.title == "步骤B1").unwrap();
        assert_eq!(s1.id, "a1b1", "a1 的第一个子任务应为 a1b1");
        assert_eq!(s2.id, "a1b2", "a1 的第二个子任务应为 a1b2");
        assert_eq!(s3.id, "a2b3", "b 层数字全局递增不重复，a2 的子任务应为 a2b3");
        // 三级任务：只保留父自身段（b1）+ 本级字母 c
        plan_dispatch(&mut opt, "plan_breakdown", &json!({"parent_id": "a1b1", "tasks": [
            {"title": "细分C1"}
        ]})).unwrap();
        let t = opt.as_ref().unwrap();
        let c1 = t.data.nodes.values().find(|n| n.title == "细分C1").unwrap();
        assert_eq!(c1.id, "b1c1", "a1b1 的子任务应为 b1c1（父自身段 b1 + c1），不携带祖父编号");
        // 每层数字不重复：一级任务（parent 为 root 或空）共 2 个
        let a_count = t
            .data
            .nodes
            .values()
            .filter(|n| n.id != "root" && (n.parent_id == "root" || n.parent_id.is_empty()))
            .count();
        assert_eq!(a_count, 2);
    }

    #[test]
    fn legacy_ids_migrated_to_hierarchical() {
        // 构造旧格式数据（root=n1，一级=n2/n3，n2 的子任务 n4），模拟 DB 旧任务图
        fn legacy_node(id: &str, title: &str, parent: &str, created: i64) -> TaskNode {
            TaskNode {
                id: id.into(),
                title: title.into(),
                detail: String::new(),
                status: "todo".into(),
                progress: 0.0,
                note: String::new(),
                parent_id: parent.into(),
                deps: vec![],
                pos: [0.0, 0.0],
                created,
                started_at: None,
                finished_at: None,
            }
        }
        let mut created = 1700000000000i64;
        let mut data = TaskMapData {
            version: 1,
            requirement: "旧数据迁移测试".into(),
            root_id: "n1".into(),
            nodes: HashMap::new(),
            changelog: vec![],
            created,
            updated: created,
        };
        created += 1;
        data.nodes.insert("n1".into(), legacy_node("n1", "任务目标", "", created));
        created += 1;
        data.nodes.insert("n2".into(), legacy_node("n2", "目标A", "n1", created));
        created += 1;
        data.nodes.insert("n3".into(), legacy_node("n3", "目标B", "n1", created));
        created += 1;
        data.nodes.insert("n4".into(), legacy_node("n4", "步骤A1", "n2", created));
        // n4 依赖 n3（引用也要迁移）
        data.nodes.get_mut("n4").unwrap().deps = vec!["n3".into()];
        // 独立一级任务线（parent_id == ""）
        created += 1;
        data.nodes.insert("n5".into(), legacy_node("n5", "独立线", "", created));
        let t = TaskMap::from_data(data);
        // root 固定为 "root"
        assert_eq!(t.data.root_id, "root");
        assert!(t.data.nodes.contains_key("root"), "root 应迁移为固定 ID");
        // 一级任务：root 直属 + 独立线按创建顺序 → a1/a2/a3
        let n2 = t.data.nodes.values().find(|n| n.title == "目标A").unwrap();
        let n3 = t.data.nodes.values().find(|n| n.title == "目标B").unwrap();
        let n5 = t.data.nodes.values().find(|n| n.title == "独立线").unwrap();
        assert_eq!(n2.id, "a1");
        assert_eq!(n3.id, "a2");
        assert_eq!(n5.id, "a3", "独立任务线也应迁移为 a 层编号");
        assert_eq!(n5.parent_id, "", "独立任务线 parent_id 保持为空");
        // 二级：n2 的子任务 → a1b1
        let n4 = t.data.nodes.values().find(|n| n.title == "步骤A1").unwrap();
        assert_eq!(n4.id, "a1b1");
        assert_eq!(n4.parent_id, "a1");
        // deps 引用同步迁移：n4 依赖 n3 → a1b1 依赖 a2
        assert_eq!(n4.deps, vec!["a2".to_string()], "deps 引用应同步迁移");
        // 无残留旧编号
        assert!(!t.data.nodes.keys().any(|k| k.starts_with('n')), "不应残留旧编号: {:?}", t.data.nodes.keys());
    }

    #[test]
    fn plan_review_subtree_and_undo() {
        let mut opt = None;
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "x", "breakdown": [
            {"key": "a", "title": "分析", "children": [{"key": "a1", "title": "阅读"}]},
            {"key": "b", "title": "实现"}
        ]})).unwrap();
        // 子树视图：node_id 用标题（key 不是节点 ID）
        let out = plan_dispatch(&mut opt, "plan_review", &json!({"node_id": "分析"})).unwrap();
        assert!(out.contains("子树视图"), "应输出子树视图: {out}");
        assert!(out.contains("阅读"), "子树应含子节点: {out}");
        assert!(!out.contains("实现"), "子树不应含其它分支: {out}");
        // 删除后 undo 恢复
        let b_id = opt.as_ref().unwrap().data.nodes.values().find(|n| n.title == "实现").unwrap().id.clone();
        plan_dispatch(&mut opt, "plan_delete", &json!({"task_id": b_id})).unwrap();
        assert!(opt.as_ref().unwrap().data.nodes.get(&b_id).is_none());
        let out = plan_dispatch(&mut opt, "plan_undo", &json!({})).unwrap();
        assert!(out.contains("已回滚"), "应回滚: {out}");
        assert!(opt.as_ref().unwrap().data.nodes.get(&b_id).is_some(), "undo 后应恢复");
    }

    #[test]
    fn plan_focus_sets_in_progress() {
        let mut opt = None;
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "x", "breakdown": [{"title": "任务A"}]})).unwrap();
        let id = opt.as_ref().unwrap().data.nodes.values().find(|n| n.title == "任务A").unwrap().id.clone();
        // 声明焦点：自动 in_progress
        let out = plan_dispatch(&mut opt, "plan_focus", &json!({"task_id": id})).unwrap();
        assert!(out.contains("焦点"), "{out}");
        assert_eq!(opt.as_ref().unwrap().data.nodes.get(&id).unwrap().status, "in_progress");
        // 有执行焦点
        assert!(opt.as_ref().unwrap().has_in_progress());
        // 清除焦点
        plan_dispatch(&mut opt, "plan_focus", &json!({"task_id": "clear"})).unwrap();
        assert!(opt.as_ref().unwrap().active_focus.is_none());
    }

    // ---------- 多一级任务线 ----------

    #[test]
    fn multi_top_level_task_lines() {
        let mut opt = None;
        // plan_init 的 breakdown 顶层任务 = root 直属一级任务（目标层，parent_id == root_id）
        plan_dispatch(&mut opt, "plan_init", &json!({"requirement": "x", "breakdown": [
            {"key": "a", "title": "任务线A"},
            {"key": "b", "title": "任务线B"}
        ]})).unwrap();
        let t = opt.as_ref().unwrap();
        assert_eq!(t.top_level_ids().len(), 2, "应有 2 个一级任务（目标层）");
        for id in &t.top_level_ids() {
            let n = t.data.nodes.get(id).unwrap();
            assert_eq!(n.parent_id, t.data.root_id, "一级任务应为 root 直属（目标层），而非独立顶层");
            assert_ne!(n.id, t.data.root_id);
        }
        // plan_breakdown parent_id=top → 新增独立一级任务线（parent_id == ""，多轮新需求）
        let out = plan_dispatch(&mut opt, "plan_breakdown", &json!({"parent_id": "top", "tasks": [{"title": "任务线C"}]})).unwrap();
        assert!(out.contains("任务线C"), "{out}");
        let t = opt.as_ref().unwrap();
        assert_eq!(t.top_level_ids().len(), 3);
        let c = t.data.nodes.values().find(|n| n.title == "任务线C").unwrap();
        assert_eq!(c.parent_id, "", "parent_id=top 创建独立一级任务线");
        // 省略 parent_id → root 直属一级任务（目标层）
        plan_dispatch(&mut opt, "plan_breakdown", &json!({"tasks": [{"title": "任务线D"}]})).unwrap();
        let t = opt.as_ref().unwrap();
        let d = t.data.nodes.values().find(|n| n.title == "任务线D").unwrap();
        assert_eq!(d.parent_id, t.data.root_id, "省略 parent_id 应挂 root 下");
        assert_eq!(t.top_level_ids().len(), 4, "root 直属子任务也算一级任务");
        // 摘要包含一级任务列表
        let s = t.review_summary_compact();
        assert!(s.contains("一级任务"), "摘要应包含一级任务列表: {s}");
        // plan_move 把任务移到顶层成为独立一级任务（parent_id == ""）
        let c_id = t.data.nodes.values().find(|n| n.title == "任务线C").unwrap().id.clone();
        plan_dispatch(&mut opt, "plan_move", &json!({"task_id": c_id, "new_parent_id": "top"})).unwrap();
        let t = opt.as_ref().unwrap();
        assert_eq!(t.data.nodes.get(&c_id).unwrap().parent_id, "", "移动到顶层后 parent_id 应为空");
    }

    #[test]
    fn plan_init_top_level_are_root_children() {
        // create 时 breakdown 顶层任务为 root 直属的目标层（parent_id == root_id），
        // 层级语义：一级=需求目标、二级=大体步骤、三级=细分步骤（不再平铺成独立顶层）
        let t = TaskMap::create("测试", &json!([
            {"title": "任务线A"},
            {"title": "任务线B"}
        ])).unwrap();
        assert_eq!(t.data.nodes.len(), 3, "root + 2 个一级任务（目标层）");
        for n in t.data.nodes.values() {
            if n.id != t.data.root_id {
                assert_eq!(n.parent_id, t.data.root_id, "顶层任务应挂 root 下: {}", n.title);
            }
        }
        // 一级任务可执行（无依赖）
        let ready = t.next_tasks(10);
        assert_eq!(ready.len(), 2);
    }
}
