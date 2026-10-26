export type ConnectionPhase = "connecting" | "connected" | "disconnected" | "error";

export interface ConnectionSnapshot {
  phase: ConnectionPhase;
  detail: string;
}

export const IDLE_CONNECTION: ConnectionSnapshot = {
  phase: "disconnected",
  detail: "未附着。",
};

export type ToastTone = "info" | "error";

export interface ToastState {
  message: string;
  tone: ToastTone;
}

export interface NewSessionDraft {
  name: string;
  wsl: boolean;
}
