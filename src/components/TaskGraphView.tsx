// 任务图（简化版）
// - 功能仅保留：图显示（父子关系 + 排序/层级布局）、点击节点查看详情、任务完成进度、折叠子任务
// - 控制按钮仅三个：打断（停止当前进程）/ 继续（恢复被中断的进程）/ 刷新（重载任务图状态）
// - 任务设计对话框（底部）：
//   · 未开始（无任务图）：发送内容 → AI 直接建图并执行
//   · 已开始：发送内容 → 自动打断当前进程 → AI 仅规划调整任务图（plan_only）→ 询问是否同意
//     - 同意 → 保留调整，继续进程
//     - 不同意 → 回滚任务图快照，等待用户重新输入（不开始）
//     - 未发送第二轮内容时点「继续」→ 回滚并按原进程继续
import { useCallback, useEffect, useRef, useState } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  applyNodeChanges,
  Handle,
  Position,
  type Node,
  type Edge,
  type OnNodesChange,
} from "@xyflow/react";
import { Send } from "lucide-react";
import { taskmapGet, taskmapSave, taskmapSyncMemory } from "../lib/api";
import type { TaskMapData, TaskNode, TaskStatus } from "../types";

const STATUS_META: Record<TaskStatus, { icon: string; color: string; label: string }> = {
  todo: { icon: "⬜", color: "#8b93a7", label: "待办" },
  in_progress: { icon: "🔵", color: "#4f8cff", label: "进行中" },
  done: { icon: "✅", color: "#35d07f", label: "完成" },
  blocked: { icon: "⛔", color: "#ff5c6c", label: "阻塞" },
};

interface NodeData {
  /** 层级编号（如 a1 / a1b2 / b2c1） */
  id?: string;
  label: string;
  status: TaskStatus;
  progress: number;
  detail: string;
  isRoot: boolean;
  /** 是否一级任务（任务线）：root 直属子任务 或 独立顶层任务（parentId 为空） */
  isTopLevel?: boolean;
  /** 拓扑执行序号 */
  seq?: number;
  /** 下一步可执行高亮 */
  isNext?: boolean;
  /** 根节点聚合进度 */
  aggProgress?: number;
  /** 是否有子节点（决定是否显示折叠按钮） */
  hasChildren?: boolean;
  /** 直接子任务数量（折叠徽标显示） */
  childCount?: number;
  /** 当前是否折叠（隐藏其子孙节点） */
  collapsed?: boolean;
  onToggleCollapse?: () => void;
}

function TaskNodeWidget(props: { data: Record<string, unknown>; selected?: boolean }) {
  const d = props.data as unknown as NodeData;
  const meta = STATUS_META[d.status] || STATUS_META.todo;
  const cls =
    "task-node" +
    (props.selected ? " selected" : "") +
    (d.isNext ? " next" : "") +
    (d.isRoot ? " root" : "") +
    (d.isTopLevel ? " top-level" : "") +
    (d.status === "done" ? " done" : "") +
    (d.status === "in_progress" ? " running" : "");
  return (
    <div className={cls} style={{ borderColor: meta.color }}>
      {/* 连线锚点：左侧入线（父级），右侧出线（子级） */}
      <Handle type="target" position={Position.Left} className="task-handle" />
      <Handle type="source" position={Position.Right} className="task-handle" />
      <div className="task-node-title">
        {d.seq !== undefined && !d.isRoot && (
          <span className="task-seq" style={{ background: meta.color }}>
            {d.seq}
          </span>
        )}
        {d.id && !d.isRoot && <span className="task-id" title="层级编号">{d.id}</span>}
        <span>{meta.icon}</span> {d.label}
        {d.isTopLevel && !d.isRoot && (
          <span className="task-top-badge" title="一级任务（任务线）">一级</span>
        )}
        {d.hasChildren && (
          <button
            className={"task-collapse-btn" + (d.collapsed ? " collapsed" : "")}
            title={d.collapsed ? `展开 ${d.childCount ?? ""} 个子任务` : `收纳 ${d.childCount ?? ""} 个子任务`}
            onClick={(e) => {
              e.stopPropagation();
              d.onToggleCollapse?.();
            }}
          >
            {d.collapsed ? `▸ 展开 ${d.childCount ?? ""}` : `▾ 收纳 ${d.childCount ?? ""}`}
          </button>
        )}
      </div>
      {d.isRoot && d.aggProgress !== undefined ? (
        <div className="task-root-agg">
          <div className="task-root-agg-bar">
            <div className="task-root-agg-fill" style={{ width: `${d.aggProgress}%` }} />
          </div>
          <span className="task-root-agg-text">总体 {d.aggProgress}%</span>
        </div>
      ) : (
        <>
          {d.detail && <div className="task-node-detail">{d.detail.slice(0, 40)}</div>}
          <div className="task-node-progress">
            <div
              className="task-node-progress-bar"
              style={{ width: `${d.progress}%`, background: meta.color }}
            />
          </div>
        </>
      )}
      {d.collapsed && d.childCount !== undefined && d.childCount > 0 && (
        <div className="task-node-collapsed-hint">📁 已收纳 {d.childCount} 个子任务</div>
      )}
      {d.isNext && <div className="task-next-label">▶ 下一步</div>}
    </div>
  );
}

const nodeTypes = { task: TaskNodeWidget as never };

/** 任务设计对话框流程状态 */
type DesignState = "idle" | "planning" | "awaiting-confirm";

interface Props {
  activeId: string | null;
  /** AI 修改任务图后由外部递增触发自动刷新 */
  refreshKey: number;
  /** 当前是否有 agent 进程在运行 */
  streaming: boolean;
  /** 授权模式（全自动模式 none 时跳过任务设计确认，直接执行） */
  authMode: string;
  /** 打断当前进程 */
  onInterrupt: () => void;
  /** 继续当前进程（恢复被中断的 agent 循环） */
  onResume: () => Promise<void>;
  /** 发送任务设计内容给 AI；planOnly=true 时 AI 仅调整任务图、不执行 */
  onSendPlan: (text: string, planOnly: boolean) => Promise<void>;
}

export default function TaskGraphView({
  activeId,
  refreshKey,
  streaming,
  authMode,
  onInterrupt,
  onResume,
  onSendPlan,
}: Props) {
  const [taskMap, setTaskMap] = useState<TaskMapData | null>(null);
  const [nodes, setNodes] = useState<Node[]>([]);
  const [edges, setEdges] = useState<Edge[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  /** 折叠的节点 id 集合（父节点折叠时隐藏其全部子孙） */
  const [collapsedIds, setCollapsedIds] = useState<Set<string>>(new Set());
  // ref 同步折叠集合（applyMap/collectEdges 读取最新值，避免 setState 异步滞后）
  const collapsedRef = useRef<Set<string>>(new Set());
  const [notice, setNotice] = useState<string | null>(null);
  const taskMapRef = useRef<TaskMapData | null>(null);
  taskMapRef.current = taskMap;
  // 任务设计对话框状态
  const [designState, setDesignState] = useState<DesignState>("idle");
  const [designInput, setDesignInput] = useState("");
  /** 发送调整内容前的任务图快照（不同意/打断时回滚用） */
  const snapshotRef = useRef<TaskMapData | null>(null);
  /** 当前确认中的是「创建任务」还是「调整任务」（确认卡片文案区分） */
  const designCreateRef = useRef(false);
  /** 规划期间用户是否点了「打断」（用于取消规划，避免误入确认态） */
  const cancelledRef = useRef(false);
  const composingRef = useRef(false);
  const enterDuringCompositionRef = useRef(false);
  const lastCompositionEndRef = useRef(0);
  // DB 防抖保存（内存同步即时，落盘延迟）
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const noticeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** 最近一次加载的节点 ID 集合签名（结构指纹）：用于检测增删/移动节点后自动整理布局 */
  const structureSigRef = useRef<string>("");

  const flash = (msg: string) => {
    setNotice(msg);
    if (noticeTimerRef.current) clearTimeout(noticeTimerRef.current);
    noticeTimerRef.current = setTimeout(() => setNotice(null), 2600);
  };

  // ---------- 加载 ----------
  const load = useCallback(async () => {
    if (!activeId) {
      structureSigRef.current = "";
      setTaskMap(null);
      setNodes([]);
      setEdges([]);
      return;
    }
    // 恢复折叠状态（按会话记忆）
    const restored = loadCollapsed();
    collapsedRef.current = restored;
    setCollapsedIds(restored);
    try {
      const data = await taskmapGet(activeId);
      if (data) {
        const tm = data as TaskMapData;
        // 结构指纹：节点 ID 集合（增删/移动节点会变化；仅改状态/拖拽位置不变）
        const sig = Object.keys(tm.nodes).sort().join(",");
        const structuralChange =
          structureSigRef.current !== "" && structureSigRef.current !== sig;
        structureSigRef.current = sig;
        if (structuralChange) {
          // AI 重新规划/增删任务后：自动按导图规则整理布局，保持界面整洁
          const pos = computeLayoutPos(tm);
          const next: TaskMapData = { ...tm, nodes: { ...tm.nodes } };
          for (const [id, p] of Object.entries(pos)) {
            next.nodes[id] = { ...next.nodes[id], pos: p };
          }
          persist(next);
          applyMap(next);
          flash("⇲ 任务图结构已变化，已自动整理布局");
        } else {
          applyMap(tm);
        }
      } else {
        structureSigRef.current = "";
        setTaskMap(null);
        setNodes([]);
        setEdges([]);
      }
    } catch (e) {
      console.error(e);
    }
  }, [activeId]); // eslint-disable-line

  useEffect(() => {
    load();
  }, [load]);

  // AI 修改任务图后自动刷新
  useEffect(() => {
    if (refreshKey > 0) load();
  }, [refreshKey]); // eslint-disable-line

  // 切换会话：重置任务设计流程状态 + 结构签名（避免误判为新结构触发自动整理）
  useEffect(() => {
    setDesignState("idle");
    setDesignInput("");
    snapshotRef.current = null;
    cancelledRef.current = false;
    setSelectedId(null);
    structureSigRef.current = "";
  }, [activeId]);

  function applyMap(tm: TaskMapData) {
    setTaskMap(tm);
    const topo = computeTopoOrder(tm);
    const nextIds = computeNextTasks(tm);
    const agg = computeAggProgress(tm);
    const childrenOf = (pid: string) =>
      Object.values(tm.nodes).filter((n) => n.parentId === pid);
    // 新图（所有节点坐标未初始化）自动应用唯一导图布局规则
    const allNodes = Object.values(tm.nodes);
    const autoPos =
      allNodes.length > 1 && allNodes.every((n) => n.pos[0] === 0 && n.pos[1] === 0)
        ? computeLayoutPos(tm)
        : null;
    // 折叠：被折叠节点的子孙一律隐藏
    const hidden = new Set<string>();
    for (const cid of collapsedRef.current) {
      const stack = [...childrenOf(cid)];
      while (stack.length) {
        const cur = stack.pop()!;
        hidden.add(cur.id);
        stack.push(...childrenOf(cur.id));
      }
    }
    setNodes(
      Object.values(tm.nodes)
        .filter((n) => !hidden.has(n.id))
        .map((n) => ({
          id: n.id,
          type: "task",
          position: autoPos ? { x: autoPos[n.id][0], y: autoPos[n.id][1] } : { x: n.pos[0], y: n.pos[1] },
          data: {
            id: n.id,
            label: n.title,
            status: n.status,
            progress: n.progress,
            detail: n.detail,
            isRoot: n.id === tm.rootId,
            isTopLevel:
              n.id !== tm.rootId && (n.parentId === "" || n.parentId === tm.rootId),
            seq: topo.get(n.id),
            isNext: nextIds.has(n.id),
            aggProgress: n.id === tm.rootId ? agg : undefined,
            hasChildren: childrenOf(n.id).length > 0,
            childCount: childrenOf(n.id).length,
            collapsed: collapsedRef.current.has(n.id),
            onToggleCollapse: () => toggleCollapse(n.id),
          },
        }))
    );
    setEdges(collectEdges(tm));
  }

  function toggleCollapse(id: string) {
    const next = new Set(collapsedRef.current);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    collapsedRef.current = next;
    setCollapsedIds(next);
    persistCollapsed(next);
    if (taskMapRef.current) applyMap(taskMapRef.current);
  }

  // 折叠状态持久化（按会话记忆，刷新/切换不丢失）
  const COLLAPSED_KEY = "canlow-collapsed-v1";
  function persistCollapsed(set: Set<string>) {
    if (!activeId) return;
    try {
      const all = JSON.parse(localStorage.getItem(COLLAPSED_KEY) || "{}");
      all[activeId] = [...set];
      localStorage.setItem(COLLAPSED_KEY, JSON.stringify(all));
    } catch { /* 忽略 */ }
  }
  function loadCollapsed(): Set<string> {
    if (!activeId) return new Set();
    try {
      const all = JSON.parse(localStorage.getItem(COLLAPSED_KEY) || "{}");
      return new Set(all[activeId] || []);
    } catch {
      return new Set();
    }
  }

  // 拓扑排序（Kahn）：返回 id -> 执行序号
  function computeTopoOrder(tm: TaskMapData): Map<string, number> {
    const order = new Map<string, number>();
    const indeg = new Map<string, number>();
    for (const n of Object.values(tm.nodes)) indeg.set(n.id, n.deps.filter((d) => tm.nodes[d]).length);
    const ready: string[] = Object.values(tm.nodes)
      .filter((n) => n.id !== tm.rootId && (indeg.get(n.id) || 0) === 0)
      .map((n) => n.id);
    let counter = 1;
    while (ready.length) {
      const id = ready.shift()!;
      order.set(id, counter++);
      for (const n of Object.values(tm.nodes)) {
        if (n.deps.includes(id) && !order.has(n.id)) {
          const dd = (indeg.get(n.id) || 1) - 1;
          indeg.set(n.id, dd);
          if (dd === 0) ready.push(n.id);
        }
      }
    }
    return order;
  }

  // 下一步可执行：未完成且依赖全部完成
  function computeNextTasks(tm: TaskMapData): Set<string> {
    const next = new Set<string>();
    for (const n of Object.values(tm.nodes)) {
      if (n.id === tm.rootId || n.status === "done") continue;
      const depsDone = n.deps.every((d) => tm.nodes[d]?.status === "done");
      if (depsDone) next.add(n.id);
    }
    return next;
  }

  // 根节点聚合进度：子节点完成比例（不含根）
  function computeAggProgress(tm: TaskMapData): number {
    const all = Object.values(tm.nodes).filter((n) => n.id !== tm.rootId);
    if (all.length === 0) return 0;
    const done = all.filter((n) => n.status === "done").length;
    return Math.round((done / all.length) * 100);
  }

  // 父子分解线（唯一连线类型）：父 → 子，每个任务只有一条入线；
  // 执行顺序不用线表达，由排列位置体现（同级一律竖排，见 computeLayoutPos）
  function collectEdges(tm: TaskMapData): Edge[] {
    const out: Edge[] = [];
    // 折叠：被折叠节点的子孙隐藏，边也隐藏
    const hidden = new Set<string>();
    const childrenOf = (pid: string) =>
      Object.values(tm.nodes).filter((n) => n.parentId === pid);
    for (const cid of collapsedRef.current) {
      const stack = [...childrenOf(cid)];
      while (stack.length) {
        const cur = stack.pop()!;
        hidden.add(cur.id);
        stack.push(...childrenOf(cur.id));
      }
    }
    const vis = (id: string) => tm.nodes[id] && !hidden.has(id);
    for (const n of Object.values(tm.nodes)) {
      if (n.id !== tm.rootId && n.parentId && tm.nodes[n.parentId] && vis(n.id) && vis(n.parentId)) {
        out.push({
          id: `s-${n.parentId}-${n.id}`,
          source: n.parentId,
          target: n.id,
          style: { stroke: "#4a5265", strokeDasharray: "6 4", strokeWidth: 1.5 },
        });
      }
    }
    return out;
  }

  // ---------- 持久化 ----------
  function persist(next: TaskMapData) {
    setTaskMap(next);
    if (activeId) {
      // 内存同步即时（agent 工具读取最新）
      taskmapSyncMemory(activeId, next).catch(console.error);
      // DB 落盘防抖（拖拽/连续输入时合并）
      if (saveTimerRef.current) clearTimeout(saveTimerRef.current);
      saveTimerRef.current = setTimeout(() => {
        taskmapSave(activeId, next).catch(console.error);
      }, 400);
    }
  }

  // ---------- React Flow 事件 ----------
  // 仅保留选择/尺寸等非拖拽变化；节点拖拽已禁用（任务图完全依靠 AI 生成与整理布局）
  const onNodesChange: OnNodesChange = useCallback((changes) => {
    setNodes((nds) => applyNodeChanges(changes, nds));
  }, []);

  /**
   * 唯一布局规则（导图形态）：
   * - 支持多条一级任务线：root（需求聚合）+ 独立一级任务（parentId === ""）作为顶层区块，
   *   从上到下竖排，每条任务线内部再逐层向右展开（x = 层级深度）
   * - 同一父节点的子任务按依赖拓扑分层（wave）后一律竖排：
   *   · 有依赖关系的（串行链）：先执行的 wave 小、靠上，后执行的 wave 大、靠下 → 纵向排列
   *   · 无依赖的同级任务：按创建顺序从上到下依次排列（当前无子代理，不横向并排）
   * - 递归为每棵子树分配垂直区间，保证不重叠
   */
  function computeLayoutPos(tm: TaskMapData): Record<string, [number, number]> {
    const X_GAP = 300; // 层级间距
    const Y_UNIT = 96; // 垂直单位
    const TOP_GAP = 72; // 一级任务线之间的垂直间距
    const childrenOf = (pid: string) =>
      Object.values(tm.nodes)
        .filter((n) => n.parentId === pid)
        .sort((a, b) => a.created - b.created);

    // 兄弟间拓扑分层：只看同父兄弟间的依赖；成环时按 0 处理防死循环
    const waveOf = (kids: TaskNode[]): Map<string, number> => {
      const kidIds = new Set(kids.map((k) => k.id));
      const wave = new Map<string, number>();
      const visiting = new Set<string>();
      const calc = (id: string): number => {
        const cached = wave.get(id);
        if (cached !== undefined) return cached;
        if (visiting.has(id)) return 0;
        visiting.add(id);
        const n = tm.nodes[id];
        let w = 0;
        if (n) {
          for (const d of n.deps) {
            if (kidIds.has(d)) w = Math.max(w, calc(d) + 1);
          }
        }
        visiting.delete(id);
        wave.set(id, w);
        return w;
      };
      for (const k of kids) calc(k.id);
      return wave;
    };

    // 同级排序：先按 wave（先执行靠上），同 wave 按创建顺序
    const sortSiblings = (kids: TaskNode[], wv: Map<string, number>): TaskNode[] =>
      [...kids].sort((a, b) => {
        const wa = wv.get(a.id) ?? 0;
        const wb = wv.get(b.id) ?? 0;
        if (wa !== wb) return wa - wb;
        return a.created - b.created;
      });

    // 子树垂直高度：叶子 = Y_UNIT；内部 = 各子任务高度之和（同级一律竖排）
    const height: Record<string, number> = {};
    const calcHeight = (id: string): number => {
      const cached = height[id];
      if (cached !== undefined) return cached;
      const kids = childrenOf(id);
      if (kids.length === 0) return Y_UNIT;
      const wv = waveOf(kids);
      const sorted = sortSiblings(kids, wv);
      height[id] = sorted.reduce((s, k) => s + (calcHeight(k.id) || Y_UNIT), 0);
      return height[id];
    };
    calcHeight(tm.rootId);

    // 顶层区块：root（需求聚合）+ 独立一级任务（parentId === ""，多一级任务线）
    const topRoots = [
      tm.rootId,
      ...Object.values(tm.nodes)
        .filter((n) => n.parentId === "" && n.id !== tm.rootId)
        .sort((a, b) => a.created - b.created)
        .map((n) => n.id),
    ];

    // 递归布局：节点在 [top, bottom] 区间垂直居中；子节点在右侧全部竖排
    const pos: Record<string, [number, number]> = {};
    const assign = (id: string, x: number, top: number, bottom: number) => {
      pos[id] = [x, (top + bottom) / 2];
      const kids = childrenOf(id);
      if (kids.length === 0) return;
      const wv = waveOf(kids);
      const sorted = sortSiblings(kids, wv);
      let cursor = top;
      for (const k of sorted) {
        const h = height[k.id] || Y_UNIT;
        assign(k.id, x + X_GAP, cursor, cursor + h);
        cursor += h;
      }
    };
    let cursor = 0;
    for (const rid of topRoots) {
      const h = height[rid] || Y_UNIT;
      assign(rid, 0, cursor, cursor + h);
      cursor += h + TOP_GAP;
    }
    return pos;
  }

  // ---------- 自动整理布局 ----------
  /** 按导图规则重排所有节点位置并持久化（AI 规划新任务后界面变乱时使用） */
  function autoLayout() {
    const tm = taskMapRef.current;
    if (!tm) return;
    const pos = computeLayoutPos(tm);
    const next: TaskMapData = { ...tm, nodes: { ...tm.nodes } };
    for (const [id, p] of Object.entries(pos)) {
      next.nodes[id] = { ...next.nodes[id], pos: p };
    }
    persist(next);
    applyMap(next);
  }

  /** 工具栏「⇲ 整理」按钮 */
  const onLayoutClick = () => {
    autoLayout();
    flash("⇲ 已按导图规则自动整理布局");
  };

  // ---------- 任务设计对话框 ----------
  /** 发送任务设计内容 */
  const submitDesign = async () => {
    const text = designInput.trim();
    if (!text || designState === "planning") return;
    setDesignInput("");
    const tm = taskMapRef.current;
    // ★ 包装为任务设计/创建指令：让 AI 明确这是高优先级的任务规划需求（不是普通对话）
    const wrapped = tm
      ? `【任务图对话框·任务设计指令】请把以下内容作为高优先级的任务调整需求处理：先 plan_review 审视当前任务图，再按指令调整任务结构（增删/层级/顺序），调整完成后说明改动并停止，等待用户确认，不要开始执行任务。\n\n用户要求：${text}`
      : `【任务图对话框·任务创建指令】请把以下内容作为高优先级的任务创建需求处理：规划任务图（一级=目标、二级=步骤、三级=细分），创建完成后说明计划并停止，等待用户确认，不要开始执行任务。\n\n用户需求：${text}`;
    // ★ 无论创建还是调整，都走「规划 → 确认 → 执行」流程（创建任务也要弹窗同意）
    designCreateRef.current = !tm;
    snapshotRef.current = tm ? JSON.parse(JSON.stringify(tm)) : null;
    cancelledRef.current = false;
    setDesignState("planning");
    await onInterrupt();
    await onSendPlan(wrapped, true);
    if (cancelledRef.current) return; // 规划期间被用户打断，已回滚
    if (authMode === "none") {
      // ★ 全自动模式：不弹确认，自动同意并继续执行
      snapshotRef.current = null;
      designCreateRef.current = false;
      setDesignState("idle");
      await load();
      await onResume();
      return;
    }
    setDesignState("awaiting-confirm");
    await load();
  };

  /** 回滚到发送调整内容前的任务图快照 */
  const restoreSnapshot = () => {
    const snap = snapshotRef.current;
    snapshotRef.current = null;
    if (!snap) return;
    persist(snap);
    applyMap(snap);
  };

  /** 同意：保留 AI 调整，继续进程 */
  const agreeDesign = async () => {
    snapshotRef.current = null;
    designCreateRef.current = false;
    setDesignState("idle");
    flash("✅ 已同意调整，继续执行");
    await onResume();
  };

  /** 不同意：回滚任务图，等待用户重新输入（不开始） */
  const rejectDesign = () => {
    restoreSnapshot();
    designCreateRef.current = false;
    setDesignState("idle");
    flash("↩ 已回滚任务图，可继续输入调整内容");
  };

  // ---------- 三个控制按钮 ----------
  /** 打断：执行中停止进程；规划中取消规划并回滚 */
  const onInterruptClick = () => {
    if (designState === "planning") {
      cancelledRef.current = true;
      restoreSnapshot();
      setDesignState("idle");
      flash("⏹ 已打断任务调整，任务图已回滚");
    }
    onInterrupt();
  };

  /** 继续：等待确认时未同意 → 回滚按原进程继续；空闲时直接恢复 */
  const onContinueClick = async () => {
    if (designState === "awaiting-confirm" && snapshotRef.current) {
      restoreSnapshot();
      flash("↩ 未同意调整，已按原任务图继续");
    }
    setDesignState("idle");
    await onResume();
  };

  /** 刷新：重新加载任务图状态 */
  const onRefreshClick = async () => {
    await load();
    flash("⟳ 已刷新任务图");
  };

  const handleDesignKey = (e: React.KeyboardEvent) => {
    if (e.key !== "Enter") return;
    // 输入法组词中按 Enter 是选候选词：打标记，本次及后续确认候选的 Enter 都不发送
    if (e.nativeEvent.isComposing || composingRef.current) {
      enterDuringCompositionRef.current = true;
      return;
    }
    if (enterDuringCompositionRef.current) return;
    if (Date.now() - lastCompositionEndRef.current < 300) {
      enterDuringCompositionRef.current = true;
      return;
    }
    if (!e.shiftKey) {
      e.preventDefault();
      submitDesign();
    }
  };
  const handleDesignKeyUp = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && enterDuringCompositionRef.current) {
      enterDuringCompositionRef.current = false;
    }
  };

  // ---------- 渲染 ----------
  const selectedNode = selectedId && taskMap ? taskMap.nodes[selectedId] : null;
  const stats = taskMap
    ? (() => {
        const vals = Object.values(taskMap.nodes);
        const total = vals.length;
        const done = vals.filter((n) => n.status === "done").length;
        const ip = vals.filter((n) => n.status === "in_progress").length;
        const blk = vals.filter((n) => n.status === "blocked").length;
        const avg = total > 0 ? Math.round(vals.reduce((s, n) => s + n.progress, 0) / total) : 0;
        return { total, done, ip, blk, avg, rate: total > 0 ? Math.round((done / total) * 100) : 0 };
      })()
    : null;

  return (
    <div className="graph-view">
      {/* 工具栏：仅控制按钮 */}
      <div className="graph-toolbar">
        <div className="graph-actions">
          <button
            className="ctrl-btn interrupt"
            onClick={onInterruptClick}
            disabled={!streaming && designState !== "planning"}
            title="打断当前进程"
          >
            ⏹ 打断
          </button>
          <button
            className="ctrl-btn resume"
            onClick={onContinueClick}
            disabled={streaming || designState === "planning"}
            title="继续当前进程"
          >
            ▶ 继续
          </button>
          <button
            className="ctrl-btn refresh"
            onClick={onRefreshClick}
            disabled={designState === "planning"}
            title="刷新任务图状态"
          >
            ⟳ 刷新
          </button>
          <button
            className="ctrl-btn layout"
            onClick={onLayoutClick}
            disabled={designState === "planning"}
            title="按导图规则自动整理布局"
          >
            ⇲ 整理
          </button>
        </div>
      </div>

      {/* 操作提示条 */}
      {notice && <div className="graph-notice">{notice}</div>}

      {/* 进度总览条 */}
      {taskMap && stats && (
        <div className="graph-overview">
          <div className="ov-bar" title="平均进度">
            <div className="ov-bar-fill" style={{ width: `${stats.avg}%` }} />
          </div>
          <span className="ov-text">完成率 <b>{stats.rate}%</b>（{stats.done}/{stats.total}）</span>
          <span className="ov-text dim">平均进度 {stats.avg}%</span>
          <span className="ov-text ip">🔵 进行中 {stats.ip}</span>
          <span className="ov-text blk">⛔ 阻塞 {stats.blk}</span>
          <span className="ov-text root">◎ 根聚合 {computeAggProgress(taskMap)}%</span>
        </div>
      )}

      {/* 当前执行中的任务横幅（实时执行状态） */}
      {taskMap && (() => {
        const running = Object.values(taskMap.nodes).filter((n) => n.status === "in_progress");
        if (running.length === 0) return null;
        return (
          <div className="graph-running">
            <span className="graph-running-dot" />
            <span className="graph-running-label">AI 正在执行：</span>
            {running.map((n) => (
              <button
                key={n.id}
                className="graph-running-item"
                onClick={() => setSelectedId(n.id)}
                title={`点击定位节点 ${n.id}`}
              >
                {n.title}
              </button>
            ))}
            <span className="graph-running-hint">（任务完成后自动更新）</span>
          </div>
        );
      })()}

      {!taskMap ? (
        <div className="graph-empty">
          <p>还没有任务图。</p>
          <p>在下方输入任务需求，AI 将创建任务图并开始执行。</p>
        </div>
      ) : (
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          onNodesChange={onNodesChange}
          onNodeClick={(_, node) => setSelectedId(node.id)}
          onPaneClick={() => setSelectedId(null)}
          deleteKeyCode={null}
          edgesReconnectable={false}
          nodesConnectable={false}
          nodesDraggable={false}
          fitView
        >
          <Background />
          <Controls />
          <MiniMap />
        </ReactFlow>
      )}

      {/* 选中节点详情面板（只读） */}
      {selectedNode && taskMap && (
        <div className="node-panel">
          <div className="node-panel-head">
            <span className="node-panel-title">任务详情</span>
            <button className="node-panel-close" onClick={() => setSelectedId(null)}>✕</button>
          </div>
          <div className="node-panel-body">
            <div className="np-field">
              <span className="np-label">标题</span>
              <div className="np-static">{selectedNode.title}</div>
            </div>
            <div className="np-field">
              <span className="np-label">说明</span>
              <div className="np-static">{selectedNode.detail || "—"}</div>
            </div>
            <div className="np-field">
              <span className="np-label">状态</span>
              <div className="np-static">
                <span className="np-status-icon" style={{ color: STATUS_META[selectedNode.status]?.color }}>
                  {STATUS_META[selectedNode.status]?.icon}
                </span>{" "}
                {STATUS_META[selectedNode.status]?.label || selectedNode.status}
              </div>
            </div>
            <div className="np-field">
              <span className="np-label">进度：{Math.round(selectedNode.progress)}%</span>
              <div className="np-progress-row">
                <div className="np-progress-bar">
                  <div
                    className="np-progress-fill"
                    style={{
                      width: `${selectedNode.progress}%`,
                      background: STATUS_META[selectedNode.status]?.color || "#4f8cff",
                    }}
                  />
                </div>
              </div>
            </div>
            <div className="np-field">
              <span className="np-label">执行顺序·前序（{selectedNode.deps.length}）</span>
              {selectedNode.deps.length === 0 ? (
                <div className="np-dim">无前序，可立即执行</div>
              ) : (
                <div className="np-dep-list">
                  {selectedNode.deps
                    .filter((d) => taskMap.nodes[d])
                    .map((d) => {
                      const dep = taskMap.nodes[d];
                      return (
                        <div key={d} className="np-dep-item">
                          <span className="np-dep-icon">{STATUS_META[dep.status]?.icon}</span>
                          <span className="np-dep-title">{dep.title}</span>
                        </div>
                      );
                    })}
                </div>
              )}
              <div className="np-dim">同级任务按图从上到下依次执行</div>
            </div>
            <div className="np-meta">
              ID: {selectedNode.id} ｜ 父: {selectedNode.parentId ? taskMap.nodes[selectedNode.parentId]?.title || selectedNode.parentId : "—"}
              {selectedNode.startedAt && (
                <div className="np-meta-line">▶ 开始：{new Date(selectedNode.startedAt).toLocaleString()}</div>
              )}
              {selectedNode.finishedAt && (
                <div className="np-meta-line">✅ 完成：{new Date(selectedNode.finishedAt).toLocaleString()}</div>
              )}
            </div>
          </div>
        </div>
      )}

      {/* 任务设计对话框（底部） */}
      <div className="design-bar">
        {designState === "awaiting-confirm" && (
          <div className="design-confirm">
            <span className="design-confirm-text">
              {designCreateRef.current
                ? "AI 已根据你的需求创建任务图，是否同意？"
                : "AI 已根据你的内容调整任务图，是否同意？"}
            </span>
            <div className="design-confirm-actions">
              <button className="btn-primary" onClick={agreeDesign}>✓ 同意并继续</button>
              <button className="btn-ghost" onClick={rejectDesign}>✗ 不同意，重新调整</button>
            </div>
          </div>
        )}
        {designState === "planning" && (
          <div className="design-status">⏳ AI 正在根据你的内容调整任务图…（可点「打断」取消）</div>
        )}
        <div className="design-input-row">
          <textarea
            className="design-input"
            value={designInput}
            placeholder={
              taskMap
                ? "输入任务调整内容"
                : "输入任务需求"
            }
            onChange={(e) => setDesignInput(e.target.value)}
            onKeyDown={handleDesignKey}
            onKeyUp={handleDesignKeyUp}
            onCompositionStart={() => (composingRef.current = true)}
            onCompositionEnd={() => {
              composingRef.current = false;
              lastCompositionEndRef.current = Date.now();
            }}
            rows={2}
            disabled={designState === "planning"}
          />
          <button
            className="send-btn"
            onClick={submitDesign}
            disabled={designState === "planning" || !designInput.trim()}
            title="发送任务设计内容"
          >
            <Send size={16} />
          </button>
        </div>
      </div>
    </div>
  );
}
