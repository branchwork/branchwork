import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";

export default function PtyTerminal({ agentId }: { agentId: string }) {
  const termRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!termRef.current) return;

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', Menlo, monospace",
      theme: {
        background: "#0a0a0f",
        foreground: "#e4e4e7",
        cursor: "#818cf8",
        selectionBackground: "#818cf840",
        black: "#18181b",
        red: "#f87171",
        green: "#4ade80",
        yellow: "#facc15",
        blue: "#60a5fa",
        magenta: "#c084fc",
        cyan: "#22d3ee",
        white: "#e4e4e7",
        brightBlack: "#52525b",
        brightRed: "#fca5a5",
        brightGreen: "#86efac",
        brightYellow: "#fde047",
        brightBlue: "#93c5fd",
        brightMagenta: "#d8b4fe",
        brightCyan: "#67e8f9",
        brightWhite: "#fafafa",
      },
      scrollback: 10000,
    });

    const fitAddon = new FitAddon();
    const webLinksAddon = new WebLinksAddon();
    term.loadAddon(fitAddon);
    term.loadAddon(webLinksAddon);
    term.open(termRef.current);

    // Connect WebSocket
    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(`${protocol}//${window.location.host}/terminal?agent=${agentId}`);

    ws.onopen = () => {
      // Fit after WS opens so we can send correct size
      requestAnimationFrame(() => {
        fitAddon.fit();
        ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      });
    };

    ws.onmessage = (ev) => {
      term.write(ev.data);
    };

    ws.onerror = () => {
      term.write("\r\n\x1b[31m--- connection error ---\x1b[0m\r\n");
    };

    ws.onclose = () => {
      term.write("\r\n\x1b[90m--- session ended ---\x1b[0m\r\n");
    };

    // Forward terminal input to WebSocket
    term.onData((data) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(data);
      }
    });

    // Handle resize
    const resizeObserver = new ResizeObserver(() => {
      requestAnimationFrame(() => {
        fitAddon.fit();
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
        }
      });
    });
    resizeObserver.observe(termRef.current);

    return () => {
      resizeObserver.disconnect();
      // Detach handlers first so any orphan messages from a still-connecting
      // socket don't write to a disposed terminal — and defer close() until
      // the socket is actually open. Calling close() on a CONNECTING socket
      // produces the dev-only "WebSocket is closed before the connection is
      // established" error during React StrictMode's mount/unmount/remount
      // cycle. The deferred close keeps the orphan WS quiet without
      // leaking it.
      ws.onopen = null;
      ws.onmessage = null;
      ws.onerror = null;
      ws.onclose = null;
      if (ws.readyState === WebSocket.CONNECTING) {
        ws.addEventListener("open", () => ws.close(), { once: true });
      } else {
        ws.close();
      }
      term.dispose();
    };
  }, [agentId]);

  return (
    <div
      ref={termRef}
      className="flex-1 min-h-0"
      style={{ padding: "4px", background: "#0a0a0f" }}
    />
  );
}
