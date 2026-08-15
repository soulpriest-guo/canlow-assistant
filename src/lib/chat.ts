// 上下文组装：核心纪律 = 纯追加 + 固定系统提示 + 延迟压缩
// 参考 Codex 的缓存友好策略：
//  1) 系统提示固定不变，永远在开头
//  2) 历史消息 append-only，不重写旧内容
//  3) 只有超出窗口上限才压缩；压缩时保留全部用户消息原文 + 摘要放最后
import type { ChatMessage } from "../types";

// 与 Rust 侧 build_core_system_prompt() 保持一致：极简固定 system（DSH minimal 风格）
export const SYSTEM_PROMPT = `你是 Canlow，一个智能编程助手。你与用户共享一个工作区。使用工具完成任务，不要编造执行结果。`;

// 压缩后最多保留的用户消息预算（字符估算）
const COMPACT_USER_CHARS = 50000;
const MAX_TOTAL_CHARS = 180000; // 按 256k 上下文留余量

/**
 * 组装发送给 API 的消息：
 * - 系统提示固定
 * - 历史原样追加（除非需要压缩）
 */
export function buildApiMessages(
  history: ChatMessage[],
  userInput: string
): ChatMessage[] {
  const base: ChatMessage[] = [{ role: "system", content: SYSTEM_PROMPT }];
  const tail: ChatMessage[] = [{ role: "user", content: userInput }];

  if (totalChars(history) <= MAX_TOTAL_CHARS * 0.9) {
    return [...base, ...history, ...tail];
  }
  const compacted = compactHistory(history);
  return [...base, ...compacted, ...tail];
}

function totalChars(msgs: ChatMessage[]): number {
  let n = 0;
  for (const m of msgs) n += (m.content || "").length + (m.reasoningContent || "").length;
  return n;
}

/**
 * 压缩历史（缓存友好版）：
 * - 保留全部用户消息原文（预算内，从新到旧收集，再恢复原顺序）
 * - 助手的过程性内容（工具调用、长回复）替换为一条摘要，放在最后
 * - 不改变消息之间的相对顺序
 */
export function compactHistory(history: ChatMessage[]): ChatMessage[] {
  // 1) 收集用户消息（预算内，从新到旧）
  const userMsgs = history.filter((m) => m.role === "user" && m.content);
  const selected: ChatMessage[] = [];
  let budget = COMPACT_USER_CHARS;
  for (let i = userMsgs.length - 1; i >= 0; i--) {
    const len = userMsgs[i].content.length;
    if (len <= budget) {
      selected.unshift(userMsgs[i]);
      budget -= len;
    } else {
      selected.unshift({ ...userMsgs[i], content: userMsgs[i].content.slice(0, budget) });
      budget = 0;
      break;
    }
    if (budget === 0) break;
  }

  // 2) 取最后一条助手消息作为摘要尾巴
  const lastAssistant = [...history].reverse().find((m) => m.role === "assistant" && m.content);
  const summaryText = lastAssistant
    ? `[对话摘要] 以下是之前对话的交接摘要：\n${lastAssistant.content.slice(0, 4000)}`
    : "[对话摘要] 早期对话已被压缩。";

  return [...selected, { role: "assistant", content: summaryText }];
}
