import { useEffect, useRef, type ReactNode } from "react";
import { CloseIcon } from "./Icons";

interface DialogProps {
  titleId: string;
  title: string;
  description?: string;
  children: ReactNode;
  onClose?: () => void;
  className?: string;
}

function focusableElements(root: HTMLElement): HTMLElement[] {
  return Array.from(
    root.querySelectorAll<HTMLElement>(
      'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
    ),
  ).filter((element) => !element.hasAttribute("hidden"));
}

export function Dialog({
  titleId,
  title,
  description,
  children,
  onClose,
  className = "",
}: DialogProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const onCloseRef = useRef(onClose);

  // App callbacks are often inline, so keep the latest one in a ref. The focus
  // setup below must not re-run (and steal focus) on every parent render.
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const panel = panelRef.current;
    if (!panel) {
      return;
    }

    const initial =
      panel.querySelector<HTMLElement>("[data-autofocus]") ??
      focusableElements(panel)[0] ??
      panel;
    initial.focus();

    const handleKeyDown = (event: KeyboardEvent) => {
      if (!panel.contains(document.activeElement)) {
        return;
      }
      if (event.key === "Escape" && onCloseRef.current) {
        event.preventDefault();
        onCloseRef.current();
        return;
      }
      if (event.key !== "Tab") {
        return;
      }

      const items = focusableElements(panel);
      if (items.length === 0) {
        event.preventDefault();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      previous?.focus();
    };
  }, []);

  return (
    <div className="dialog-backdrop">
      <div
        ref={panelRef}
        className={`dialog ${className}`.trim()}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={description ? `${titleId}-description` : undefined}
        tabIndex={-1}
      >
        <div className="dialog-rule" aria-hidden="true" />
        <div className="dialog-heading">
          <h2 id={titleId}>{title}</h2>
          {onClose ? (
            <button className="icon-button" type="button" onClick={onClose} aria-label="关闭">
              <CloseIcon />
            </button>
          ) : null}
        </div>
        {description ? (
          <p className="dialog-description" id={`${titleId}-description`}>
            {description}
          </p>
        ) : null}
        {children}
      </div>
    </div>
  );
}
