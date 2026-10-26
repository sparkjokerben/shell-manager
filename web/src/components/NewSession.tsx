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
    return "请输入会话名称。";
  }
  if (raw.trim() !== raw) {
    return "请去掉名称首尾的空格。";
  }
  if (Array.from(raw).length > 64) {
    return "名称不得超过 64 个字符。";
  }
  if (/[\u0000-\u001f\u007f]/.test(raw)) {
    return "名称不能包含控制字符。";
  }
  if (/[\\/]/.test(raw)) {
    return "名称不能包含斜杠。";
  }
  if (raw === "." || raw === "..") {
    return "名称不能为 . 或 ..";
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
      title="新建会话"
      description="默认使用 PowerShell。WSL 会运行你的默认 Linux 发行版。"
      onClose={busy ? undefined : onClose}
      className="dialog-new"
    >
      <form onSubmit={(event) => void handleSubmit(event)} noValidate>
        <label className="field-label" htmlFor="new-session-name">
          名称
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
        <p className="field-hint">最多 64 个字符。不能包含控制字符、斜杠或首尾空格。</p>

        <label className="switch-row" htmlFor="new-session-wsl">
          <span className="switch-copy">
            <span className="switch-title">在 WSL 中运行</span>
            <span className="switch-note">启动你的默认 Linux 发行版，而不是 PowerShell。</span>
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
            取消
          </button>
          <button className="button button-primary" type="submit" disabled={busy}>
            {busy ? "正在启动会话…" : "启动会话"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}
