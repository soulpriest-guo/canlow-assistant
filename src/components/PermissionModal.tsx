import { ShieldAlert } from "lucide-react";
import type { PermissionRequest } from "../types";

interface Props {
  req: PermissionRequest | null;
  onAnswer: (allow: boolean) => void;
}

export default function PermissionModal({ req, onAnswer }: Props) {
  if (!req) return null;
  return (
    <div className="modal-mask">
      <div className="modal">
        <div className="modal-header">
          <h3 style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <ShieldAlert size={18} color="#ffd479" /> 工具执行授权
          </h3>
        </div>
        <p className="perm-desc">
          <b>{req.toolName}</b> 请求执行：
        </p>
        <pre className="perm-args">{req.description}</pre>
        <p className="modal-hint">
          是否允许？也可在设置中开启"自动允许工具执行"跳过询问。
        </p>
        <div className="modal-actions">
          <button className="btn-primary" onClick={() => onAnswer(true)}>
            允许本次
          </button>
          <button className="btn-ghost" onClick={() => onAnswer(false)}>
            拒绝
          </button>
        </div>
      </div>
    </div>
  );
}
