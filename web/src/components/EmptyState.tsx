interface EmptyStateProps {
  kind: "loading" | "idle" | "empty" | "error" | "token";
  title: string;
  body: string;
  detail?: string | null;
  onNew: () => void;
  onShowSessions: () => void;
  onRetry: () => void;
  onEnterToken: () => void;
}

export function EmptyState({
  kind,
  title,
  body,
  detail,
  onNew,
  onShowSessions,
  onRetry,
  onEnterToken,
}: EmptyStateProps) {
  return (
    <section className="stage-message" aria-live="polite">
      <div className="stage-message-content">
        <div className="stage-rule" aria-hidden="true" />
        <h1>{title}</h1>
        <p className="stage-body">{body}</p>
        {detail ? <p className="stage-detail">{detail}</p> : null}

        {kind === "loading" ? (
          <p className="stage-wait">Reading the session list...</p>
        ) : (
          <div className="stage-actions">
            {kind === "error" ? (
              <button className="button button-primary" type="button" onClick={onRetry}>
                Retry connection
              </button>
            ) : kind === "token" ? (
              <button className="button button-primary" type="button" onClick={onEnterToken}>
                Enter token
              </button>
            ) : (
              <button className="button button-primary" type="button" onClick={onNew}>
                New session
              </button>
            )}

            {kind === "idle" ? (
              <button className="button button-quiet" type="button" onClick={onShowSessions}>
                Show sessions
              </button>
            ) : null}
          </div>
        )}

        <div className="stage-behavior">
          <div className="behavior-row">
            <span className="behavior-key">Detach</span>
            <span>Closes this browser connection, even when you close the tab. The shell keeps running.</span>
          </div>
          <div className="behavior-row">
            <span className="behavior-key is-danger">Stop</span>
            <span>Destroys the shell and every program running inside it.</span>
          </div>
        </div>
      </div>
    </section>
  );
}
