/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let terminalCounter = 0;

export function create(desktop) {
    terminalCounter++;
    const id = `terminal-${terminalCounter}`;

    const container = document.createElement('div');
    container.className = 'terminal-container';
    container.id = id;

    const term = new Terminal({
        cursorBlink: true,
        theme: {
            background: '#1a1a2e',
            foreground: '#eaeaea',
            cursor: '#e94560',
            cursorAccent: '#1a1a2e',
            selectionBackground: '#e94560',
            black: '#1a1a2e',
            red: '#e94560',
            green: '#0f3460',
            yellow: '#f39c12',
            blue: '#3498db',
            magenta: '#9b59b6',
            cyan: '#1abc9c',
            white: '#eaeaea',
            brightBlack: '#555555',
            brightRed: '#ff6b8a',
            brightGreen: '#2ecc71',
            brightYellow: '#f1c40f',
            brightBlue: '#5dade2',
            brightMagenta: '#bb8fce',
            brightCyan: '#48c9b0',
            brightWhite: '#ffffff'
        },
        fontFamily: 'Menlo, Monaco, "Courier New", monospace',
        fontSize: 14,
        scrollback: 10000
    });

    const fitAddon = new FitAddon.FitAddon();
    const webLinksAddon = new WebLinksAddon.WebLinksAddon();

    term.loadAddon(fitAddon);
    term.loadAddon(webLinksAddon);

    let ws = null;
    let isClosing = false;
    let resizeObserver = null;

    const win = new WinBox({
        title: `Terminal ${terminalCounter}`,
        mount: container,
        width: '700px',
        height: '450px',
        x: 50 + (terminalCounter * 30) % 200,
        y: 50 + (terminalCounter * 30) % 150,
        bottom: 28,
        class: ['no-full'],
        onclose: () => {
            isClosing = true;
            if (resizeObserver) resizeObserver.disconnect();
            if (ws && ws.readyState === WebSocket.OPEN) {
                ws.close();
            }
            term.dispose();
            desktop.removeWindow(id);
            return false;
        },
        onfocus: () => {
            term.focus();
        }
    });

    // Use ResizeObserver for reliable resize detection
    resizeObserver = new ResizeObserver(() => {
        if (fitAddon && term.element) {
            fitAddon.fit();
        }
    });
    resizeObserver.observe(container);

    // Open terminal after window is mounted
    setTimeout(() => {
        term.open(container);
        fitAddon.fit();

        // Connect WebSocket
        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        ws = new WebSocket(`${protocol}//${window.location.host}/ws/terminal`);
        ws.binaryType = 'arraybuffer';

        ws.onopen = () => {
            ws.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }));
            term.focus();
        };

        ws.onmessage = (event) => {
            const data = new Uint8Array(event.data);
            term.write(data);
        };

        ws.onclose = () => {
            if (!isClosing) {
                term.write('\r\n\x1b[31m[Connection closed]\x1b[0m\r\n');
            }
        };

        ws.onerror = () => {
            term.write('\r\n\x1b[31m[Connection error]\x1b[0m\r\n');
        };

        term.onData(data => {
            if (ws && ws.readyState === WebSocket.OPEN) {
                ws.send(new TextEncoder().encode(data));
            }
        });

        term.onResize(({ cols, rows }) => {
            if (ws && ws.readyState === WebSocket.OPEN) {
                ws.send(JSON.stringify({ type: 'resize', cols, rows }));
            }
        });
    }, 50);

    return { id, win, type: 'terminal' };
}
