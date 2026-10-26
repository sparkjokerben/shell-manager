import { useState, type FormEvent } from "react";
import { Dialog } from "./Dialog";

interface TokenGateProps {
  reason: string;
  canCancel: boolean;
  onCancel: () => void;
  onSubmit: (token: string) => void;
}

export function TokenGate({ reason, canCancel, onCancel, onSubmit }: TokenGateProps) {
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);

  const handleSubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const trimmed = token.trim();
    if (!trimmed) {
      setError("请输入此 smw 实例显示的令牌。");
      return;
    }
    if (/[\u0000-\u001f\u007f]/.test(trimmed)) {
      setError("令牌包含无效的控制字符。");
      return;
    }
    onSubmit(trimmed);
  };

  return (
    <Dialog
      titleId="token-gate-title"
      title="访问令牌"
      description={reason}
      onClose={canCancel ? onCancel : undefined}
      className="dialog-token"
    >
      <form onSubmit={handleSubmit} noValidate>
        <label className="field-label" htmlFor="smw-token">
          令牌
        </label>
        <input
          id="smw-token"
          className="text-input"
          type="password"
          value={token}
          onChange={(event) => {
            setToken(event.target.value);
            setError(null);
          }}
          autoComplete="off"
          spellCheck={false}
          data-autofocus
        />
        <p className="field-hint">
          仅保存在此浏览器中。API 请求通过 Authorization 头传递令牌；终端连接则将其作为查询参数传递，因此请勿记录该 URL。
        </p>
        {error ? (
          <p className="form-error" role="alert">
            {error}
          </p>
        ) : null}
        <div className="dialog-actions">
          {canCancel ? (
            <button className="button button-quiet" type="button" onClick={onCancel}>
              取消
            </button>
          ) : null}
          <button className="button button-primary" type="submit">
            保存令牌
          </button>
        </div>
      </form>
    </Dialog>
  );
}
