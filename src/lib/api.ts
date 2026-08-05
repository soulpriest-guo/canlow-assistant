// Tauri 命令封装：会话/工具/Agent 全部走 Rust 后端
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  ChatMessage,
  ConversationMeta,
  ProviderConfig,
  ProviderDef,
  StreamChunk,
  PermissionRequest,
  ToolResultEvent,
} from "../types";

// ---------- 会话 ----------
export const sessionList = () => invoke<ConversationMeta[]>("session_list");
export const sessionCreate = (
  title: string,
  workDir: string,
  provider: string,
  model: string,
  reasoningEffort = "high"
) =>
  invoke<ConversationMeta>("session_create", { title, workDir, provider, model, reasoningEffort });
export const sessionDelete = (id: string) => invoke<void>("session_delete", { id });
export const sessionRename = (id: string, title: string) =>
  invoke<void>("session_rename", { id, title });
export const sessionUpdate = (
  id: string,
  patch: Partial<{
    workDir: string;
    provider: string;
    model: string;
    reasoningEffort: string;
    engineeringMode: boolean;
  }>
) => invoke<void>("session_update", { id, ...patch });
export const sessionMessages = (id: string) => invoke<ChatMessage[]>("session_messages", { id });

// ---------- 上下文档位 ----------
export const contextProfileGet = () => invoke<string>("context_profile_get");
export const contextProfileSet = (profile: "1m" | "256k") =>
  invoke<void>("context_profile_set", { profile });

// ---------- 工具 ----------
// 工具执行统一走 agent 循环（agent_turn），前端不再直接调用。

// ---------- 任务图 ----------
export const taskmapGet = (id: string) =>
  invoke<unknown | null>("taskmap_get", { id });
export const taskmapSave = (id: string, data: unknown) =>
  invoke<void>("taskmap_save", { id, data });
export const taskmapDelete = (id: string) => invoke<void>("taskmap_delete", { id });
export const taskmapSyncMemory = (id: string, data: unknown) =>
  invoke<void>("taskmap_sync_memory", { id, data });

// ---------- 目录选择 ----------
export async function pickDirectory(): Promise<string | null> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const dir = await open({ directory: true, multiple: false });
  return typeof dir === "string" ? dir : null;
}

// ---------- 提供商 ----------
export const providersList = () => invoke<ProviderDef[]>("providers_list");
export const providerSaveKey = (name: string, key: string) =>
  invoke<void>("provider_save_key", { name, key });
export const providerSetModels = (name: string, models: string[]) =>
  invoke<void>("provider_set_models", { name, models });
export const providerKeyStatus = (name: string) =>
  invoke<string | null>("provider_key_status", { name });
export const providerTest = (name: string) => invoke<string>("provider_test", { name });
export const customProviderAdd = (name: string, baseUrl: string, models: string[]) =>
  invoke<void>("custom_provider_add", { name, baseUrl, models });
export const customProviderRemove = (name: string) =>
  invoke<void>("custom_provider_remove", { name });

// ---------- Agent ----------
export const agentTurn = (convId: string, text: string, planOnly = false) =>
  invoke<void>("agent_turn", { convId, text, planOnly });
/** 继续被中断的 agent 进程（不追加用户消息，后端注入「继续执行」提示后恢复循环） */
export const resumeAgent = (convId: string) =>
  invoke<void>("agent_resume", { convId });
export const respondPermission = (requestId: string, allow: boolean) =>
  invoke<void>("respond_permission", { requestId, allow });
/** 响应计划确认：允许/拒绝 AI 对任务图结构的修改（同意才继续执行，拒绝回滚） */
export const respondPlanConfirm = (convId: string, allow: boolean) =>
  invoke<void>("respond_plan_confirm", { convId, allow });
export const stopAgent = (convId: string | null) =>
  invoke<void>("stop_agent", { convId });
export const setAuthMode = (mode: "ask" | "smart" | "allow_all" | "none") =>
  invoke<void>("set_auth_mode", { mode });

// ---------- 事件 ----------
export interface CacheStats {
  hit: number;
  miss: number;
  rate: number;
  convId?: string;
}

export interface AgentEvents {
  onChunk: (chunk: StreamChunk) => void;
  onPermission: (req: PermissionRequest) => void;
  onToolResult: (e: ToolResultEvent) => void;
  onRoundEnd: (round: number, toolCalls: unknown[], convId?: string) => void;
  onError: (msg: string, convId?: string) => void;
  onNotice?: (msg: string, convId?: string) => void;
  onCacheStats?: (stats: CacheStats) => void;
  /** AI 修改任务图结构后请求用户确认（同意才继续执行，拒绝回滚） */
  onPlanConfirm?: (payload: { convId?: string; hasMap: boolean; summary: string }) => void;
}

/** 注册一次 agent 会话的事件监听；返回取消函数（页面卸载/会话切换时调用） */
export async function listenAgentEvents(events: AgentEvents): Promise<() => void> {
  const un1 = await listen<StreamChunk>("stream-chunk", (e) => events.onChunk(e.payload));
  const un2 = await listen<PermissionRequest>("permission-request", (e) =>
    events.onPermission(e.payload)
  );
  const un3 = await listen<ToolResultEvent>("tool-result", (e) => events.onToolResult(e.payload));
  const un4 = await listen<{ round: number; toolCalls: unknown[]; convId?: string }>(
    "agent-round-end",
    (e) => events.onRoundEnd(e.payload.round, e.payload.toolCalls, e.payload.convId)
  );
  const un5 = await listen<{ error: string; convId?: string }>("stream-error", (e) =>
    events.onError(e.payload.error, e.payload.convId)
  );
  let un6: (() => void) | null = null;
  if (events.onCacheStats) {
    un6 = await listen<CacheStats>("cache-stats", (e) => events.onCacheStats!(e.payload));
  }
  let un7: (() => void) | null = null;
  if (events.onNotice) {
    un7 = await listen<{ message: string; convId?: string }>("stream-notice", (e) =>
      events.onNotice!(e.payload.message, e.payload.convId)
    );
  }
  let un8: (() => void) | null = null;
  if (events.onPlanConfirm) {
    un8 = await listen<{ convId?: string; hasMap: boolean; summary: string }>(
      "plan-confirm",
      (e) => events.onPlanConfirm!(e.payload)
    );
  }
  return () => {
    un1();
    un2();
    un3();
    un4();
    un5();
    un6?.();
    un7?.();
    un8?.();
  };
}
