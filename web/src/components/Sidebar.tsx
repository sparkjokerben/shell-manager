import type { Session } from "../api";
import type { ConnectionSnapshot } from "../types";
import { CloseIcon, LiveTrace, PlusIcon, RefreshIcon } from "./Icons";

interface SidebarProps {
  sessions: Session[];
  selectedName: string | null;
  connection: ConnectionSnapshot;
  loading: boolean;
  refreshing: boolean;
  error: string | null;
  hasToken: boolean;
  tokenRequired: boolean;
  tokenKnown: boolean;
  open: boolean;
  onClose: () => void;
  onRefresh: () => void;
  onAttach: (name: string) => void;
  onNew: () => void;
  onStop: (session: Session) => void;
  onForceDisconnect: (session: Session) => void;
  onEnterToken: () => void;
  onClearToken: () => void;
}

function sessionState(session: Session, selectedName: string | null, connection: ConnectionSnapshot) {
  const isSelected = session.name === selectedName;
  if (isSelected) {
    if (connection.phase === "connected") {
      return { label: "已附着", live: true, ours: true };
    }
    if (connection.phase === "connecting") {
      return { label: "附着中", live: true, ours: true };
    }
    if (connection.phase === "error") {
      return { label: "附着失败", live: false, ours: true };
    }
    return { label: "已断开", live: false, ours: true };
  }
  if (session.attached) {
    return { label: "其他客户端已附着", live: true, ours: false };
  }
  return { label: "空闲", live: false, ours: false };
}

export function Sidebar({
  sessions,
  selectedName,
  connection,
  loading,
  refreshing,
  error,
  hasToken,
  tokenRequired,
  tokenKnown,
  open,
  onClose,
  onRefresh,
  onAttach,
  onNew,
  onStop,
  onForceDisconnect,
  onEnterToken,
  onClearToken,
}: SidebarProps) {
  const sessionCount = sessions.length;
  const countLabel = sessionCount === 1 ? "1 个会话" : `${sessionCount} 个会话`;

  return (
    <aside
      id="session-drawer"
      className={open ? "sidebar is-open" : "sidebar"}
      aria-label="会话"
    >
      <div className="sidebar-head">
        <div>
          <h2>会话</h2>
          <p className="sidebar-count">{loading ? "正在读取守护进程…" : countLabel}</p>
        </div>
        <button className="icon-button sidebar-close" type="button" onClick={onClose} aria-label="关闭会话">
          <CloseIcon />
        </button>
      </div>

      <button className="button button-new" type="button" onClick={onNew}>
        <PlusIcon className="button-icon" />
        新建会话
      </button>

      <div className="list-tools">
        <button
          className={refreshing ? "text-button is-busy" : "text-button"}
          type="button"
          onClick={onRefresh}
          disabled={refreshing}
        >
          <RefreshIcon className="button-icon" />
          {refreshing ? "刷新中" : "刷新"}
        </button>
        <span className="poll-note">每 2 秒更新</span>
      </div>

      {error ? (
        <div className="inline-error" role="alert">
          <p>{error}</p>
          <button className="text-button" type="button" onClick={onRefresh}>
            重试
          </button>
        </div>
      ) : null}

      <div className="session-scroll">
        {sessions.length === 0 ? (
          !loading && !error && !tokenRequired ? (
            <div className="sidebar-empty">
              <p>还没有会话。</p>
              <p>使用“新建会话”启动 PowerShell 或 WSL。</p>
            </div>
          ) : null
        ) : (
          <ul className="session-list">
            {sessions.map((session) => {
              const state = sessionState(session, selectedName, connection);
              const selected = session.name === selectedName;
              return (
                <li className={selected ? "session-row is-selected" : "session-row"} key={session.name}>
                  <button
                    className="session-pick"
                    type="button"
                    onClick={() => onAttach(session.name)}
                    aria-current={selected ? "page" : undefined}
                  >
                    <span className="session-name">{session.name}</span>
                    <span className="session-state">
                      <span className={state.live ? "state-word is-live" : "state-word"}>{state.label}</span>
                      {state.live ? <LiveTrace active={selected && connection.phase === "connected"} /> : null}
                    </span>
                  </button>
                  <div className="session-actions">
                    {session.attached || (state.ours && connection.phase === "connected") ? (
                      <button
                        className="mini-button"
                        type="button"
                        onClick={() => onForceDisconnect(session)}
                        aria-label={`强制断开附着到 ${session.name} 的客户端`}
                        title="断开已附着的浏览器客户端。shell 仍会继续运行。"
                      >
                        强制断开
                      </button>
                    ) : null}
                    <button
                      className="mini-button is-danger"
                      type="button"
                      onClick={() => onStop(session)}
                      aria-label={`停止并销毁 ${session.name} 及其 shell`}
                      title="销毁 shell 及其正在运行的程序。"
                    >
                      停止并销毁
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </div>

      <div className="sidebar-foot">
        <div className="token-state">
          <span className="token-label">访问令牌</span>
          <span className="token-copy">
            {hasToken
              ? "已保存在此浏览器"
              : !tokenKnown
                ? "正在检查此实例…"
                : tokenRequired
                  ? "此实例要求提供"
                  : "无需提供"}
          </span>
        </div>
        {hasToken ? (
          <div className="token-actions">
            <button className="text-button" type="button" onClick={onEnterToken}>
              更换令牌
            </button>
            <button className="text-button" type="button" onClick={onClearToken}>
              清除凭据
            </button>
          </div>
        ) : tokenRequired ? (
          <button className="text-button" type="button" onClick={onEnterToken}>
            输入令牌
          </button>
        ) : null}
      </div>
    </aside>
  );
}
