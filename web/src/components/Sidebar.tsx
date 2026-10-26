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
      return { label: "attached now", live: true, ours: true };
    }
    if (connection.phase === "connecting") {
      return { label: "attaching", live: true, ours: true };
    }
    if (connection.phase === "error") {
      return { label: "attach failed", live: false, ours: true };
    }
    return { label: "detached", live: false, ours: true };
  }
  if (session.attached) {
    return { label: "attached elsewhere", live: true, ours: false };
  }
  return { label: "idle", live: false, ours: false };
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
  const countLabel = sessionCount === 1 ? "1 session" : `${sessionCount} sessions`;

  return (
    <aside
      id="session-drawer"
      className={open ? "sidebar is-open" : "sidebar"}
      aria-label="Sessions"
    >
      <div className="sidebar-head">
        <div>
          <h2>Sessions</h2>
          <p className="sidebar-count">{loading ? "Reading the daemon..." : countLabel}</p>
        </div>
        <button className="icon-button sidebar-close" type="button" onClick={onClose} aria-label="Close sessions">
          <CloseIcon />
        </button>
      </div>

      <button className="button button-new" type="button" onClick={onNew}>
        <PlusIcon className="button-icon" />
        New session
      </button>

      <div className="list-tools">
        <button
          className={refreshing ? "text-button is-busy" : "text-button"}
          type="button"
          onClick={onRefresh}
          disabled={refreshing}
        >
          <RefreshIcon className="button-icon" />
          {refreshing ? "Refreshing" : "Refresh"}
        </button>
        <span className="poll-note">updates every 2s</span>
      </div>

      {error ? (
        <div className="inline-error" role="alert">
          <p>{error}</p>
          <button className="text-button" type="button" onClick={onRefresh}>
            Retry
          </button>
        </div>
      ) : null}

      <div className="session-scroll">
        {sessions.length === 0 ? (
          !loading && !error && !tokenRequired ? (
            <div className="sidebar-empty">
              <p>No sessions yet.</p>
              <p>Use New session to start PowerShell or WSL.</p>
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
                        aria-label={`Force disconnect the client attached to ${session.name}`}
                        title="Disconnect the attached browser client. The shell keeps running."
                      >
                        Force
                      </button>
                    ) : null}
                    <button
                      className="mini-button is-danger"
                      type="button"
                      onClick={() => onStop(session)}
                      aria-label={`Stop ${session.name} and destroy its shell`}
                      title="Destroy the shell and its running programs."
                    >
                      Stop
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
          <span className="token-label">Access token</span>
          <span className="token-copy">
            {hasToken
              ? "Stored in this browser"
              : !tokenKnown
                ? "Checking this instance…"
                : tokenRequired
                  ? "Required by this instance"
                  : "Not required"}
          </span>
        </div>
        {hasToken ? (
          <div className="token-actions">
            <button className="text-button" type="button" onClick={onEnterToken}>
              Replace token
            </button>
            <button className="text-button" type="button" onClick={onClearToken}>
              Clear credentials
            </button>
          </div>
        ) : tokenRequired ? (
          <button className="text-button" type="button" onClick={onEnterToken}>
            Enter token
          </button>
        ) : null}
      </div>
    </aside>
  );
}
