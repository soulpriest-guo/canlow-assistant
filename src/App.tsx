import { useState } from "react";
import { GitBranch, MessageSquare, Settings, ShieldCheck } from "lucide-react";
import { useChat } from "./hooks/useChat";
import Sidebar from "./components/Sidebar";
import ChatView from "./components/ChatView";
import TaskGraphView from "./components/TaskGraphView";
import SettingsModal from "./components/SettingsModal";
import PermissionModal from "./components/PermissionModal";
import NewConversationModal from "./components/NewConversationModal";
import SessionBar from "./components/SessionBar";
import { sessionUpdate } from "./lib/api";

export default function App() {
  const chat = useChat();
  const [view, setView] = useState<"chat" | "graph">("chat");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [newConvOpen, setNewConvOpen] = useState(false);
  const activeConv = chat.conversations.find((c) => c.id === chat.activeId) || null;

  return (
    <div className="app">
      <Sidebar
        conversations={chat.conversations}
        activeId={chat.activeId}
        onSelect={chat.setActiveId}
        onNew={() => setNewConvOpen(true)}
        onDelete={chat.deleteConversation}
        onRename={chat.renameConversation}
      />
      <main className="main">
        <div className="view-tabs">
          <button
            className={"tab" + (view === "chat" ? " active" : "")}
            onClick={() => setView("chat")}
          >
            <MessageSquare size={15} /> 对话
          </button>
          <button
            className={"tab" + (view === "graph" ? " active" : "")}
            onClick={() => setView("graph")}
          >
            <GitBranch size={15} /> 工程
          </button>
          <div className="tab-spacer" />
          <label className="auth-mode-select" title="工具授权模式">
            <ShieldCheck size={15} />
            <select
              value={chat.authMode}
              onChange={(e) => chat.setAuthMode(e.target.value as "ask" | "smart" | "allow_all" | "none")}
            >
              <option value="ask">逐次询问</option>
              <option value="smart">智能（只读自动）</option>
              <option value="allow_all">仅确认计划</option>
              <option value="none">全自动</option>
            </select>
          </label>
          <button className="tab" onClick={() => setSettingsOpen(true)} title="API 设置">
            <Settings size={15} /> 设置
          </button>
        </div>

        {view === "chat" ? (
          <>
          <SessionBar
            conv={activeConv}
            providers={chat.providers}
            cacheRate={chat.cacheStats.rate}
            engineeringMode={activeConv?.engineeringMode ?? false}
            onToggleEngineering={async (v) => {
              if (!chat.activeId) return;
              await sessionUpdate(chat.activeId, { engineeringMode: v });
              await chat.refreshConversations();
            }}
            onChange={async (patch) => {
              if (!chat.activeId) return;
              await sessionUpdate(chat.activeId, patch);
              await chat.refreshConversations();
            }}
          />
          <ChatView
            messages={chat.messages}
            thinking={chat.thinking}
            streaming={chat.streaming}
            input={chat.input}
            onInput={chat.setInput}
            onSend={chat.send}
            onStop={chat.stop}
            scrollResetKey={chat.activeId}
          />
          </>
        ) : activeConv?.engineeringMode ? (
          <TaskGraphView
            activeId={chat.activeId}
            refreshKey={chat.mapDirty}
            streaming={chat.streaming}
            authMode={chat.authMode}
            onInterrupt={chat.stop}
            onResume={chat.resume}
            onSendPlan={(text, planOnly) => chat.sendText(text, planOnly)}
          />
        ) : (
          <div className="graph-empty">
            <p>工程模式未开启</p>
            <p>勾选「工程模式」后，这里将显示任务图（AI 始终按计划执行，任务图数据照常维护）。</p>
          </div>
        )}
      </main>

      <SettingsModal
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        onProvidersChanged={chat.refreshProviders}
      />
      <PermissionModal req={chat.permission} onAnswer={chat.answerPermission} />

      {/* 计划确认弹窗：AI 修改任务图结构后全局提示（对话页/工程页都可见，与授权模式无关） */}
      {chat.planConfirm && (
        <div className="modal-mask">
          <div className="modal">
            <div className="modal-header">
              <h3>任务图变更确认</h3>
            </div>
            <p className="modal-hint">
              {chat.planConfirm.hasMap
                ? "AI 已修改任务图，是否同意并继续执行？"
                : "AI 已创建任务图，是否同意并开始执行？"}
            </p>
            {chat.planConfirm.summary && (
              <div className="plan-confirm-summary">{chat.planConfirm.summary}</div>
            )}
            <div className="modal-actions">
              <button className="btn-primary" onClick={() => chat.answerPlanConfirm(true)}>
                ✓ 同意并继续
              </button>
              <button className="btn-danger" onClick={() => chat.answerPlanConfirm(false)}>
                ✗ 不同意（回滚）
              </button>
            </div>
          </div>
        </div>
      )}
      <NewConversationModal
        open={newConvOpen}
        onClose={() => setNewConvOpen(false)}
        onCreate={async (title, workDir) => {
          await chat.createConversation(title, workDir);
        }}
      />
    </div>
  );
}
