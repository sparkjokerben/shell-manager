import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type PointerEvent as ReactPointerEvent,
} from "react";

interface MobileKeysProps {
  /** 与 terminal.onData 走同一条发送路径（含 ready 前缓冲）。 */
  onSend: (data: string) => void;
  /** 让 xterm 的隐藏 textarea 保持焦点，避免软键盘收起。 */
  onFocusTerminal: () => void;
}

interface KeySpec {
  label: string;
  ariaLabel: string;
  data: string;
}

const KEYS: KeySpec[] = [
  { label: "Esc", ariaLabel: "Esc", data: "\x1b" },
  { label: "Tab", ariaLabel: "Tab", data: "\t" },
  { label: "↑", ariaLabel: "向上", data: "\x1b[A" },
  { label: "↓", ariaLabel: "向下", data: "\x1b[B" },
  { label: "←", ariaLabel: "向左", data: "\x1b[D" },
  { label: "→", ariaLabel: "向右", data: "\x1b[C" },
  { label: "Ctrl+C", ariaLabel: "Ctrl+C 中断", data: "\x03" },
  { label: "Ctrl+D", ariaLabel: "Ctrl+D 结束输入", data: "\x04" },
];

const HINT_DURATION = 5000;

export function MobileKeys({ onSend, onFocusTerminal }: MobileKeysProps) {
  const [hint, setHint] = useState<string | null>(null);
  const hintTimerRef = useRef<number | null>(null);
  // 同一次点击可能依次触发 pointerdown/touchend/click，
  // 用这个开关保证剪贴板只读取、粘贴一次。
  const pasteGuardRef = useRef(false);

  useEffect(
    () => () => {
      if (hintTimerRef.current !== null) {
        window.clearTimeout(hintTimerRef.current);
      }
    },
    [],
  );

  const showHint = useCallback((message: string) => {
    setHint(message);
    if (hintTimerRef.current !== null) {
      window.clearTimeout(hintTimerRef.current);
    }
    hintTimerRef.current = window.setTimeout(() => {
      hintTimerRef.current = null;
      setHint(null);
    }, HINT_DURATION);
  }, []);

  const pressKey = (event: ReactPointerEvent<HTMLButtonElement>, data: string) => {
    // preventDefault 阻止按钮抢走焦点，否则点一下软键盘就会收起。
    event.preventDefault();
    onSend(data);
    onFocusTerminal();
  };

  const pasteFromClipboard = async () => {
    if (pasteGuardRef.current) {
      return;
    }
    pasteGuardRef.current = true;

    const clipboard: Clipboard | undefined = navigator.clipboard;
    if (!clipboard || typeof clipboard.readText !== "function") {
      // HTTP 局域网访问时 navigator.clipboard 通常不存在。
      pasteGuardRef.current = false;
      showHint("浏览器未开放剪贴板读取（HTTP 访问时常见）。请长按终端手动粘贴。");
      return;
    }
    try {
      const text = await clipboard.readText();
      if (!text) {
        showHint("剪贴板为空。");
        return;
      }
      setHint(null);
      // 终端里换行应发送回车（\r），与 xterm 自身的粘贴行为保持一致。
      onSend(text.replace(/\r\n?|\n/g, "\r"));
    } catch {
      // 某些浏览器只认可 touchend/click 上的用户手势，
      // 放开开关让同一手势里的后续事件再试一次。
      pasteGuardRef.current = false;
      showHint("无法读取剪贴板，请长按终端手动粘贴。");
    }
  };

  const handlePastePointerDown = (event: ReactPointerEvent<HTMLButtonElement>) => {
    // 同样要 preventDefault 保持终端焦点，否则软键盘会收起。
    event.preventDefault();
    pasteGuardRef.current = false;
    void pasteFromClipboard();
    onFocusTerminal();
  };

  return (
    <div className="mobile-keys">
      {hint ? (
        <p className="mobile-keys-note" role="status">
          {hint}
        </p>
      ) : null}
      <div className="mobile-keys-scroll" role="toolbar" aria-label="终端快捷键">
        {KEYS.map((key) => (
          <button
            key={key.label}
            className="mobile-key"
            type="button"
            tabIndex={-1}
            aria-label={key.ariaLabel}
            onPointerDown={(event) => pressKey(event, key.data)}
          >
            {key.label}
          </button>
        ))}
        <button
          className="mobile-key mobile-key-wide"
          type="button"
          tabIndex={-1}
          aria-label="粘贴剪贴板内容"
          onPointerDown={handlePastePointerDown}
          onTouchEnd={() => void pasteFromClipboard()}
          onClick={() => void pasteFromClipboard()}
        >
          粘贴
        </button>
      </div>
    </div>
  );
}
