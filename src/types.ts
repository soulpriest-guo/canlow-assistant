// 与 Rust 后端共享的数据模型
export type Role = "system" | "user" | "assistant" | "tool";

export interface ChatMessage {
  role: Role;
  content: string;
  reasoningContent?: string;
  toolCalls?: ToolCall[];
  toolCallId?: string;
  name?: string;
  uiRole?: "user" | "assistant" | "system" | "tool" | "tool_call";
  card?: ToolCardInfo;
}

export interface ToolCardInfo {
  name: string;
  args: string;
  status: "running" | "done" | "rejected";
  result: string;
}

export interface ToolCall {
  id: string;
  type: "function";
  function: { name: string; arguments: string };
}

export interface ConversationMeta {
  id: string;
  title: string;
  workDir: string;
  provider: string;
  model: string;
  reasoningEffort: string;
  engineeringMode: boolean;
  createdAt: number;
  updatedAt: number;
}

export interface ProviderConfig {
  baseUrl: string;
  apiKey: string;
  model: string;
  reasoningEffort?: string;
  thinking?: boolean;
}

export interface StreamChunk {
  delta: string;
  reasoning?: string;
  usage?: {
    prompt_tokens?: number;
    completion_tokens?: number;
    prompt_cache_hit_tokens?: number;
    prompt_cache_miss_tokens?: number;
  };
  done: boolean;
  /** 来源会话 ID（用于多会话事件过滤） */
  convId?: string;
}

export interface PermissionRequest {
  requestId: string;
  toolName: string;
  description: string;
  convId?: string;
}

export interface ToolResultEvent {
  toolName: string;
  /** 工具是否执行成功 */
  ok: boolean;
  /** 是否被用户拒绝 */
  rejected?: boolean;
  message: string;
  convId?: string;
}

export interface ProviderDef {
  name: string;
  baseUrl: string;
  models: string[];
  supportsThinking: boolean;
  contextWindow: number;
}

// ---------- 任务图 ----------
export type TaskStatus = "todo" | "in_progress" | "done" | "blocked";

export interface TaskNode {
  id: string;
  title: string;
  detail: string;
  status: TaskStatus;
  progress: number;
  note: string;
  parentId: string;
  deps: string[];
  pos: [number, number];
  created: number;
  /** 开始执行时间戳（plan_update 置 in_progress 时记录） */
  startedAt?: number;
  /** 完成时间戳 */
  finishedAt?: number;
}

export interface TaskMapData {
  version: number;
  requirement: string;
  rootId: string;
  nodes: Record<string, TaskNode>;
  changelog: string[];
  created: number;
  updated: number;
}
