# Canlow 旧版 → Tauri 新版迁移方案

> 依据：`canlow-新界面/`（v1.3.1，250KB 主程序 + 工具/任务图/沙箱/MCP 拆分模块）
> 目标：`canlow-next/`（Tauri 2 + React + TypeScript + Rust）

## 一、旧版功能全景（盘点结果）

### 1. API 层
- 内置 6 家提供商：DeepSeek、OpenAI、智谱AI、MiniMax、小米 MiMo、Kimi
- 每家配置：base_url、models、supports_thinking、token_plan_base_url、context_window
- 自定义提供商（custom_providers JSON）
- 多 Key 管理（provider_keys）+ PBKDF2 加密存储
- 连接测试、thinking 显式开关（仅 DeepSeek）、reasoning_effort

### 2. 会话与上下文
- 多会话：新建/重命名/删除/切换，JSON 持久化
- 会话级设置：work_dir、model、reasoning_effort、engineering_mode
- 上下文压缩：85% 阈值触发、KEEP_RECENT=12、本地抽取摘要、system 原位保留
- 上下文缓存 cache/：压缩原文可 retrieve_cache_entry 找回
- 大文件工具结果存 files/，指纹去重 `<<<CANLOW_FILE:sha>>>`，可 retrieve_stored_file
- search_conversation_history 搜索历史
- 缓存命中统计（usage 提取 cache_hit/miss，界面显示）

### 3. 工具系统（40+）
- 文件：list_dir / read_file / read_file_segment / write_file / replace_in_file /
  rename_file / copy_file / move_file / create_directory / get_file_info /
  open_file / diff_file / undo_file
- 搜索：search_files / grep_search / glob_search
- Git：git_status / git_diff / git_log / git_commit / git_add / git_branch
- 命令：run_command（token 级高危拦截）/ check_command（异步任务表）/ terminate_command
- Web：fetch_webpage（编码探测+正文提取）/ search_web
- 对话：search_conversation_history / retrieve_cache_entry / retrieve_stored_file
- 技能：list_skills / get_skill_guidance / create_skill / edit_skill / delete_skill
- 任务：todo_create / todo_list / todo_update
- 子代理：spawn_sub_agent / list_sub_agents / get_sub_agent_result
- 工程：project_info（项目类型/语言/构建/测试/Lint 探测）
- 思维导图：plan_init / plan_breakdown / plan_update / plan_link / plan_review /
  plan_requirement / plan_move / plan_delete / plan_export
- 沙箱/MCP：get_sandbox_info / mcp_status / mcp_refresh / mcp_* 动态工具
- 参数校验（validate_tool_args）、工具定义会话级缓存、授权模式（ask/allow_all）

### 4. Agent 循环
- stream + tool_calls 多轮循环（MAX_RETRIES=3）
- reasoning_content 流式、流锁防交错、每轮结束 flush
- 工具授权弹窗、停止按钮
- 工程模式：无任务图时只开放 plan_init

### 5. 任务图（核心增值）
- TaskMap：节点树/依赖边/状态/进度/快照恢复/auto_layout/flow_layout/topo_order
- plan_* 工具派发、changelog、markdown 导出
- plan_graph_ui：画布拖拽、右键菜单、更新任务打断继续

### 6. 技能系统
- skills/ 目录扫描、YAML frontmatter 解析、旧 JSON 兼容
- 创建/编辑/删除、关键词匹配（match_skills）、动态注入（!command 带安全拦截）
- 内置技能种子

### 7. 沙箱与 MCP
- sandbox.py：后端探测（seatbelt/nsjail）、rlimits、高危命令 token 级拦截
- mcp_client.py：stdio JSON-RPC、多服务器、动态工具注册
- mcp_server.py：MCP 服务端（工具导出）

### 8. Web UI 与 CLI
- web_server.py：Bottle + SSE + WebAgent（headless 复用主 agent）
- CLIApp：命令行对话模式
- SessionLogger：jsonl 结构化日志

## 二、新架构分层（目标态）

```
canlow-next/
├── src-tauri/src/
│   ├── lib.rs            # 入口/命令注册（现有）
│   ├── commands/         # Tauri 命令层（薄壳）
│   │   ├── chat.rs       # api_stream / stop / usage
│   │   ├── session.rs    # 会话 CRUD / 持久化
│   │   ├── tools.rs      # 工具命令入口
│   │   ├── taskmap.rs    # 任务图读写/导出
│   │   └── settings.rs   # 提供商/密钥/配置
│   ├── core/             # 核心逻辑（无 Tauri 依赖，可单测）
│   │   ├── agent.rs      # agent 循环：tool_calls 多轮
│   │   ├── context.rs    # 消息组装/压缩/缓存（从 chat.ts 下沉）
│   │   ├── providers.rs  # 提供商注册表 + 自定义
│   │   ├── taskmap.rs    # TaskMap 数据模型
│   │   ├── skills.rs     # 技能管理
│   │   ├── sandbox.rs    # 命令沙箱/拦截
│   │   └── mcp.rs        # MCP stdio 客户端
│   └── tools/            # 工具执行
│       ├── fs.rs / search.rs / git.rs / cmd.rs / web.rs
├── src/                  # 前端（React）
│   ├── components/       # 聊天/工具卡片/任务图/技能/设置/授权弹窗
│   ├── lib/chat.ts       # 消息组装（保留，agent 下沉后改为 UI 状态）
│   └── lib/api.ts        # invoke 封装
└── data/                 # SQLite：会话/缓存/文件存储（替代 JSON 目录）
```

关键决策：
- **agent 循环放 Rust**：工具执行、授权、停止、上下文压缩都在后端，前端只做展示；
  流式事件通道已打通（stream-chunk / stream-error）
- **存储用 SQLite**（rusqlite）：替代 conversations/*.json + cache/ + files/，仿 Codex 的 state/logs
- **任务图数据模型放 TS**（React Flow 直接消费），Rust 负责持久化与 markdown 导出
- **工具定义由 Rust 生成 JSON**，会话级缓存保证字节稳定

## 三、分阶段实施计划

### Phase 1 ✅ 已完成（当前骨架）
- 聊天 + 虚拟滚动 + 思考显示 + 设置面板 + DeepSeek 流式 + list_dir/read_file/run_command

### Phase 2 核心可用（下一步）
- [ ] Rust：会话持久化（SQLite，CRUD + 消息存储）
- [ ] Rust：完整工具集（文件全套 + grep/glob + git + 异步命令表）
- [ ] Rust：agent 循环（tool_calls 多轮 + 重试 + 停止）
- [ ] 前端：工具卡片渲染 + 授权弹窗（ask/allow_all）+ 停止按钮
- [ ] 前端：会话管理 UI 接真实存储（重命名/删除/切换）

### Phase 3 增值能力
- [ ] 任务图完整交互：React Flow 节点增删改/连线/快照恢复/右键菜单
- [ ] 工程模式：计划优先、plan_* 工具、更新任务打断继续
- [ ] 上下文缓存：cache 存储 + retrieve_cache_entry + search_conversation_history
- [ ] 技能系统：skills 目录 + 管理界面 + 注入
- [ ] 缓存命中率遥测显示（usage 已透传，接 UI footer）

### Phase 4 生态能力
- [ ] 多提供商：6 家内置 + 自定义 + Key 加密存储（Rust 侧）
- [ ] 沙箱：命令拦截 token 级 + seatbelt/nsjail 后端 + rlimits
- [ ] MCP：客户端（stdio）+ 动态工具 + 状态界面
- [ ] Web 工具：fetch_webpage / search_web

### Phase 5 打磨与分发
- [ ] CLI 模式、日志系统、主题
- [ ] tauri build 打包（dmg，约 10-20MB）
- [ ] 回归测试（Rust tests + 前端 vitest）

## 四、迁移注意点（从旧版踩坑中继承）

1. 工具定义必须缓存且字节稳定（缓存命中的前提）
2. system 消息固定、历史纯追加、满窗口才压缩
3. 压缩保留用户消息原文 + 摘要放最后（Codex 模式）
4. 命令执行必须过 token 级高危拦截，不允许裸 shell=True
5. 流式更新用不可变数据（React 渲染前提），不再改共享引用
6. MCP 刷新后必须失效工具缓存
