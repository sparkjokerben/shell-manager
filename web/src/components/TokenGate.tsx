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
    if (!token) {
      setError("Enter the token shown for this smw instance.");
      return;
    }
    onSubmit(token);
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
          The token is kept in this browser only and sent as an Authorization header.
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
