// 设置弹窗：左侧设置项目导航 + 右侧内容面板（含技能管理）
import { useEffect, useState } from "react";
import { BookOpen, Database, FolderOpen, Plus, RefreshCw, Server, Sparkles, Trash2, X } from "lucide-react";
import type { ProviderDef } from "../types";
import {
  contextProfileGet,
  contextProfileSet,
  customProviderAdd,
  customProviderRemove,
  providerKeyStatus,
  providerSaveKey,
  providerSetModels,
  providerTest,
  providersList,
  skillCreate,
  skillOpenDir,
  skillsList,
} from "../lib/api";

interface Props {
  open: boolean;
  onClose: () => void;
  onProvidersChanged: () => void;
  /** 当前会话工作目录（技能的项目级位置） */
  workDir?: string;
  /** 在对话中应用技能：发送使用指令 */
  onUseSkill?: (name: string) => void;
}

type Section = "api" | "context" | "skills";

export default function SettingsModal({ open, onClose, onProvidersChanged, workDir, onUseSkill }: Props) {
  const [section, setSection] = useState<Section>("api");
  const [providers, setProviders] = useState<ProviderDef[]>([]);
  const [selected, setSelected] = useState<string>("DeepSeek");
  const [keyInput, setKeyInput] = useState("");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [keyStatus, setKeyStatus] = useState<string | null>(null);
  const [editableModels, setEditableModels] = useState<string[]>([]);
  const [newModel, setNewModel] = useState("");
  const [contextProfile, setContextProfile] = useState<"1m" | "256k">("1m");
  const [showCustom, setShowCustom] = useState(false);
  const [customName, setCustomName] = useState("");
  const [customUrl, setCustomUrl] = useState("");
  const [customModels, setCustomModels] = useState("");
  // 技能管理
  const [skills, setSkills] = useState<[string, string, string, string][]>([]);
  const [newSkillName, setNewSkillName] = useState("");
  const [newSkillDesc, setNewSkillDesc] = useState("");
  const [newSkillContent, setNewSkillContent] = useState("");
  const [skillMsg, setSkillMsg] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      skillsList(workDir || "").then(setSkills).catch(() => {});
      contextProfileGet().then((p) => setContextProfile(p === "256k" ? "256k" : "1m")).catch(() => {});
      providersList().then((ps) => {
        setProviders(ps);
        if (ps.length && !ps.some((p) => p.name === selected)) {
          setSelected(ps[0].name);
        }
        const current = ps.some((p) => p.name === selected) ? selected : ps[0]?.name;
        if (current) {
          providerKeyStatus(current).then(setKeyStatus).catch(() => setKeyStatus(null));
        }
      });
      setTestResult(null);
    }
  }, [open]); // eslint-disable-line

  useEffect(() => {
    if (open && selected) {
      providerKeyStatus(selected).then(setKeyStatus).catch(() => setKeyStatus(null));
      const p = providers.find((x) => x.name === selected);
      setEditableModels(p ? [...p.models] : []);
    }
  }, [selected, open, providers]); // eslint-disable-line

  if (!open) return null;
  const current = providers.find((p) => p.name === selected);

  const saveKey = async () => {
    if (!keyInput.trim()) return;
    await providerSaveKey(selected, keyInput.trim());
    setKeyInput("");
    setTestResult("✅ Key 已保存");
    providerKeyStatus(selected).then(setKeyStatus).catch(() => setKeyStatus(null));
  };

  const test = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      const r = await providerTest(selected);
      setTestResult(r);
    } catch (e) {
      setTestResult(`❌ ${String(e)}`);
    }
    setTesting(false);
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal settings-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h3>设置</h3>
          <button className="icon-btn" onClick={onClose}>
            <X size={16} />
          </button>
        </div>

        <div className="settings-layout">
          {/* 左侧：设置项目导航 */}
          <nav className="settings-nav">
            <button
              className={"settings-nav-item" + (section === "api" ? " active" : "")}
              onClick={() => setSection("api")}
            >
              <Server size={14} /> API 提供商
            </button>
            <button
              className={"settings-nav-item" + (section === "context" ? " active" : "")}
              onClick={() => setSection("context")}
            >
              <Database size={14} /> 上下文长度
            </button>
            <button
              className={"settings-nav-item" + (section === "skills" ? " active" : "")}
              onClick={() => setSection("skills")}
            >
              <BookOpen size={14} /> 技能
            </button>
          </nav>

          {/* 右侧：内容面板 */}
          <div className="settings-content">
            {section === "api" ? (
              <div className="provider-layout">
                <div className="provider-list">
                  {providers.map((p) => (
                    <div
                      key={p.name}
                      className={"provider-item" + (p.name === selected ? " active" : "")}
                      onClick={() => {
                        setSelected(p.name);
                        setTestResult(null);
                      }}
                    >
                      <span className="provider-name">{p.name}</span>
                      {!["DeepSeek", "OpenAI", "智谱AI (Zhipu)", "MiniMax", "小米 (Xiaomi MiMo)", "Kimi (Moonshot)"].includes(p.name) && (
                        <button
                          className="provider-del"
                          title="删除自定义提供商"
                          onClick={async (e) => {
                            e.stopPropagation();
                            await customProviderRemove(p.name);
                            const ps = await providersList();
                            setProviders(ps);
                            setSelected(ps[0]?.name || "");
                            onProvidersChanged();
                          }}
                        >
                          <Trash2 size={12} />
                        </button>
                      )}
                    </div>
                  ))}
                </div>

                <div className="provider-detail">
                  {current && (
                    <>
                      <div className="form-row">
                        <label>接口地址</label>
                        <input value={current.baseUrl} readOnly />
                      </div>
                      <div className="form-row">
                        <label>
                          可用模型（可自行增删，保存后生效）
                          <span className="key-status">{current.models.length} 个</span>
                        </label>
                        <div className="model-tags">
                          {editableModels.map((m) => (
                            <span key={m} className="model-tag">
                              {m}
                              <button
                                className="model-tag-del"
                                title="删除该模型"
                                onClick={() => setEditableModels((prev) => prev.filter((x) => x !== m))}
                              >
                                ×
                              </button>
                            </span>
                          ))}
                        </div>
                        <div className="dir-input-row" style={{ marginTop: 8 }}>
                          <input
                            value={newModel}
                            placeholder="输入新模型名"
                            onChange={(e) => setNewModel(e.target.value)}
                            onKeyDown={(e) => {
                              if (e.key === "Enter" && newModel.trim()) {
                                setEditableModels((prev) => [...prev, newModel.trim()]);
                                setNewModel("");
                              }
                            }}
                          />
                          <button
                            className="btn-ghost"
                            onClick={() => {
                              if (newModel.trim()) {
                                setEditableModels((prev) => [...prev, newModel.trim()]);
                                setNewModel("");
                              }
                            }}
                          >
                            添加
                          </button>
                          <button
                            className="btn-primary"
                            onClick={async () => {
                              await providerSetModels(selected, editableModels);
                              const ps = await providersList();
                              setProviders(ps);
                              onProvidersChanged();
                            }}
                          >
                            保存模型
                          </button>
                        </div>
                      </div>
                      <div className="form-row">
                        <label>
                          API Key（保存到本机）
                          {keyStatus && <span className="key-status">● 已配置：{keyStatus}</span>}
                          {keyStatus === null && <span className="key-status unset">○ 未配置</span>}
                        </label>
                        <div className="dir-input-row">
                          <input
                            type="password"
                            value={keyInput}
                            placeholder={keyStatus ? "输入新 Key 可覆盖" : "sk-..."}
                            onChange={(e) => setKeyInput(e.target.value)}
                            onKeyDown={(e) => e.key === "Enter" && saveKey()}
                          />
                          <button className="btn-primary" onClick={saveKey}>保存</button>
                        </div>
                      </div>
                      <div className="form-row">
                        <button className="btn-ghost" onClick={test} disabled={testing}>
                          <RefreshCw size={13} /> {testing ? "测试中…" : "测试连接"}
                        </button>
                        {testResult && <div className="test-result">{testResult}</div>}
                      </div>
                    </>
                  )}

                  {!showCustom ? (
                    <button className="btn-ghost add-custom" onClick={() => setShowCustom(true)}>
                      <Plus size={13} /> 添加自定义提供商
                    </button>
                  ) : (
                    <div className="custom-form">
                      <div className="form-row">
                        <label>名称</label>
                        <input value={customName} onChange={(e) => setCustomName(e.target.value)} placeholder="如: 我的API" />
                      </div>
                      <div className="form-row">
                        <label>接口地址</label>
                        <input value={customUrl} onChange={(e) => setCustomUrl(e.target.value)} placeholder="https://api.example.com/v1" />
                      </div>
                      <div className="form-row">
                        <label>模型（逗号分隔）</label>
                        <input value={customModels} onChange={(e) => setCustomModels(e.target.value)} placeholder="model-a, model-b" />
                      </div>
                      <div className="modal-actions">
                        <button
                          className="btn-primary"
                          onClick={async () => {
                            await customProviderAdd(
                              customName,
                              customUrl,
                              customModels.split(",").map((m) => m.trim()).filter(Boolean)
                            );
                            setCustomName("");
                            setCustomUrl("");
                            setCustomModels("");
                            setShowCustom(false);
                            const ps = await providersList();
                            setProviders(ps);
                            setSelected(customName.trim());
                            onProvidersChanged();
                          }}
                        >
                          添加
                        </button>
                        <button className="btn-ghost" onClick={() => setShowCustom(false)}>取消</button>
                      </div>
                    </div>
                  )}
                </div>
              </div>
            ) : section === "context" ? (
              <div className="context-section">
                <div className="form-row">
                  <label>上下文档位</label>
                  <select
                    value={contextProfile}
                    onChange={async (e) => {
                      const v = e.target.value as "1m" | "256k";
                      setContextProfile(v);
                      await contextProfileSet(v);
                    }}
                  >
                    <option value="1m">1M 上下文（DeepSeek v4 等大窗口模型）</option>
                    <option value="256k">256k 上下文（OpenAI / Kimi 等小窗口模型）</option>
                  </select>
                </div>
                <p className="modal-hint">
                  档位决定上下文压缩触发时机与 AI 的预算感知，影响缓存命中率与长对话体验。
                </p>
                <div className="profile-table">
                  <div className="profile-row head">
                    <span>参数</span>
                    <span>1M 档</span>
                    <span>256k 档</span>
                  </div>
                  <div className="profile-row">
                    <span>压缩触发阈值</span>
                    <span>180 万字符（~60 万 tokens）</span>
                    <span>60 万字符（~20 万 tokens）</span>
                  </div>
                  <div className="profile-row">
                    <span>保留用户消息</span>
                    <span>最多 12 万字符（~4 万 tokens）</span>
                    <span>最多 6 万字符（~2 万 tokens）</span>
                  </div>
                  <div className="profile-row">
                    <span>保留最近消息</span>
                    <span>40 万字符（~13 万 tokens）</span>
                    <span>15 万字符（~5 万 tokens）</span>
                  </div>
                  <div className="profile-row">
                    <span>上下文预算（AI 报告口径）</span>
                    <span>180 万字符（~60 万 tokens）</span>
                    <span>60 万字符（~20 万 tokens）</span>
                  </div>
                </div>
              </div>
            ) : (
              <div className="skills-section">
                <div className="skills-toolbar">
                  <button
                    className="btn-ghost"
                    onClick={async () => {
                      try {
                        const p = await skillOpenDir(workDir || "", "project");
                        setSkillMsg("已打开项目技能目录：" + p);
                      } catch (e) { setSkillMsg("❌ " + String(e)); }
                    }}
                  >
                    <FolderOpen size={13} /> 打开项目技能目录
                  </button>
                  <button
                    className="btn-ghost"
                    onClick={async () => {
                      try {
                        const p = await skillOpenDir(workDir || "", "user");
                        setSkillMsg("已打开用户技能目录：" + p);
                      } catch (e) { setSkillMsg("❌ " + String(e)); }
                    }}
                  >
                    <FolderOpen size={13} /> 打开用户技能目录
                  </button>
                </div>
                <p className="modal-hint">
                  技能 = 一组指令文件（<code>.canlow/skills/&lt;名称&gt;/SKILL.md</code>，文件头用 <code>---</code> 标注 name/description）。
                  AI 会自动知道可用技能；对话中说「使用技能 xxx」或点下方「应用」即可让它加载执行。
                </p>
                {skillMsg && <div className="test-result">{skillMsg}</div>}
                <div className="skills-list">
                  {skills.length === 0 && <p className="modal-hint">（暂无技能。可在下方新建，或在技能目录里手动放 SKILL.md 文件）</p>}
                  {skills.map(([name, desc, source, path]) => (
                    <div key={name} className="skill-item">
                      <div className="skill-item-info">
                        <span className="skill-item-name"><Sparkles size={12} /> {name}</span>
                        <span className="skill-item-desc">{desc || "（无描述）"}</span>
                        <span className="skill-item-path" title={path}>{source} · {path}</span>
                      </div>
                      <button
                        className="btn-primary skill-item-use"
                        title="关闭设置并向当前对话发送使用指令"
                        onClick={() => {
                          onUseSkill?.(name);
                          onClose();
                        }}
                      >
                        应用
                      </button>
                    </div>
                  ))}
                </div>
                <div className="skill-create">
                  <div className="form-row">
                    <label>新建技能</label>
                  </div>
                  <div className="form-row">
                    <input value={newSkillName} onChange={(e) => setNewSkillName(e.target.value)} placeholder="技能名称（英文/数字/-_）" />
                  </div>
                  <div className="form-row">
                    <input value={newSkillDesc} onChange={(e) => setNewSkillDesc(e.target.value)} placeholder="一句话描述（AI 判断何时使用）" />
                  </div>
                  <div className="form-row">
                    <textarea
                      className="skill-content-input"
                      value={newSkillContent}
                      onChange={(e) => setNewSkillContent(e.target.value)}
                      placeholder={"技能指令正文：告诉 AI 何时使用、如何一步步执行、有什么注意事项。\n（留空会生成模板）"}
                      rows={5}
                    />
                  </div>
                  <div className="modal-actions">
                    <button
                      className="btn-primary"
                      onClick={async () => {
                        try {
                          const p = await skillCreate(workDir || "", newSkillName, newSkillDesc, newSkillContent);
                          setSkillMsg("✅ 技能已创建：" + p);
                          setNewSkillName(""); setNewSkillDesc(""); setNewSkillContent("");
                          const list = await skillsList(workDir || "");
                          setSkills(list);
                        } catch (e) { setSkillMsg("❌ " + String(e)); }
                      }}
                    >
                      <Plus size={13} /> 创建技能
                    </button>
                    <span className="modal-hint">
                      {workDir ? "写入项目目录（当前会话工作区）" : "未设置工作目录，写入用户目录 ~/.canlow/skills/"}
                    </span>
                  </div>
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
