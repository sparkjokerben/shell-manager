import { useCallback, useEffect, useRef, useState } from "react";
import {
  createSession,
  detachSession,
  errorMessage,
  forgetToken,
  getHealth,
  isUnauthorized,
  listSessions,
  loadToken,
  saveToken,
  stopSession,
  type Health,
  type Session,
} from "./api";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { EmptyState } from "./components/EmptyState";
import { BridgeMark, CloseIcon, MenuIcon } from "./components/Icons";
import { NewSession } from "./components/NewSession";
import { Sidebar } from "./components/Sidebar";
import { TerminalPane } from "./components/TerminalPane";
import { TokenGate } from "./components/TokenGate";
import {
  IDLE_CONNECTION,
  type ConnectionSnapshot,
  type NewSessionDraft,
  type ToastState,
} from "./types";
import { useViewportHeight } from "./useViewportHeight";

interface TokenPrompt {
  reason: string;
  canCancel: boolean;
}

function tokenErrorMessage(error: unknown): string {
  if (isUnauthorized(error)) {
    return "访问令牌缺失或已被拒绝。";
  }
  return errorMessage(error);
}

export default function App() {
  useViewportHeight();

  const [token, setToken] = useState<string | null>(() => loadToken());
  const [health, setHealth] = useState<Health | null>(null);
  const [healthError, setHealthError] = useState<string | null>(null);
  const [healthLoading, setHealthLoading] = useState(true);

  const [sessions, setSessions] = useState<Session[]>([]);
  const [sessionsLoaded, setSessionsLoaded] = useState(false);
  const [sessionsLoading, setSessionsLoading] = useState(true);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const [selectedName, setSelectedName] = useState<string | null>(null);
  const [connection, setConnection] = useState<ConnectionSnapshot>(IDLE_CONNECTION);
  const [attachSignal, setAttachSignal] = useState(0);

  const [drawerOpen, setDrawerOpen] = useState(false);
  const [newOpen, setNewOpen] = useState(false);
  const [stopTarget, setStopTarget] = useState<Session | null>(null);
  const [forceTarget, setForceTarget] = useState<Session | null>(null);
  const [actionBusy, setActionBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  const [tokenGate, setTokenGate] = useState<TokenPrompt | null>(null);
  const [authBlocked, setAuthBlocked] = useState(false);
  const [toast, setToast] = useState<ToastState | null>(null);

  const healthSeqRef = useRef(0);
  const sessionsSeqRef = useRef(0);
  const selectedNameRef = useRef<string | null>(null);
  const connectionRef = useRef<ConnectionSnapshot>(IDLE_CONNECTION);

  useEffect(() => {
    selectedNameRef.current = selectedName;
  }, [selectedName]);

  const promptToken = useCallback((reason: string, canCancel = true) => {
    setAuthBlocked(true);
    setTokenGate((current) => current ?? { reason, canCancel });
  }, []);

  const handleConnectionChange = useCallback((snapshot: ConnectionSnapshot) => {
    connectionRef.current = snapshot;
    setConnection(snapshot);
  }, []);

  const loadHealth = useCallback(async () => {
    const seq = healthSeqRef.current + 1;
    healthSeqRef.current = seq;
    setHealthLoading(true);
    setHealthError(null);

    try {
      const result = await getHealth(token);
      if (healthSeqRef.current !== seq) {
        return;
      }
      setHealth(result);
      if (result.has_token && !token) {
        promptToken("此 smw 实例需要访问令牌。输入令牌以管理会话。");
      }
    } catch (caught) {
      if (healthSeqRef.current !== seq) {
        return;
      }
      if (isUnauthorized(caught)) {
        promptToken("此 smw 实例需要访问令牌。");
      }
      setHealthError(tokenErrorMessage(caught));
    } finally {
      if (healthSeqRef.current === seq) {
        setHealthLoading(false);
      }
    }
  }, [promptToken, token]);

  useEffect(() => {
    void loadHealth();
  }, [loadHealth]);

  const fetchSessions = useCallback(
    async (mode: "initial" | "poll" | "manual") => {
      const seq = sessionsSeqRef.current + 1;
      sessionsSeqRef.current = seq;
      if (mode === "initial") {
        setSessionsLoading(true);
      }
      if (mode === "manual") {
        setRefreshing(true);
      }

      try {
        const result = await listSessions(token);
        if (sessionsSeqRef.current !== seq) {
          return;
        }
        setSessions(result.sessions);
        setSessionsLoaded(true);
        setSessionsError(null);
      } catch (caught) {
        if (sessionsSeqRef.current !== seq) {
          return;
        }
        if (isUnauthorized(caught)) {
          promptToken(
            token
              ? "已保存的访问令牌被拒绝，请重新输入。"
              : "此 smw 实例需要访问令牌。",
          );
        }
        setSessionsError(tokenErrorMessage(caught));
      } finally {
        if (sessionsSeqRef.current === seq) {
          setSessionsLoading(false);
        }
        if (mode === "manual") {
          setRefreshing(false);
        }
      }
    },
    [promptToken, token],
  );

  const needsToken = authBlocked || (health?.has_token === true && !token);
  const pollBlocked = needsToken || tokenGate !== null;

  useEffect(() => {
    if (pollBlocked) {
      setSessionsLoading(false);
      return;
    }

    void fetchSessions("initial");

    const timer = window.setInterval(() => {
      if (!document.hidden) {
        void fetchSessions("poll");
      }
    }, 2000);

    const handleVisibility = () => {
      if (!document.hidden) {
        void fetchSessions("poll");
      }
    };

    document.addEventListener("visibilitychange", handleVisibility);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", handleVisibility);
      sessionsSeqRef.current += 1;
    };
  }, [fetchSessions, pollBlocked]);

  useEffect(() => {
    if (!selectedName || !sessionsLoaded || sessionsError) {
      return;
    }
    if (sessions.some((session) => session.name === selectedName)) {
      return;
    }
    selectedNameRef.current = null;
    setSelectedName(null);
    connectionRef.current = IDLE_CONNECTION;
    setConnection(IDLE_CONNECTION);
  }, [selectedName, sessions, sessionsError, sessionsLoaded]);

  useEffect(() => {
    const media = window.matchMedia("(max-width: 900px)");
    const handleChange = () => {
      if (!media.matches) {
        setDrawerOpen(false);
      }
    };
    media.addEventListener("change", handleChange);
    return () => media.removeEventListener("change", handleChange);
  }, []);

  useEffect(() => {
    if (!toast) {
      return;
    }
    const timer = window.setTimeout(
      () => setToast(null),
      toast.tone === "error" ? 7000 : 4500,
    );
    return () => window.clearTimeout(timer);
  }, [toast]);

  const selectedSession =
    selectedName === null
      ? null
      : sessions.find((session) => session.name === selectedName) ?? null;

  const attach = useCallback((name: string) => {
    setDrawerOpen(false);
    const current = connectionRef.current;
    if (
      name === selectedNameRef.current &&
      (current.phase === "connected" || current.phase === "connecting")
    ) {
      return;
    }
    selectedNameRef.current = name;
    setSelectedName(name);
    const next: ConnectionSnapshot = {
      phase: "connecting",
      detail: `正在附着到 ${name}…`,
    };
    connectionRef.current = next;
    setConnection(next);
    setAttachSignal((value) => value + 1);
  }, []);

  const showSessions = useCallback(() => {
    if (window.matchMedia("(max-width: 900px)").matches) {
      setDrawerOpen(true);
      return;
    }
    document.querySelector<HTMLElement>(".session-pick")?.focus();
  }, []);

  const closeDrawer = useCallback(() => {
    setDrawerOpen(false);
    window.requestAnimationFrame(() => {
      document.querySelector<HTMLElement>(".drawer-toggle")?.focus();
    });
  }, []);

  useEffect(() => {
    if (!drawerOpen) {
      return;
    }

    const frame = window.requestAnimationFrame(() => {
      const drawer = document.querySelector<HTMLElement>(".sidebar.is-open");
      const target =
        drawer?.querySelector<HTMLElement>(".session-pick") ??
        drawer?.querySelector<HTMLElement>(".sidebar-close");
      target?.focus();
    });

    const handleKeyDown = (event: KeyboardEvent) => {
      if (document.querySelector(".dialog")) {
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        closeDrawer();
        return;
      }
      if (event.key !== "Tab") {
        return;
      }
      const drawer = document.querySelector<HTMLElement>(".sidebar.is-open");
      if (!drawer) {
        return;
      }
      const items = Array.from(
        drawer.querySelectorAll<HTMLElement>(
          'button:not([disabled]), input:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
        ),
      );
      if (items.length === 0) {
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    return () => {
      window.cancelAnimationFrame(frame);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [closeDrawer, drawerOpen]);

  const refreshNow = () => {
    void loadHealth();
    void fetchSessions("manual");
  };

  const handleCreate = async (draft: NewSessionDraft) => {
    try {
      await createSession(token, draft);
    } catch (caught) {
      if (isUnauthorized(caught)) {
        promptToken("创建会话前请输入有效的令牌。");
      }
      throw caught;
    }

    setNewOpen(false);
    closeDrawer();
    setSessions((current) =>
      current.some((session) => session.name === draft.name)
        ? current
        : [...current, { name: draft.name, attached: false }],
    );
    selectedNameRef.current = draft.name;
    setSelectedName(draft.name);
    const next: ConnectionSnapshot = {
      phase: "connecting",
      detail: `正在附着到 ${draft.name}…`,
    };
    connectionRef.current = next;
    setConnection(next);
    setAttachSignal((value) => value + 1);
    setToast({ message: `已启动 ${draft.name}。`, tone: "info" });
    void fetchSessions("manual");
  };

  const requestStop = (session: Session) => {
    setActionError(null);
    setStopTarget(session);
  };

  const confirmStop = async () => {
    const target = stopTarget;
    if (!target) {
      return;
    }
    setActionBusy(true);
    setActionError(null);
    try {
      await stopSession(token, target.name);
      setStopTarget(null);
      setSessions((current) => current.filter((session) => session.name !== target.name));
      if (selectedNameRef.current === target.name) {
        selectedNameRef.current = null;
        setSelectedName(null);
        connectionRef.current = IDLE_CONNECTION;
        setConnection(IDLE_CONNECTION);
      }
      setToast({
        message: `已停止 ${target.name}，其 shell 和正在运行的程序已一并销毁。`,
        tone: "info",
      });
      void fetchSessions("manual");
    } catch (caught) {
      if (isUnauthorized(caught)) {
        promptToken("停止会话前请输入有效的令牌。");
      }
      setActionError(tokenErrorMessage(caught));
    } finally {
      setActionBusy(false);
    }
  };

  const requestForce = (session: Session) => {
    setActionError(null);
    setForceTarget(session);
  };

  const confirmForce = async () => {
    const target = forceTarget;
    if (!target) {
      return;
    }
    setActionBusy(true);
    setActionError(null);
    try {
      await detachSession(token, target.name);
      setForceTarget(null);
      setToast({
        message: `已断开附着到 ${target.name} 的客户端，shell 仍在运行。`,
        tone: "info",
      });
      void fetchSessions("manual");
    } catch (caught) {
      if (isUnauthorized(caught)) {
        promptToken("断开客户端前请输入有效的令牌。");
      }
      setActionError(tokenErrorMessage(caught));
    } finally {
      setActionBusy(false);
    }
  };

  const handleTokenSubmit = (value: string) => {
    const trimmed = value.trim();
    if (!trimmed) {
      return;
    }
    saveToken(trimmed);
    setToken(trimmed);
    setTokenGate(null);
    setAuthBlocked(false);
    setSessionsError(null);
    setHealthError(null);
    setToast({ message: "令牌已保存在此浏览器中。", tone: "info" });
  };

  const clearCredentials = () => {
    forgetToken();
    setToken(null);
    selectedNameRef.current = null;
    setSelectedName(null);
    connectionRef.current = IDLE_CONNECTION;
    setConnection(IDLE_CONNECTION);
    setSessions([]);
    setSessionsLoaded(false);
    setSessionsError(null);
    if (health?.has_token || authBlocked) {
      setAuthBlocked(true);
      setTokenGate({
        reason: "凭据已清除。请输入令牌以重新连接此 smw 实例。",
        canCancel: true,
      });
    } else {
      setAuthBlocked(false);
      setTokenGate(null);
    }
    setToast({ message: "已清除保存的令牌。", tone: "info" });
  };

  const tokenRequired = health?.has_token === true;
  const hasToken = token !== null && token.length > 0;
  const healthOffline = healthError !== null;
  const bridgeState = healthOffline ? "offline" : healthLoading && !health ? "checking" : "ready";

  let stage = null;
  if (selectedSession) {
    stage = (
      <TerminalPane
        key={selectedSession.name}
        session={selectedSession}
        instance={health?.instance ?? "未知"}
        token={token}
        attachSignal={attachSignal}
        onConnectionChange={handleConnectionChange}
      />
    );
  } else if (needsToken) {
    stage = (
      <EmptyState
        kind="token"
        title="需要访问令牌。"
        body="此 smw 实例使用令牌保护其 API。输入令牌即可列出并附着会话。"
        detail={sessionsError ?? healthError}
        onNew={() => setNewOpen(true)}
        onShowSessions={showSessions}
        onRetry={refreshNow}
        onEnterToken={() =>
          setTokenGate({
            reason: "输入此 smw 实例的访问令牌。",
            canCancel: true,
          })
        }
      />
    );
  } else if (sessionsLoading && sessions.length === 0) {
    stage = (
      <EmptyState
        kind="loading"
        title="正在打开控制台。"
        body="正在从 smw 读取会话列表。"
        onNew={() => setNewOpen(true)}
        onShowSessions={showSessions}
        onRetry={refreshNow}
        onEnterToken={() =>
          setTokenGate({ reason: "输入此 smw 实例的访问令牌。", canCancel: true })
        }
      />
    );
  } else if (sessions.length === 0 && (sessionsError || healthError)) {
    stage = (
      <EmptyState
        kind="error"
        title="smw 无响应。"
        body="无法加载会话列表。请确认 smd 和 smw 正在运行，然后重试。"
        detail={sessionsError ?? healthError}
        onNew={() => setNewOpen(true)}
        onShowSessions={showSessions}
        onRetry={refreshNow}
        onEnterToken={() =>
          setTokenGate({ reason: "输入此 smw 实例的访问令牌。", canCancel: true })
        }
      />
    );
  } else if (sessions.length === 0) {
    stage = (
      <EmptyState
        kind="empty"
        title="还没有会话。"
        body="启动一个 PowerShell 或 WSL 会话。关闭此页面后它仍会继续运行。"
        onNew={() => setNewOpen(true)}
        onShowSessions={showSessions}
        onRetry={refreshNow}
        onEnterToken={() =>
          setTokenGate({ reason: "输入此 smw 实例的访问令牌。", canCancel: true })
        }
      />
    );
  } else {
    stage = (
      <EmptyState
        kind="idle"
        title="未选择会话。"
        body="从侧栏选择一个会话，或新建会话。断开连接不会停止它。"
        onNew={() => setNewOpen(true)}
        onShowSessions={showSessions}
        onRetry={refreshNow}
        onEnterToken={() =>
          setTokenGate({ reason: "输入此 smw 实例的访问令牌。", canCancel: true })
        }
      />
    );
  }

  return (
    <div className="app">
      <header className="masthead">
        <button
          className="drawer-toggle"
          type="button"
          onClick={() => setDrawerOpen(true)}
          aria-expanded={drawerOpen}
          aria-controls="session-drawer"
          aria-label="显示会话"
        >
          <MenuIcon />
          <span>会话</span>
        </button>

        <div className="brand">
          <BridgeMark className="brand-mark" />
          <div className="brand-lockup">
            <span className="brand-name">smw</span>
            <span className="brand-sub">面向 Windows 的持久 shell</span>
          </div>
        </div>

        <div className="masthead-meta">
          <div className="bridge-readout" data-state={bridgeState} title={healthError ?? undefined}>
            <span className="signal-lamp" aria-hidden="true" />
            <span className="readout">
              <span className="readout-label">实例</span>
              <span className="readout-value">
                {health?.instance ?? (healthLoading ? "检查中" : "不可用")}
              </span>
            </span>
          </div>
          {health ? <span className="version-readout">v{health.version}</span> : null}
        </div>
      </header>

      <div className="workspace">
        {drawerOpen ? (
          <button
            className="drawer-scrim"
            type="button"
            aria-label="关闭会话"
            onClick={closeDrawer}
          />
        ) : null}

        <Sidebar
          sessions={sessions}
          selectedName={selectedName}
          connection={connection}
          loading={sessionsLoading && sessions.length === 0}
          refreshing={refreshing}
          error={sessionsError}
          hasToken={hasToken}
          tokenRequired={tokenRequired || authBlocked}
          tokenKnown={health !== null}
          open={drawerOpen}
          onClose={closeDrawer}
          onRefresh={refreshNow}
          onAttach={attach}
          onNew={() => setNewOpen(true)}
          onStop={requestStop}
          onForceDisconnect={requestForce}
          onEnterToken={() =>
            setTokenGate({
              reason: "输入此 smw 实例的访问令牌。",
              canCancel: true,
            })
          }
          onClearToken={clearCredentials}
        />

        <main className="stage">{stage}</main>
      </div>

      {newOpen ? (
        <NewSession onClose={() => setNewOpen(false)} onCreate={handleCreate} />
      ) : null}

      {stopTarget ? (
        <ConfirmDialog
          title={`停止并销毁 ${stopTarget.name}？`}
          description="这会销毁 shell 及其正在运行的所有程序。若只想关闭浏览器连接，请使用“断开连接”。"
          confirmLabel="停止并销毁"
          busy={actionBusy}
          error={actionError}
          onCancel={() => {
            if (!actionBusy) {
              setStopTarget(null);
              setActionError(null);
            }
          }}
          onConfirm={() => void confirmStop()}
        />
      ) : null}

      {forceTarget ? (
        <ConfirmDialog
          title={`断开附着到 ${forceTarget.name} 的客户端？`}
          description="当前附着的浏览器客户端将被断开。shell 及其程序仍会继续运行，你可以稍后再次附着。"
          confirmLabel="强制断开"
          tone="caution"
          busy={actionBusy}
          error={actionError}
          onCancel={() => {
            if (!actionBusy) {
              setForceTarget(null);
              setActionError(null);
            }
          }}
          onConfirm={() => void confirmForce()}
        />
      ) : null}

      {tokenGate ? (
        <TokenGate
          reason={tokenGate.reason}
          canCancel={tokenGate.canCancel}
          onCancel={() => setTokenGate(null)}
          onSubmit={handleTokenSubmit}
        />
      ) : null}

      {toast ? (
        <div
          className={`toast is-${toast.tone}`}
          role={toast.tone === "error" ? "alert" : "status"}
          aria-live="polite"
        >
          <span className="toast-mark" aria-hidden="true" />
          <span className="toast-message">{toast.message}</span>
          <button
            className="icon-button toast-close"
            type="button"
            onClick={() => setToast(null)}
            aria-label="关闭消息"
          >
            <CloseIcon />
          </button>
        </div>
      ) : null}
    </div>
  );
}
