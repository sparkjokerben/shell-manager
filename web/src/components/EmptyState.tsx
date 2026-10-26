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
          <p className="stage-wait">正在读取会话列表…</p>
        ) : (
          <div className="stage-actions">
            {kind === "error" ? (
              <button className="button button-primary" type="button" onClick={onRetry}>
                重试连接
              </button>
            ) : kind === "token" ? (
              <button className="button button-primary" type="button" onClick={onEnterToken}>
                输入令牌
              </button>
            ) : (
              <button className="button button-primary" type="button" onClick={onNew}>
                新建会话
              </button>
            )}

            {kind === "idle" ? (
              <button className="button button-quiet" type="button" onClick={onShowSessions}>
                显示会话
              </button>
            ) : null}
          </div>
        )}

        <div className="stage-behavior">
          <div className="behavior-row">
            <span className="behavior-key">断开连接</span>
            <span>关闭此浏览器连接，包括关闭标签页时。shell 仍会继续运行。</span>
          </div>
          <div className="behavior-row">
            <span className="behavior-key is-danger">停止并销毁</span>
            <span>销毁 shell 及其内部运行的所有程序。</span>
          </div>
        </div>
      </div>
    </section>
  );
}
