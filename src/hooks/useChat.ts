import { useCallback, useEffect, useRef, useState } from "react";
import type {
  ChatMessage,
  ConversationMeta,
  PermissionRequest,
  ProviderConfig,
  ProviderDef,
  StreamChunk,
  ToolCall,
} from "../types";
import {
  agentTurn,
  providersList,
  listenAgentEvents,
  respondPermission,
  respondPlanConfirm,
  resumeAgent,
  sessionCreate,
  sessionDelete,
  sessionList,
  sessionMessages,
  sessionRename,
  setAuthMode,
  stopAgent,
} from "../lib/api";
import { loadProvider, saveProvider } from "../lib/settings";

export function useChat() {
  const [conversations, setConversations] = useState<ConversationMeta[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [thinking, setThinking] = useState(false);
  const [streaming, setStreaming] = useState(false);
  const [input, setInput] = useState("");
  const [provider, setProviderState] = useState<ProviderConfig>(loadProvider);
  const [providers, setProviders] = useState<ProviderDef[]>([]);
  const [permission, setPermission] = useState<PermissionRequest | null>(null);
  /** 计划确认弹窗数据（AI 修改任务图结构后请求用户确认；同意才继续执行） */
  const [planConfirm, setPlanConfirm] = useState<{
    convId: string;
    hasMap: boolean;
    summary: string;
  } | null>(null);
  const [authMode, setAuthModeState] = useState<"ask" | "smart" | "allow_all" | "none">("smart");
  const [debugStat, setDebugStat] = useState({ content: 0, reasoning: 0, done: false });
  const [mapDirty, setMapDirty] = useState(0);
  const [cacheStats, setCacheStats] = useState<{ hit: number; miss: number; rate: number }>({
    hit: 0,
    miss: 0,
    rate: 0,
  });

  const activeIdRef = useRef<string | null>(null);
  activeIdRef.current = activeId;
  const providerRef = useRef(provider);
  providerRef.current = provider;
  const streamingRef = useRef(streaming);
  streamingRef.current = streaming;
  // 授权弹窗超时定时器（与后端 600s 授权超时对齐）
  const permTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // 启动加载会话与提供商
  useEffect(() => {
    refreshConversations();
    refreshProviders();
  }, []); // eslint-disable-line

  const refreshProviders = useCallback(async () => {
    try {
      setProviders(await providersList());
    } catch (e) {
      console.error(e);
    }
  }, []);

  const refreshConversations = useCallback(async () => {
    try {
      setConversations(await sessionList());
    } catch (e) {
      console.error(e);
    }
  }, []);

  // 全局事件监听（挂载一次）
  useEffect(() => {
    let mounted = true;
    let unlisten: (() => void) | null = null;
    (async () => {
      unlisten = await listenAgentEvents({
      onChunk: (chunk: StreamChunk) => {
        if (!mounted) return;
        // 只处理当前会话的事件，避免切换会话后串扰
        if (chunk.convId && chunk.convId !== activeIdRef.current) return;
        if (chunk.delta || chunk.reasoning) {
          setMessages((msgs) => {
            const copy = [...msgs];
            const last = copy[copy.length - 1];
            // 最后一条是可追加的 assistant（当前轮次）：追加增量
            if (last && last.role === "assistant" && last.uiRole !== "tool_call") {
              const updated = { ...last };
              if (chunk.delta) updated.content += chunk.delta;
              if (chunk.reasoning) {
                updated.reasoningContent = (updated.reasoningContent || "") + chunk.reasoning;
              }
              copy[copy.length - 1] = updated;
            } else if (chunk.delta || chunk.reasoning) {
              // 最后一条是工具卡片/非 assistant：工具调用后的新一轮输出从这里开始，
              // 必须新建 assistant 消息承载，否则回答会被丢弃（切会话重载才可见）
              copy.push({
                role: "assistant",
                content: chunk.delta || "",
                reasoningContent: chunk.reasoning || undefined,
                uiRole: "assistant",
              });
            }
            return copy;
          });
          setDebugStat((d) => ({
            content: d.content + (chunk.delta ? 1 : 0),
            reasoning: d.reasoning + (chunk.reasoning ? 1 : 0),
            done: d.done,
          }));
        }
        if (chunk.done) {
          setStreaming(false);
          setThinking(false);
          setDebugStat((d) => ({ ...d, done: true }));
        }
      },
      onPermission: (req) => {
        if (req.convId && req.convId !== activeIdRef.current) return;
        setPermission(req);
        // 授权超时自动关闭弹窗（后端 600s 超时后该授权已失效）
        if (permTimerRef.current) clearTimeout(permTimerRef.current);
        permTimerRef.current = setTimeout(() => {
          setPermission((cur) => (cur?.requestId === req.requestId ? null : cur));
        }, 610_000);
      },
      onPlanConfirm: (payload) => {
        if (!mounted) return;
        // 只处理当前会话的确认请求（未指定 convId 时按当前会话处理）
        if (payload.convId && payload.convId !== activeIdRef.current) return;
        setPlanConfirm({
          convId: payload.convId || activeIdRef.current || "",
          hasMap: payload.hasMap,
          summary: payload.summary,
        });
      },
      onToolResult: (e) => {
        if (e.convId && e.convId !== activeIdRef.current) return;
        // AI 每次执行任务图工具后立即通知任务图刷新（实时进度）
        if (e.toolName.startsWith("plan_")) {
          setMapDirty((d) => d + 1);
        }
        setMessages((msgs) => {
          const copy = [...msgs];
          // 更新最后一张同名的工具卡片
          for (let i = copy.length - 1; i >= 0; i--) {
            if (copy[i].uiRole === "tool_call" && copy[i].card?.name === e.toolName) {
              const updated = { ...copy[i] };
              updated.card = {
                name: e.toolName,
                args: copy[i].card?.args || "",
                status: e.rejected ? "rejected" : e.ok ? "done" : "rejected",
                result: e.message,
              };
              copy[i] = updated;
              break;
            }
          }
          return copy;
        });
      },
      onRoundEnd: (round, toolCalls, convId) => {
        if (convId && convId !== activeIdRef.current) return;
        // 如果 AI 动过任务图，通知任务图界面刷新
        if ((toolCalls as ToolCall[]).some((tc) => tc.function.name.startsWith("plan_"))) {
          setMapDirty((d) => d + 1);
        }
        const cards: ChatMessage[] = (toolCalls as ToolCall[]).map((tc) => ({
          role: "tool",
          content: "",
          uiRole: "tool_call",
          card: {
            name: tc.function.name,
            args: tc.function.arguments,
            status: "running" as const,
            result: "",
          },
        }));
        setMessages((msgs) => [...msgs, ...cards]);
      },
      onCacheStats: (stats) => {
        if (stats.convId && stats.convId !== activeIdRef.current) return;
        setCacheStats(stats);
      },
      onNotice: (msg, convId) => {
        if (convId && convId !== activeIdRef.current) return;
        setMessages((msgs) => [
          ...msgs,
          { role: "system", content: `⏳ ${msg}` },
        ]);
      },
      onError: (msg, convId) => {
        if (convId && convId !== activeIdRef.current) return;
        setStreaming(false);
        setThinking(false);
        setMessages((msgs) => [
          ...msgs,
          { role: "system", content: `❌ ${msg}` },
        ]);
      },
      });
    })();
    return () => {
      mounted = false;
      unlisten?.();
      if (permTimerRef.current) clearTimeout(permTimerRef.current);
    };
  }, []);

  // ---------- 会话操作 ----------
  const newConversation = useCallback(async () => {
    const conv = await sessionCreate(
      "新对话",
      "",
      "DeepSeek",
      providerRef.current.model || "deepseek-v4-flash",
      "high"
    );
    setConversations((prev) => [conv, ...prev]);
    setActiveId(conv.id);
    setMessages([]);
    setDebugStat({ content: 0, reasoning: 0, done: false });
  }, []);

  // 带项目信息的创建（名称 + 工作目录）
  const createConversation = useCallback(async (title: string, workDir: string) => {
    const conv = await sessionCreate(
      title || "新对话",
      workDir,
      "DeepSeek",
      providerRef.current.model || "deepseek-v4-flash",
      "high"
    );
    setConversations((prev) => [conv, ...prev]);
    setActiveId(conv.id);
    setMessages([]);
    setDebugStat({ content: 0, reasoning: 0, done: false });
    return conv;
  }, []);

  const selectConversation = useCallback(async (id: string) => {
    // 若当前会话正在流式输出，先停止（只停当前会话，其它会话不受影响）
    if (streamingRef.current) await stopAgent(activeIdRef.current);
    // 立即同步 ref，避免切换瞬间旧会话的迟到事件串入新会话
    activeIdRef.current = id;
    setActiveId(id);
    const msgs = await sessionMessages(id);
    setMessages(msgs);
    setDebugStat({ content: 0, reasoning: 0, done: false });
  }, []);

  const deleteConversation = useCallback(
    async (id: string) => {
      await sessionDelete(id);
      setConversations((prev) => prev.filter((c) => c.id !== id));
      if (activeId === id) {
        setActiveId(null);
        setMessages([]);
      }
    },
    [activeId]
  );

  const renameConversation = useCallback(async (id: string, title: string) => {
    await sessionRename(id, title);
    setConversations((prev) =>
      prev.map((c) => (c.id === id ? { ...c, title } : c))
    );
  }, []);

  // ---------- 发送 ----------
  const send = useCallback(async () => {
    const text = input.trim();
    if (!text || streaming) return;
    // 发送瞬间立即清空输入框（不能等 agent 循环结束，可能耗时几分钟）
    setInput("");
    await sendText(text);
  }, [input, streaming]); // eslint-disable-line

  /**
   * 以用户身份发送一条消息（任务图等界面调用：把用户意图指令发给 AI）。
   * 与输入框发送等价，但不依赖 input state。
   * planOnly = true 时为任务设计模式：AI 仅调整任务图，不执行（后端限制工具列表）
   */
  const sendText = useCallback(async (raw: string, planOnly = false) => {
    const text = raw.trim();
    if (!text) return;
    if (streamingRef.current) return;
    const convId = activeIdRef.current;
    if (!convId) return;

    setMessages((msgs) => [
      ...msgs,
      { role: "user", content: text },
      { role: "assistant", content: "", uiRole: "assistant" },
    ]);
    // ★ 立即同步 ref：任务设计流程依赖「打断后马上能再次发送」的判断
    streamingRef.current = true;
    setStreaming(true);
    setThinking(true);
    setDebugStat({ content: 0, reasoning: 0, done: false });

    try {
      await agentTurn(convId, text, planOnly);
    } catch (err) {
      streamingRef.current = false;
      setStreaming(false);
      setThinking(false);
      setMessages((msgs) => [
        ...msgs,
        { role: "system", content: `❌ 请求失败：${String(err)}` },
      ]);
    }
  }, []);

  /** 继续被中断的 agent 进程：不追加用户消息，恢复原循环（任务图「继续」按钮） */
  const resume = useCallback(async () => {
    const convId = activeIdRef.current;
    if (!convId || streamingRef.current) return;
    streamingRef.current = true;
    setStreaming(true);
    setThinking(true);
    setDebugStat({ content: 0, reasoning: 0, done: false });
    try {
      await resumeAgent(convId);
    } catch (err) {
      streamingRef.current = false;
      setStreaming(false);
      setThinking(false);
      setMessages((msgs) => [
        ...msgs,
        { role: "system", content: `❌ 继续失败：${String(err)}` },
      ]);
    }
  }, []);

  // ---------- 授权 ----------
  const answerPermission = useCallback(async (allow: boolean) => {
    if (permission) {
      await respondPermission(permission.requestId, allow);
      setPermission(null);
    }
  }, [permission]);

  /** 响应计划确认：同意（继续执行）或拒绝（回滚任务图修改） */
  const answerPlanConfirm = useCallback(
    async (allow: boolean) => {
      if (!planConfirm) return;
      const convId = planConfirm.convId;
      setPlanConfirm(null);
      try {
        await respondPlanConfirm(convId, allow);
      } catch (e) {
        console.error(e);
      }
    },
    [planConfirm]
  );

  const stop = useCallback(async () => {
    await stopAgent(activeIdRef.current);
    // ★ 立即同步 ref：任务设计流程「打断后马上发送规划内容」依赖此判断
    streamingRef.current = false;
    setStreaming(false);
    setThinking(false);
  }, []);

  const setProvider = useCallback((p: ProviderConfig) => {
    setProviderState(p);
    saveProvider(p);
  }, []);

  const changeAuthMode = useCallback(async (mode: "ask" | "smart" | "allow_all" | "none") => {
    setAuthModeState(mode);
    await setAuthMode(mode);
  }, []);

  return {
    conversations,
    refreshConversations,
    providers,
    refreshProviders,
    activeId,
    setActiveId: selectConversation,
    messages,
    thinking,
    streaming,
    input,
    setInput,
    send,
    sendText,
    stop,
    resume,
    newConversation,
    createConversation,
    deleteConversation,
    renameConversation,
    provider,
    setProvider,
    permission,
    answerPermission,
    planConfirm,
    answerPlanConfirm,
    authMode,
    setAuthMode: changeAuthMode,
    debugStat,
    mapDirty,
    cacheStats,
  };
}
