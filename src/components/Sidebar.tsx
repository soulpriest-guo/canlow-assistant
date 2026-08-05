import { useState } from "react";
import { MessageSquarePlus, Pencil, Trash2 } from "lucide-react";
import type { ConversationMeta } from "../types";
import Dialog, { type DialogState } from "./Dialog";

interface Props {
  conversations: ConversationMeta[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onDelete: (id: string) => void;
  onRename: (id: string, title: string) => void;
}

export default function Sidebar({
  conversations,
  activeId,
  onSelect,
  onNew,
  onDelete,
  onRename,
}: Props) {
  const [dialog, setDialog] = useState<DialogState | null>(null);

  const askRename = (id: string, current: string) => {
    setDialog({
      type: "input",
      title: "重命名对话",
      placeholder: "新名称",
      initial: current,
      onSubmit: (v) => {
        if (v.trim()) onRename(id, v.trim());
      },
    });
  };

  const askDelete = (c: ConversationMeta) => {
    setDialog({
      type: "confirm",
      title: "删除对话",
      message: `确定删除对话「${c.title}」？此操作不可恢复。`,
      confirmLabel: "删除",
      danger: true,
      onSubmit: () => onDelete(c.id),
    });
  };

  return (
    <aside className="sidebar">
      <div className="sidebar-header">
        <span className="logo">Canlow</span>
        <button className="icon-btn" title="新对话" onClick={onNew}>
          <MessageSquarePlus size={18} />
        </button>
      </div>
      <div className="conv-list">
        {conversations.map((c) => (
          <div
            key={c.id}
            className={"conv-item" + (c.id === activeId ? " active" : "")}
            onClick={() => onSelect(c.id)}
            onDoubleClick={() => askRename(c.id, c.title)}
            title="双击重命名"
          >
            <span className="conv-main">
              <span className="conv-title">{c.title || "未命名对话"}</span>
              {c.workDir && <span className="conv-dir">{c.workDir}</span>}
            </span>
            <span className="conv-actions">
              <button
                className="conv-btn"
                title="重命名"
                onClick={(e) => {
                  e.stopPropagation();
                  askRename(c.id, c.title);
                }}
              >
                <Pencil size={12} />
              </button>
              <button
                className="conv-btn"
                title="删除"
                onClick={(e) => {
                  e.stopPropagation();
                  askDelete(c);
                }}
              >
                <Trash2 size={12} />
              </button>
            </span>
          </div>
        ))}
      </div>
      <div className="sidebar-footer">
        <span className="status-dot" /> 本地存储 · SQLite
      </div>
      <Dialog state={dialog} onClose={() => setDialog(null)} />
    </aside>
  );
}
