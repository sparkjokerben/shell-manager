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
      return "smw rejected the request. Check the session name and try again.";
    case 401:
      return "The access token is missing or was rejected.";
    case 403:
      return "This token is not allowed to perform that action.";
    case 404:
      return "That session no longer exists.";
    case 409:
      return "The session is already attached to another client.";
    case 429:
      return "Too many requests. Wait a moment, then retry.";
    case 500:
    case 502:
    case 503:
    case 504:
      return "smw had a problem completing the request. Try again.";
    default:
      return `smw returned HTTP ${status}.`;
  }
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
      "Cannot reach smw. Check that the daemon is running and the page address is correct.",
      0,
    );
  }

  let text = "";
  try {
    text = await response.text();
  } catch {
    if (response.ok) {
      throw new ApiError("smw closed the connection before sending a response.", response.status);
    }
  }

  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text) as unknown;
    } catch {
      if (response.ok) {
        throw new ApiError("smw returned a response the browser could not read.", response.status);
      }
    }
  }

  if (!response.ok) {
    const serverMessage =
      isRecord(payload) && typeof payload.error === "string" ? payload.error : null;
    throw new ApiError(serverMessage || statusMessage(response.status), response.status);
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
  return Math.min(65535, Math.max(1, Math.floor(value)));
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
  return "Something went wrong. Try again.";
}
