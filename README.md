# Canlow Next

Canlow 的 Tauri 2 重构版：**Rust 核心 + React/TypeScript 前端**，目标是在保持思维图谱交互核心的同时，解决旧版 tkinter 长对话卡顿、分发体积大、缓存命中率低的问题。

## 技术栈

| 层 | 技术 |
|---|---|
| 桌面壳 | Tauri 2（系统 WebView，打包约 10-20MB） |
| 前端 | React 19 + TypeScript + Vite |
| 消息列表 | @tanstack/react-virtual（虚拟滚动，长对话不卡） |
| 思维图谱 | React Flow（节点拖拽/连线/缩放） |
| 后端 | Rust + reqwest + tokio（流式 API、文件、命令） |

## 运行

```bash
# 前置：Node 18+、Rust 1.77+
npm install
npm run tauri dev
```

打包：

```bash
npm run tauri build
```

## 目录结构

```
canlow-next/
├── src/                  # 前端
│   ├── components/       # 聊天、消息、侧边栏、任务图
│   ├── hooks/useChat.ts  # 会话状态与流式发送
│   └── lib/
│       ├── api.ts        # Rust 命令封装
│       └── chat.ts       # ★ 缓存友好消息组装
└── src-tauri/            # Rust 后端
    ├── src/lib.rs        # 流式 API、文件/命令工具
    ├── tauri.conf.json
    └── capabilities/
```

## 核心设计：缓存友好的上下文机制

`src/lib/chat.ts` 从第一天就按 Codex 的纪律实现：

1. **系统提示固定**：永远在开头，内容不变
2. **历史纯追加**：发送请求前不重写任何旧消息
3. **延迟压缩**：只有超出窗口上限才压缩
4. **压缩保留用户消息**：全部用户消息原文（预算内）保留，AI 过程替换为摘要放最后

Rust 侧（`api_stream`）不做任何消息改写，只负责流式转发和透传 usage（含 `prompt_cache_hit_tokens`），为后续缓存命中率统计留好接口。

## 路线图

- [x] 项目骨架：聊天 + 虚拟滚动 + 基础任务图
- [ ] 设置面板（API Key、模型、提供商）
- [ ] 会话持久化（Rust 侧 JSON/SQLite）
- [ ] 工具调用循环（list_dir/read_file 已就绪，接入 agent loop）
- [ ] 思维图谱完整交互（节点增删改、连线、快照回退）
- [ ] 沙箱执行 + 高危命令拦截
- [ ] MCP 客户端/服务端
- [ ] 缓存命中率遥测展示

## 致谢

上下文缓存与压缩机制的设计参考了 OpenAI Codex（Apache-2.0）与 Pi（MIT）的公开实现。
