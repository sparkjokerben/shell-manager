import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  buildWebSocketUrl,
  translateServerError,
  type Session,
  type WsClientMessage,
  type WsServerMessage,
} from "../api";
import type { ConnectionPhase, ConnectionSnapshot } from "../types";

interface TerminalPaneProps {
  session: Session;
  instance: string;
  token: string | null;
  attachSignal: number;
  onConnectionChange: (snapshot: ConnectionSnapshot) => void;
}

interface Attempt {
  id: number;
  takeover: boolean;
}

const TERMINAL_THEME = {
  background: "#102023",
  foreground: "#ded8c8",
  cursor: "#e0b258",
  cursorAccent: "#102023",
  selectionBackground: "#35545a",
  black: "#16272a",
  red: "#cc715e",
  green: "#9ab584",
  yellow: "#d7b25f",
  blue: "#82aeb9",
  magenta: "#b48e9f",
  cyan: "#7fb9ad",
  white: "#ded8c8",
  brightBlack: "#607b77",
  brightRed: "#e08a74",
  brightGreen: "#b2cf98",
  brightYellow: "#eccd7d",
  brightBlue: "#9ec9d2",
  brightMagenta: "#cba8b7",
  brightCyan: "#99d2c7",
  brightWhite: "#f3edde",
};

const MOUSE_RESET = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

/**
 * Re-create the DEC mouse modes the hosted app had enabled at attach time.
 * The daemon reports them as a bitmask because a reattached client never sees
 * the original `?1000h`-style sequences.
 */
function mouseSequences(mouse: number): string {
  let sequence = MOUSE_RESET;
  if (mouse & 1) sequence += "\x1b[?1000h";
  if (mouse & 2) sequence += "\x1b[?1002h";
  if (mouse & 4) sequence += "\x1b[?1003h";
  if (mouse & 8) sequence += "\x1b[?1006h";
  return sequence;
}

function parseServerMessage(raw: string): WsServerMessage | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null || !("type" in parsed)) {
      return null;
    }
    const message = parsed as Record<string, unknown>;
    if (message.type === "ready") {
      if (
        typeof message.name === "string" &&
        typeof message.mouse === "number" &&
        typeof message.instance === "string"
      ) {
        return {
          type: "ready",
          name: message.name,
          mouse: message.mouse,
          instance: message.instance,
        };
      }
      return null;
    }
    if (message.type === "exit" && typeof message.message === "string") {
      return { type: "exit", message: message.message };
    }
    if (message.type === "error" && typeof message.message === "string") {
      return { type: "error", message: message.message };
    }
    return null;
  } catch {
    return null;
  }
}

function looksOccupied(message: string): boolean {
  return /409|占用|接管|另一个网页终端|occupied|already attached|in use|takeover|attached elsewhere/i.test(
    message,
  );
}

function phaseLabel(phase: ConnectionPhase): string {
  switch (phase) {
    case "connecting":
      return "连接中";
    case "connected":
      return "已连接";
    case "disconnected":
      return "已断开";
    case "error":
      return "错误";
  }
}

export function TerminalPane({
  session,
  instance,
  token,
  attachSignal,
  onConnectionChange,
}: TerminalPaneProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const socketRef = useRef<WebSocket | null>(null);
  const readyRef = useRef(false);
  const manualCloseRef = useRef(false);
  const connectionSeqRef = useRef(0);
  const lastEventRef = useRef<"exit" | "error" | null>(null);
  const resizeTimerRef = useRef<number | null>(null);
  const fitFrameRef = useRef<number | null>(null);
  const attachedRef = useRef(session.attached);
  const initialAttachSignalRef = useRef(attachSignal);
  const encoderRef = useRef(new TextEncoder());
  const pendingInputRef = useRef<string[]>([]);
  const onConnectionChangeRef = useRef(onConnectionChange);

  const [phase, setPhase] = useState<ConnectionPhase>("connecting");
  const [detail, setDetail] = useState(`正在附着到 ${session.name}…`);
  const [occupied, setOccupied] = useState(false);
  const [attempt, setAttempt] = useState<Attempt>({ id: 0, takeover: false });

  useEffect(() => {
    onConnectionChangeRef.current = onConnectionChange;
  }, [onConnectionChange]);

  useEffect(() => {
    onConnectionChangeRef.current({ phase, detail });
  }, [phase, detail]);

  useEffect(() => {
    attachedRef.current = session.attached;
  }, [session.attached]);

  useEffect(() => {
    if (attachSignal !== initialAttachSignalRef.current) {
      initialAttachSignalRef.current = attachSignal;
      setAttempt((current) => ({ id: current.id + 1, takeover: false }));
    }
  }, [attachSignal]);

  const sendMessage = useCallback((message: WsClientMessage) => {
    const socket = socketRef.current;
    if (socket?.readyState !== WebSocket.OPEN) {
      return;
    }
    socket.send(JSON.stringify(message));
  }, []);

  const scheduleResize = useCallback((cols: number, rows: number) => {
    if (resizeTimerRef.current !== null) {
      window.clearTimeout(resizeTimerRef.current);
    }
    resizeTimerRef.current = window.setTimeout(() => {
      resizeTimerRef.current = null;
      if (!readyRef.current) {
        return;
      }
      sendMessage({ type: "resize", cols, rows });
    }, 150);
  }, [sendMessage]);

  const fitTerminal = useCallback(() => {
    const terminal = terminalRef.current;
    const fit = fitRef.current;
    if (!terminal || !fit) {
      return;
    }
    try {
      fit.fit();
    } catch {
      // The host can be zero-sized during a drawer transition; the observer retries.
    }
  }, []);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) {
      return;
    }

    const terminal = new Terminal({
      scrollback: 5000,
      fontFamily: '"Cascadia Mono", "Cascadia Code", Consolas, "Courier New", monospace',
      fontSize: 13,
      lineHeight: 1.2,
      cursorBlink: true,
      cursorStyle: "bar",
      cursorWidth: 2,
      allowTransparency: false,
      theme: TERMINAL_THEME,
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(host);
    terminalRef.current = terminal;
    fitRef.current = fit;

    const dataDisposable = terminal.onData((data) => {
      const socket = socketRef.current;
      if (socket?.readyState !== WebSocket.OPEN) {
        return;
      }
      if (!readyRef.current) {
        // Hold a few characters typed while the handshake is in flight.
        const pending = pendingInputRef.current;
        const length = pending.reduce((total, part) => total + part.length, 0);
        if (length + data.length <= 4096) {
          pending.push(data);
        }
        return;
      }
      socket.send(encoderRef.current.encode(data));
    });

    // xterm reports X10 mouse encoding (and other binary input) through
    // onBinary, not onData. Without this subscription those reports vanish.
    const binaryDisposable = terminal.onBinary((data) => {
      const socket = socketRef.current;
      if (!readyRef.current || socket?.readyState !== WebSocket.OPEN) {
        return;
      }
      const bytes = new Uint8Array(data.length);
      for (let i = 0; i < data.length; i += 1) {
        bytes[i] = data.charCodeAt(i) & 0xff;
      }
      socket.send(bytes);
    });

    const resizeDisposable = terminal.onResize(({ cols, rows }) => {
      scheduleResize(cols, rows);
    });

    const resizeObserver = new ResizeObserver(() => {
      if (fitFrameRef.current !== null) {
        window.cancelAnimationFrame(fitFrameRef.current);
      }
      fitFrameRef.current = window.requestAnimationFrame(() => {
        fitFrameRef.current = null;
        fitTerminal();
      });
    });
    resizeObserver.observe(host);

    fitTerminal();
    terminal.focus();

    return () => {
      resizeObserver.disconnect();
      dataDisposable.dispose();
      binaryDisposable.dispose();
      resizeDisposable.dispose();
      if (resizeTimerRef.current !== null) {
        window.clearTimeout(resizeTimerRef.current);
      }
      if (fitFrameRef.current !== null) {
        window.cancelAnimationFrame(fitFrameRef.current);
      }
      terminal.dispose();
      terminalRef.current = null;
      fitRef.current = null;
    };
  }, [fitTerminal, scheduleResize]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }

    const seq = connectionSeqRef.current + 1;
    connectionSeqRef.current = seq;
    readyRef.current = false;
    manualCloseRef.current = false;
    lastEventRef.current = null;
    setOccupied(false);
    setPhase("connecting");
    setDetail(
      attempt.takeover
        ? `正在接管 ${session.name}；该处已附着的客户端将被断开。`
        : `正在附着到 ${session.name}…`,
    );

    fitTerminal();
    // Start each connection from a clean screen: the daemon repaints the
    // hosted app's current screen, and stale rows from the previous attach
    // would otherwise be duplicated.
    terminal.reset();

    const url = buildWebSocketUrl(
      session.name,
      terminal.cols,
      terminal.rows,
      token,
      attempt.takeover,
    );

    let socket: WebSocket;
    try {
      socket = new WebSocket(url);
    } catch {
      setPhase("error");
      setDetail("浏览器无法与 smw 建立 WebSocket 连接。");
      return;
    }

    socket.binaryType = "arraybuffer";
    socketRef.current = socket;

    const isCurrent = () => connectionSeqRef.current === seq;
    const readyTimer = window.setTimeout(() => {
      if (!isCurrent() || readyRef.current) {
        return;
      }
      lastEventRef.current = "error";
      setPhase("error");
      setDetail("连接在 15 秒内未就绪，smw 可能无法访问。");
      try {
        socket.close(1000, "ready timeout");
      } catch {
        // Already closing.
      }
    }, 15_000);

    socket.onopen = () => {
      if (!isCurrent()) {
        return;
      }
      terminal.focus();
    };

    socket.onmessage = (event) => {
      if (!isCurrent()) {
        return;
      }

      if (typeof event.data === "string") {
        const message = parseServerMessage(event.data);
        if (!message) {
          return;
        }
        if (message.type === "ready") {
          window.clearTimeout(readyTimer);
          readyRef.current = true;
          lastEventRef.current = null;
          setPhase("connected");
          setDetail(`已连接到 ${message.instance} 上的 ${message.name}。`);
          setOccupied(false);
          terminal.write(mouseSequences(message.mouse));
          const pending = pendingInputRef.current;
          pendingInputRef.current = [];
          if (pending.length > 0 && socket.readyState === WebSocket.OPEN) {
            socket.send(encoderRef.current.encode(pending.join("")));
          }
          sendMessage({ type: "resize", cols: terminal.cols, rows: terminal.rows });
          terminal.focus();
          return;
        }
        if (message.type === "exit") {
          window.clearTimeout(readyTimer);
          readyRef.current = false;
          lastEventRef.current = "exit";
          setPhase("disconnected");
          setDetail(translateServerError(message.message) || "shell 已退出。");
          return;
        }

        window.clearTimeout(readyTimer);
        readyRef.current = false;
        lastEventRef.current = "error";
        const busy = looksOccupied(message.message);
        const serverDetail = translateServerError(message.message);
        setOccupied(busy);
        setPhase("error");
        setDetail(
          busy
            ? "此 shell 已附着到另一个客户端。强制接管会断开该客户端；shell 仍会继续运行。"
            : serverDetail || "无法附着此会话。",
        );
        return;
      }

      if (event.data instanceof ArrayBuffer) {
        terminal.write(new Uint8Array(event.data));
      }
    };

    socket.onerror = () => {
      if (!isCurrent() || lastEventRef.current !== null) {
        return;
      }
      setDetail("WebSocket 握手失败。smw 可能无法访问，或此会话正在被使用。");
    };

    socket.onclose = (event) => {
      if (!isCurrent()) {
        return;
      }
      socketRef.current = null;
      readyRef.current = false;
      window.clearTimeout(readyTimer);
      if (manualCloseRef.current) {
        manualCloseRef.current = false;
        setPhase("disconnected");
        setDetail("已断开连接。shell 及其程序仍在运行。");
        return;
      }
      if (lastEventRef.current === "exit" || lastEventRef.current === "error") {
        return;
      }

      if (event.code === 1000 || event.code === 1005) {
        setPhase("disconnected");
        setDetail("桥接端已关闭连接。shell 仍在运行。");
        return;
      }

      setPhase("error");
      const attachedElsewhere = attachedRef.current;
      setOccupied(attachedElsewhere);
      if (event.code === 1006) {
        setDetail(
          attachedElsewhere
            ? "连接失败。此 shell 似乎已在别处附着；强制接管会断开该客户端。shell 仍在运行。"
            : "连接失败。令牌可能有误，smw 可能无法访问，或已有另一个客户端附着。",
        );
      } else {
        const reason = event.reason ? ` ${event.reason}` : "";
        setDetail(`连接已关闭，代码 ${event.code}。${reason}`);
      }
    };

    return () => {
      connectionSeqRef.current += 1;
      window.clearTimeout(readyTimer);
      pendingInputRef.current = [];
      const current = socketRef.current;
      socketRef.current = null;
      readyRef.current = false;
      if (current) {
        current.onopen = null;
        current.onmessage = null;
        current.onerror = null;
        current.onclose = null;
        if (current.readyState === WebSocket.OPEN) {
          try {
            current.send(JSON.stringify({ type: "detach" }));
          } catch {
            // The socket may already be closing.
          }
        }
        if (current.readyState === WebSocket.CONNECTING || current.readyState === WebSocket.OPEN) {
          current.close(1000, "client switched");
        }
      }
    };
  }, [attempt, fitTerminal, sendMessage, session.name, token]);

  const reconnect = (takeover: boolean) => {
    setAttempt((current) => ({ id: current.id + 1, takeover }));
  };

  const detach = () => {
    const socket = socketRef.current;
    if (!socket) {
      setPhase("disconnected");
      setDetail("未附着。shell 仍在运行。");
      return;
    }
    manualCloseRef.current = true;
    sendMessage({ type: "detach" });
    setPhase("disconnected");
    setDetail("已断开连接。shell 及其程序仍在运行。");
    socket.close(1000, "detach");
  };

  const showStrip = phase !== "connected";
  const canDetach = phase === "connected" || phase === "connecting";
  const showForce = occupied || (phase === "error" && session.attached);

  return (
    <section className="terminal-pane" aria-label={`${session.name} 的终端`}>
      <header className="terminal-head">
        <div className="terminal-title">
          <h1>{session.name}</h1>
          <span className="terminal-instance">实例 {instance}</span>
        </div>
        <div className="terminal-toolbar">
          <span className="connection-readout" data-phase={phase} aria-live="polite">
            <span className="signal-lamp" aria-hidden="true" />
            {phaseLabel(phase)}
          </span>
          <button className="text-button" type="button" onClick={() => reconnect(true)}>
            重连
          </button>
          <button
            className="text-button"
            type="button"
            onClick={detach}
            disabled={!canDetach}
            title="关闭此浏览器连接。shell 仍在运行。"
          >
            断开连接
          </button>
        </div>
      </header>

      <div className={`terminal-frame is-${phase}`}>
        {showStrip ? (
          <div className="link-strip" role="status">
            <div className="link-copy">
              <span className="link-state">{phaseLabel(phase)}</span>
              <p>{detail}</p>
            </div>
            <div className="link-actions">
              <button className="button button-quiet button-small" type="button" onClick={() => reconnect(true)}>
                {phase === "connecting" ? "重试" : "重连"}
              </button>
              {showForce ? (
                <button className="button button-caution button-small" type="button" onClick={() => reconnect(true)}>
                  强制接管
                </button>
              ) : null}
            </div>
          </div>
        ) : null}

        <div className="terminal-host" ref={hostRef} aria-label={`${session.name} 的交互式终端`} />

        <footer className="terminal-foot">
          <span>关闭标签页或断开连接会结束此浏览器连接。</span>
          <span className="terminal-foot-strong">shell 仍会继续运行。</span>
        </footer>
      </div>
    </section>
  );
}
