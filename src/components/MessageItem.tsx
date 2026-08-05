import { memo, useState } from "react";
import { marked } from "marked";
import DOMPurify from "dompurify";
import type { ChatMessage } from "../types";

interface Props {
  msg: ChatMessage;
}

function renderMd(content: string): string {
  const raw = marked.parse(content, { breaks: true }) as string;
  // 清洗模型输出中的 HTML，防止 XSS（如 <img onerror> / <script>）
  return DOMPurify.sanitize(raw);
}

function MessageItem({ msg }: Props) {
  // 所有折叠状态放顶层（不能在条件分支里调 hooks）
  const [showReasoning, setShowReasoning] = useState(false);
  const [toolExpanded, setToolExpanded] = useState(false);

  // 工具卡片（前端流式创建，带 card 数据）
  if (msg.uiRole === "tool_call" && msg.card) {
    const { name, args, status, result } = msg.card;
    const statusLabel =
      status === "running" ? "运行中…" : status === "done" ? "完成" : "失败/已拒绝";
    const statusClass = status === "running" ? "run" : status === "done" ? "ok" : "no";
    return (
      <div className="msg tool-card">
        <div
          className="tool-head tool-head-clickable"
          onClick={() => setToolExpanded((v) => !v)}
          title={toolExpanded ? "点击折叠" : "点击展开"}
        >
          <span className="tool-label">
            {toolExpanded ? "▾" : "▶"} {name}
          </span>
          <span className={"tool-status " + statusClass}>{statusLabel}</span>
        </div>
        {toolExpanded && (
          <>
            <pre className="tool-args">{args}</pre>
            {result && (
              <pre className="tool-result">
                {result.length > 12000
                  ? result.slice(0, 12000) + "\n…（已截断，仅显示前 12000 字符）"
                  : result}
              </pre>
            )}
          </>
        )}
      </div>
    );
  }

  // 历史加载的工具结果消息（role=tool，来自数据库）：可折叠，默认折叠
  if (msg.role === "tool") {
    const label = msg.name || "工具结果";
    return (
      <div className="msg tool-card">
        <div
          className="tool-head tool-head-clickable"
          onClick={() => setToolExpanded((v) => !v)}
          title={toolExpanded ? "点击折叠" : "点击展开"}
        >
          <span className="tool-label">
            {toolExpanded ? "▾" : "▶"} {label}
          </span>
          <span className="tool-status ok">完成</span>
        </div>
        {toolExpanded && (
          <pre className="tool-result">
            {msg.content.length > 12000
              ? msg.content.slice(0, 12000) + "\n…（已截断，仅显示前 12000 字符）"
              : msg.content}
          </pre>
        )}
      </div>
    );
  }

  if (msg.role === "system") {
    return (
      <div className="msg sys-msg">
        <span>{msg.content}</span>
      </div>
    );
  }

  const isUser = msg.role === "user";
  return (
    <div className={"msg " + (isUser ? "user" : "assistant")}>
      {msg.reasoningContent && (
        <div className="reasoning">
          <button className="reasoning-toggle" onClick={() => setShowReasoning((v) => !v)}>
            {showReasoning ? "▾" : "▶"} 思考过程
          </button>
          {showReasoning && <div className="reasoning-body">{msg.reasoningContent}</div>}
        </div>
      )}
      <div
        className="msg-bubble"
        dangerouslySetInnerHTML={{ __html: renderMd(msg.content || "") }}
      />
    </div>
  );
}

// 自定义比较：useChat 的 setMessages 用浅拷贝数组更新，未变的消息引用不变，
// 因此流式更新时只有最后一条消息会重渲染（memo 不再被引用变化击穿）
export default memo(MessageItem, (prev, next) => prev.msg === next.msg);
