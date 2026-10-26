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
      setError("Enter the token shown for this smw instance.");
      return;
    }
    if (/[\u0000-\u001f\u007f]/.test(trimmed)) {
      setError("The token contains invalid control characters.");
      return;
    }
    onSubmit(trimmed);
  };

  return (
    <Dialog
      titleId="token-gate-title"
      title="Access token"
      description={reason}
      onClose={canCancel ? onCancel : undefined}
      className="dialog-token"
    >
      <form onSubmit={handleSubmit} noValidate>
        <label className="field-label" htmlFor="smw-token">
          Token
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
          Kept in this browser only. API requests use an Authorization header; the terminal
          connection passes it as a query parameter, so avoid logging that URL.
        </p>
        {error ? (
          <p className="form-error" role="alert">
            {error}
          </p>
        ) : null}
        <div className="dialog-actions">
          {canCancel ? (
            <button className="button button-quiet" type="button" onClick={onCancel}>
              Cancel
            </button>
          ) : null}
          <button className="button button-primary" type="submit">
            Save token
          </button>
        </div>
      </form>
    </Dialog>
  );
}
