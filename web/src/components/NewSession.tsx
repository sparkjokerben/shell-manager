import { useState, type FormEvent } from "react";
import { errorMessage } from "../api";
import type { NewSessionDraft } from "../types";
import { Dialog } from "./Dialog";

interface NewSessionProps {
  onClose: () => void;
  onCreate: (draft: NewSessionDraft) => Promise<void>;
}

function validateName(raw: string): string | null {
  if (raw.length === 0) {
    return "Enter a name for the session.";
  }
  if (raw.trim() !== raw) {
    return "Remove spaces from the start or end of the name.";
  }
  if (Array.from(raw).length > 64) {
    return "Keep the name to 64 characters or fewer.";
  }
  if (/[\u0000-\u001f\u007f]/.test(raw)) {
    return "Names cannot contain control characters.";
  }
  if (/[\\/]/.test(raw)) {
    return "Names cannot contain slashes.";
  }
  return null;
}

export function NewSession({ onClose, onCreate }: NewSessionProps) {
  const [name, setName] = useState("");
  const [wsl, setWsl] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const validation = validateName(name);
    if (validation) {
      setError(validation);
      return;
    }

    setBusy(true);
    setError(null);
    try {
      await onCreate({ name, wsl });
    } catch (caught) {
      setError(errorMessage(caught));
      setBusy(false);
    }
  };

  return (
    <Dialog
      titleId="new-session-title"
      title="New session"
      description="PowerShell is the default. WSL runs your default Linux distribution."
      onClose={busy ? undefined : onClose}
      className="dialog-new"
    >
      <form onSubmit={(event) => void handleSubmit(event)} noValidate>
        <label className="field-label" htmlFor="new-session-name">
          Name
        </label>
        <input
          id="new-session-name"
          className="text-input"
          value={name}
          onChange={(event) => {
            setName(event.target.value);
            setError(null);
          }}
          placeholder="build-main"
          autoComplete="off"
          spellCheck={false}
          data-autofocus
        />
        <p className="field-hint">Up to 64 characters. No control characters, slashes, or outer spaces.</p>

        <label className="switch-row" htmlFor="new-session-wsl">
          <span className="switch-copy">
            <span className="switch-title">Run in WSL</span>
            <span className="switch-note">Start your default Linux distribution instead of PowerShell.</span>
          </span>
          <input
            id="new-session-wsl"
            className="switch-input"
            type="checkbox"
            checked={wsl}
            onChange={(event) => setWsl(event.target.checked)}
          />
          <span className="switch-track" aria-hidden="true">
            <span className="switch-thumb" />
          </span>
        </label>

        {error ? (
          <p className="form-error" role="alert">
            {error}
          </p>
        ) : null}

        <div className="dialog-actions">
          <button className="button button-quiet" type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="button button-primary" type="submit" disabled={busy}>
            {busy ? "Starting session..." : "Start session"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}
