export interface Health {
  ok: true;
  instance: string;
  version: string;
  has_token: boolean;
}

export interface Session {
  name: string;
  attached: boolean;
}

export interface SessionList {
  sessions: Session[];
}

export type WsServerMessage =
  | { type: "ready"; name: string; mouse: number; instance: string }
  | { type: "exit"; message: string }
  | { type: "error"; message: string };

export type WsClientMessage =
  | { type: "resize"; cols: number; rows: number }
  | { type: "detach" };

const TOKEN_STORAGE_KEY = "smw.token";

export class ApiError extends Error {
  readonly status: number;

  constructor(message: string, status = 0) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

export function loadToken(): string | null {
  try {
    return window.localStorage.getItem(TOKEN_STORAGE_KEY);
  } catch {
    return null;
  }
}

export function saveToken(token: string): void {
  try {
    window.localStorage.setItem(TOKEN_STORAGE_KEY, token);
  } catch {
    // Storage can be unavailable in privacy modes; the in-memory token still works.
  }
}

export function forgetToken(): void {
  try {
    window.localStorage.removeItem(TOKEN_STORAGE_KEY);
  } catch {
    // Nothing else to do.
  }
}

function baseDirectory(): URL {
  return new URL(".", document.baseURI);
}

function apiUrl(path: string): URL {
  return new URL(path, baseDirectory());
}

interface RequestOptions {
  method?: "GET" | "POST" | "DELETE";
  body?: unknown;
  token?: string | null;
  signal?: AbortSignal;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function statusMessage(status: number): string {
  switch (status) {
    case 400:
      return "smw 拒绝了该请求。请检查会话名称后重试。";
    case 401:
      return "访问令牌缺失或已被拒绝。";
    case 403:
      return "此令牌无权执行该操作。";
    case 404:
      return "该会话已不存在。";
    case 409:
      return "该会话已附着到另一个客户端。";
    case 429:
      return "请求过于频繁，请稍候重试。";
    case 500:
    case 502:
    case 503:
    case 504:
      return "smw 在处理请求时出现问题，请重试。";
    default:
      return `smw 返回了 HTTP ${status}。`;
  }
}

const SERVER_ERROR_PATTERNS: ReadonlyArray<readonly [RegExp, string]> = [
  [
    /failed to connect to control pipe|is smd running/i,
    "无法连接 smd，请确认守护进程正在运行",
  ],
  [/output backlog exceeded/i, "输出积压过多，已断开；shell 仍在运行"],
  [/input backlog exceeded/i, "输入积压过多，已断开；shell 仍在运行"],
  [/output read failed|session input failed/i, "读取/写入终端失败"],
  [/has exited/i, "会话已退出"],
  [/already exists/i, "同名会话已存在"],
  [/session ended/i, "会话已结束"],
  [/access denied/i, "访问被拒绝"],
  [/timed out|timeout/i, "操作超时"],
  [/not found/i, "会话不存在"],
];

/**
 * Translate a message produced by smd/smw before it reaches the UI.
 * Messages already in Chinese and unknown English messages pass through unchanged.
 */
export function translateServerError(message: string): string {
  for (const [pattern, translation] of SERVER_ERROR_PATTERNS) {
    if (pattern.test(message)) {
      return translation;
    }
  }
  return message;
}

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const headers = new Headers({ Accept: "application/json" });
  if (options.token) {
    headers.set("Authorization", `Bearer ${options.token}`);
  }
  if (options.body !== undefined) {
    headers.set("Content-Type", "application/json");
  }

  let response: Response;
  try {
    response = await fetch(apiUrl(path), {
      method: options.method ?? "GET",
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: options.signal,
    });
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") {
      throw error;
    }
    throw new ApiError(
      "无法访问 smw。请确认守护进程正在运行，且页面地址正确。",
      0,
    );
  }

  let text = "";
  try {
    text = await response.text();
  } catch {
    if (response.ok) {
      throw new ApiError("smw 在发送响应前关闭了连接。", response.status);
    }
  }

  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text) as unknown;
    } catch {
      if (response.ok) {
        throw new ApiError("smw 返回了浏览器无法读取的响应。", response.status);
      }
    }
  }

  if (!response.ok) {
    const serverMessage =
      isRecord(payload) && typeof payload.error === "string" ? payload.error : null;
    throw new ApiError(
      serverMessage ? translateServerError(serverMessage) : statusMessage(response.status),
      response.status,
    );
  }

  return payload as T;
}

export function getHealth(token: string | null, signal?: AbortSignal): Promise<Health> {
  return request<Health>("api/health", { token, signal });
}

export function listSessions(token: string | null, signal?: AbortSignal): Promise<SessionList> {
  return request<SessionList>("api/sessions", { token, signal });
}

export interface CreateSessionInput {
  name: string;
  wsl: boolean;
}

export async function createSession(
  token: string | null,
  input: CreateSessionInput,
): Promise<void> {
  await request<{ ok: true }>("api/sessions", {
    method: "POST",
    token,
    body: input.wsl ? { name: input.name, wsl: true } : { name: input.name },
  });
}

export async function detachSession(token: string | null, name: string): Promise<void> {
  await request<{ ok: true }>(`api/sessions/${encodeURIComponent(name)}/detach`, {
    method: "POST",
    token,
  });
}

export async function stopSession(token: string | null, name: string): Promise<void> {
  await request<{ ok: true }>(`api/sessions/${encodeURIComponent(name)}`, {
    method: "DELETE",
    token,
  });
}

function clampDimension(value: number): number {
  if (!Number.isFinite(value)) {
    return 80;
  }
  return Math.min(1000, Math.max(1, Math.floor(value)));
}

export function buildWebSocketUrl(
  name: string,
  cols: number,
  rows: number,
  token: string | null,
  takeover: boolean,
): string {
  const url = new URL(`ws/${encodeURIComponent(name)}`, baseDirectory());
  url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";

  const params = new URLSearchParams();
  params.set("cols", String(clampDimension(cols)));
  params.set("rows", String(clampDimension(rows)));
  if (takeover) {
    params.set("takeover", "1");
  }
  if (token) {
    params.set("token", token);
  }
  url.search = params.toString();
  return url.toString();
}

export function isUnauthorized(error: unknown): error is ApiError {
  return error instanceof ApiError && error.status === 401;
}

export function errorMessage(error: unknown): string {
  if (error instanceof ApiError) {
    return error.message;
  }
  if (error instanceof Error && error.message) {
    return error.message;
  }
  return "出了点问题，请重试。";
}
