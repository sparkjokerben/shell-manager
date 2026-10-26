import { Dialog } from "./Dialog";

interface ConfirmDialogProps {
  title: string;
  description: string;
  confirmLabel: string;
  tone?: "danger" | "caution";
  busy: boolean;
  error: string | null;
  onCancel: () => void;
  onConfirm: () => void;
}

export function ConfirmDialog({
  title,
  description,
  confirmLabel,
  tone = "danger",
  busy,
  error,
  onCancel,
  onConfirm,
}: ConfirmDialogProps) {
  return (
    <Dialog
      titleId="confirm-dialog-title"
      title={title}
      description={description}
      onClose={busy ? undefined : onCancel}
      className={`dialog-confirm ${tone === "danger" ? "is-danger" : "is-caution"}`}
    >
      {error ? (
        <p className="form-error" role="alert">
          {error}
        </p>
      ) : null}
      <div className="dialog-actions">
        <button className="button button-quiet" type="button" onClick={onCancel} disabled={busy} data-autofocus>
          Cancel
        </button>
        <button
          className={`button ${tone === "danger" ? "button-danger" : "button-caution"}`}
          type="button"
          onClick={onConfirm}
          disabled={busy}
        >
          {busy ? "Working..." : confirmLabel}
        </button>
      </div>
    </Dialog>
  );
}
