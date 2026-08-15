// 会话工具栏：工作目录 / 模型 / 思考强度 / 会话日志导出（会话级）
import { useRef, useState } from "react";
import { Download, FolderOpen } from "lucide-react";
import type { ConversationMeta, ProviderDef } from "../types";
import { pickDirectory, sessionExport } from "../lib/api";

const EFFORTS = ["low", "high", "max"];

interface Props {
  conv: ConversationMeta | null;
  providers: ProviderDef[];
  cacheRate: number;
  cacheHit: number;
  cacheMiss: number;
  engineeringMode: boolean;
  onToggleEngineering: (v: boolean) => void;
  onChange: (patch: { provider?: string; model?: string; reasoningEffort?: string; workDir?: string }) => Promise<void>;
}

export default function SessionBar({ conv, providers, cacheRate, cacheHit, cacheMiss, engineeringMode, onToggleEngineering, onChange }: Props) {
  const [editingDir, setEditingDir] = useState(false);
  const [dirInput, setDirInput] = useState("");
  const composingRef = useRef(false);
  const enterDuringCompositionRef = useRef(false);
  const lastCompositionEndRef = useRef(0);

  if (!conv) return null;

  const dirLabel = conv.workDir || "未设置工作目录";

  return (
    <div className="session-bar">
      <button
        className="sb-item sb-dir"
        title={dirLabel + "（点击修改工作目录）"}
        onClick={() => {
          setDirInput(conv.workDir);
          setEditingDir(true);
        }}
      >
        <FolderOpen size={13} />
        <span className="sb-dir-text">{dirLabel}</span>
      </button>

      <button
        className="sb-item sb-export"
        title="导出会话日志：JSONL（完整事件日志，含推理/工具调用/任务图）或 Markdown（可读转写）；文件名后缀 .md 导出 Markdown，否则导出 JSONL"
        onClick={async () => {
          try {
            const { save } = await import("@tauri-apps/plugin-dialog");
            const safeTitle = (conv.title || "会话").replace(/[\\/:*?"<>|\s]+/g, "-").slice(0, 40);
            const path = await save({
              defaultPath: "canlow-" + safeTitle + ".jsonl",
              filters: [
                { name: "会话日志", extensions: ["jsonl", "md"] },
              ],
            });
            if (!path) return;
            const format = path.toLowerCase().endsWith(".md") ? "markdown" : "jsonl";
            const written = await sessionExport(conv.id, path, format);
            alert("✅ 会话日志已导出：" + written);
          } catch (e) {
            alert("❌ 导出失败：" + String(e));
          }
        }}
      >
        <Download size={13} />
        导出
      </button>

      <label className="sb-item">
        提供商
        <select
          value={providers.some((p) => p.name === conv.provider) ? conv.provider : conv.provider}
          onChange={(e) => {
            const p = providers.find((x) => x.name === e.target.value);
            onChange({
              provider: e.target.value,
              model: p?.models?.[0] || conv.model,
            });
          }}
        >
          {providers.map((p) => (
            <option key={p.name} value={p.name}>{p.name}</option>
          ))}
        </select>
      </label>

      <label className="sb-item">
        模型
        <select
          value={conv.model}
          onChange={(e) => onChange({ model: e.target.value })}
        >
          {(providers.find((p) => p.name === conv.provider)?.models || []).map((m) => (
            <option key={m} value={m}>{m}</option>
          ))}
          {conv.model &&
            !(providers.find((p) => p.name === conv.provider)?.models || []).includes(conv.model) && (
              <option value={conv.model}>{conv.model}（自定义）</option>
            )}
        </select>
      </label>

      <label className="sb-item">
        思考
        <select
          value={conv.reasoningEffort}
          onChange={(e) => onChange({ reasoningEffort: e.target.value })}
        >
          {EFFORTS.map((e) => (
            <option key={e} value={e}>
              {e === "low" ? "低" : e === "high" ? "高" : "最高"}
            </option>
          ))}
        </select>
      </label>

      <label className="sb-item sb-eng" title="工程模式：AI 先建任务图再执行">
        <input
          type="checkbox"
          checked={engineeringMode}
          onChange={(e) => onToggleEngineering(e.target.checked)}
        />
        工程模式
      </label>

      <span
        className={"sb-item sb-cache" + (cacheRate >= 90 ? " good" : "")}
        title={"DeepSeek 前缀缓存命中率（prompt_cache_hit_tokens）：命中 " + cacheHit.toLocaleString() + " / 未命中 " + cacheMiss.toLocaleString() + " tokens"}
      >
        ⚡ 缓存 {cacheRate.toFixed(1)}%
      </span>

      {editingDir && (
        <div className="modal-mask" onClick={() => setEditingDir(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <h3>修改工作目录</h3>
            </div>
            <div className="form-row">
              <div className="dir-input-row">
                <input
                  autoFocus
                  value={dirInput}
                  placeholder="项目文件夹路径"
                  onChange={(e) => setDirInput(e.target.value)}
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
                    if (dirInput.trim()) {
                      onChange({ workDir: dirInput.trim() });
                      setEditingDir(false);
                    }
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
                    if (dir) setDirInput(dir);
                  }}
                >
                  <FolderOpen size={16} />
                </button>
              </div>
            </div>
            <div className="modal-actions">
              <button
                className="btn-primary"
                onClick={() => {
                  if (dirInput.trim()) {
                    onChange({ workDir: dirInput.trim() });
                    setEditingDir(false);
                  }
                }}
              >
                保存
              </button>
              <button className="btn-ghost" onClick={() => setEditingDir(false)}>
                取消
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
