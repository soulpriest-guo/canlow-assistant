// 通用对话框：Tauri WebView 里 window.prompt/confirm 不可用，统一用这个
import { useRef, useState } from "react";

export interface DialogState {
  type: "input" | "confirm";
  title: string;
  placeholder?: string;
  initial?: string;
  message?: string;
  confirmLabel?: string;
  danger?: boolean;
  onSubmit?: (value: string) => void;
}

export default function Dialog({ state, onClose }: { state: DialogState | null; onClose: () => void }) {
  const [value, setValue] = useState(state?.initial || "");
  const composingRef = useRef(false);
  const enterDuringCompositionRef = useRef(false);
  const lastCompositionEndRef = useRef(0);
  if (!state) return null;

  const submit = () => {
    state.onSubmit?.(value);
    onClose();
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h3>{state.title}</h3>
        </div>

        {state.type === "input" ? (
          <div className="form-row">
            <input
              autoFocus
              value={value}
              placeholder={state.placeholder || ""}
              onChange={(e) => setValue(e.target.value)}
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
                submit();
              }}
              onKeyUp={(e) => {
                if (e.key === "Enter" && enterDuringCompositionRef.current) {
                  enterDuringCompositionRef.current = false;
                }
              }}
            />
          </div>
        ) : (
          <p className="modal-hint" style={{ fontSize: 14 }}>
            {state.message}
          </p>
        )}

        <div className="modal-actions">
          <button
            className={state.danger ? "btn-danger" : "btn-primary"}
            onClick={submit}
          >
            {state.confirmLabel || "确定"}
          </button>
          <button className="btn-ghost" onClick={onClose}>
            取消
          </button>
        </div>
      </div>
    </div>
  );
}
