import { useEffect, useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Send, Square } from "lucide-react";
import type { ChatMessage } from "../types";
import MessageItem from "./MessageItem";

interface Props {
  messages: ChatMessage[];
  thinking: boolean;
  streaming: boolean;
  input: string;
  onInput: (v: string) => void;
  onSend: () => void;
  onStop: () => void;
  scrollResetKey?: string | null;
}

export default function ChatView({
  messages,
  thinking,
  streaming,
  input,
  onInput,
  onSend,
  onStop,
  scrollResetKey,
}: Props) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const composingRef = useRef(false);
  const enterDuringCompositionRef = useRef(false);
  const lastCompositionEndRef = useRef(0);

  // 虚拟滚动：只渲染视口附近的消息，长对话不卡
  const virtualizer = useVirtualizer({
    count: messages.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 120,
    overscan: 6,
    measureElement: (el) => el.getBoundingClientRect().height,
  });

  // 新消息/流式时自动滚到底部
  const autoScroll = useRef(true);

  // 切换会话时：重置自动滚动，直接定位到底部（最新消息）
  useEffect(() => {
    autoScroll.current = true;
    if (messages.length > 0) {
      requestAnimationFrame(() => {
        virtualizer.scrollToIndex(messages.length - 1, { align: 'end' });
      });
    }
  }, [scrollResetKey]); // eslint-disable-line
  // 新消息/流式时自动滚到底部
  useEffect(() => {
    if (autoScroll.current && messages.length > 0) {
      requestAnimationFrame(() => {
        virtualizer.scrollToIndex(messages.length - 1, { align: 'end' });
      });
    }
  }, [messages.length, messages[messages.length - 1]?.content]);

  const handleKey = (e: React.KeyboardEvent) => {
    if (e.key !== "Enter") return;
    // 输入法组词中按 Enter 是选候选词：打标记，本次及后续确认候选的 Enter 都不发送
    if (e.nativeEvent.isComposing || composingRef.current) {
      enterDuringCompositionRef.current = true;
      return;
    }
    // 组词确认后的残留 Enter（某些输入法会重复触发 keydown）
    if (enterDuringCompositionRef.current) return;
    // WebKit 兼容：compositionend 可能先于 Enter keydown 触发，用时间戳拦截
    // （300ms 内的 Enter 视为输入法确认候选，不发送）
    if (Date.now() - lastCompositionEndRef.current < 300) {
      enterDuringCompositionRef.current = true;
      return;
    }
    if (!e.shiftKey) {
      e.preventDefault();
      onSend();
    }
  };
  const handleKeyUp = (e: React.KeyboardEvent) => {
    // 按键完整抬起后恢复发送能力
    if (e.key === "Enter" && enterDuringCompositionRef.current) {
      enterDuringCompositionRef.current = false;
    }
  };

  return (
    <div className="chat-view">
      <div
        className="chat-scroll"
        ref={scrollRef}
        onScroll={() => {
          const el = scrollRef.current!;
          autoScroll.current = el.scrollHeight - el.scrollTop - el.clientHeight < 200;
        }}
      >
        <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
          {virtualizer.getVirtualItems().map((vi) => (
            <div
              key={vi.key}
              data-index={vi.index}
              ref={virtualizer.measureElement}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${vi.start}px)`,
              }}
            >
              <MessageItem msg={messages[vi.index]} />
            </div>
          ))}
        </div>
        {thinking && (
          <div className="thinking-live">
            <div className="thinking-live-head">思考中…</div>
            <div className="thinking-live-body">
              {messages.length > 0 && messages[messages.length - 1].reasoningContent
                ? messages[messages.length - 1].reasoningContent
                : "正在等待模型响应…"}
            </div>
          </div>
        )}
      </div>

      <div className="input-bar">
        <textarea
          ref={inputRef}
          value={input}
          placeholder="输入消息，Enter 发送，Shift+Enter 换行"
          onChange={(e) => onInput(e.target.value)}
          onKeyDown={handleKey}
          onKeyUp={handleKeyUp}
          onCompositionStart={() => (composingRef.current = true)}
          onCompositionEnd={() => {
            composingRef.current = false;
            lastCompositionEndRef.current = Date.now();
          }}
          rows={4}
        />
        {streaming ? (
          <button className="send-btn stop" onClick={onStop} title="停止">
            <Square size={16} />
          </button>
        ) : (
          <button className="send-btn" onClick={onSend} title="发送">
            <Send size={16} />
          </button>
        )}
      </div>
    </div>
  );
}
