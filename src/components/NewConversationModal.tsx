// 新建对话弹窗：项目名称 + 工作文件夹（可原生选择目录）
import { useRef, useState } from "react";
import { FolderOpen } from "lucide-react";
import { pickDirectory } from "../lib/api";

interface Props {
  open: boolean;
  onClose: () => void;
  onCreate: (title: string, workDir: string) => void;
}

export default function NewConversationModal({ open, onClose, onCreate }: Props) {
  const [title, setTitle] = useState("");
  const [workDir, setWorkDir] = useState("");
  const composingRef = useRef(false);
  const enterDuringCompositionRef = useRef(false);
  const lastCompositionEndRef = useRef(0);

  if (!open) return null;

  const create = () => {
    onCreate(title.trim() || "新对话", workDir.trim());
    setTitle("");
    setWorkDir("");
    onClose();
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h3>新建对话</h3>
        </div>

        <div className="form-row">
          <label>对话名称</label>
          <input
            autoFocus
            value={title}
            placeholder="新对话"
            onChange={(e) => setTitle(e.target.value)}
            onCompositionStart={() => (composingRef.current = true)}
            onCompositionEnd={() => {
              composingRef.current = false;
              lastCompositionEndRef.current = Date.now();
            }}
            onKeyDown={(e) => {
              if (e.key !== "Enter") return;
              if (e.nativeEvent.isComposing || composingRef.current) {
                enterDuringCompositionRef.current = true;
                return;
              }
              if (enterDuringCompositionRef.current) return;
              if (Date.now() - lastCompositionEndRef.current < 300) {
                enterDuringCompositionRef.current = true;
                return;
              }
              create();
            }}
            onKeyUp={(e) => {
              if (e.key === "Enter" && enterDuringCompositionRef.current) {
                enterDuringCompositionRef.current = false;
              }
            }}
          />
        </div>

        <div className="form-row">
          <label>工作文件夹（项目目录）</label>
          <div className="dir-input-row">
            <input
              value={workDir}
              placeholder="/Users/you/project 或点击右侧选择"
              onChange={(e) => setWorkDir(e.target.value)}
              onCompositionStart={() => (composingRef.current = true)}
              onCompositionEnd={() => {
                composingRef.current = false;
                lastCompositionEndRef.current = Date.now();
              }}
              onKeyDown={(e) => {
                if (e.key !== "Enter") return;
                if (e.nativeEvent.isComposing || composingRef.current) {
                  enterDuringCompositionRef.current = true;
                  return;
                }
                if (enterDuringCompositionRef.current) return;
                if (Date.now() - lastCompositionEndRef.current < 300) {
                  enterDuringCompositionRef.current = true;
                  return;
                }
                create();
              }}
              onKeyUp={(e) => {
                if (e.key === "Enter" && enterDuringCompositionRef.current) {
                  enterDuringCompositionRef.current = false;
                }
              }}
            />
            <button
              className="icon-btn"
              title="选择文件夹"
              onClick={async () => {
                const dir = await pickDirectory();
                if (dir) setWorkDir(dir);
              }}
            >
              <FolderOpen size={16} />
            </button>
          </div>
        </div>

        <div className="modal-actions">
          <button className="btn-primary" onClick={create}>创建</button>
          <button className="btn-ghost" onClick={onClose}>取消</button>
        </div>
      </div>
    </div>
  );
}
