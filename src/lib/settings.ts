// 设置持久化（骨架阶段用 localStorage；后续迁移到 Rust 端加密存储）
import type { ProviderConfig } from "../types";

const KEY = "canlow-provider";

export const DEFAULT_PROVIDER: ProviderConfig = {
  baseUrl: "https://api.deepseek.com",
  apiKey: "",
  model: "deepseek-chat",
};

export function loadProvider(): ProviderConfig {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return { ...DEFAULT_PROVIDER };
    const parsed = JSON.parse(raw);
    return { ...DEFAULT_PROVIDER, ...parsed };
  } catch {
    return { ...DEFAULT_PROVIDER };
  }
}

export function saveProvider(p: ProviderConfig): void {
  localStorage.setItem(KEY, JSON.stringify(p));
}
