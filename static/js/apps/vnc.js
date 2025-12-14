/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let vncCounter = 0;
let RFB = null;

async function loadNoVNC() {
    if (RFB) return RFB;
    const module = await import('https://cdn.jsdelivr.net/npm/@novnc/novnc@1.4.0/lib/rfb.js');
    RFB = module.default;
    return RFB;
}

export async function create(desktop) {
    const RFBClass = await loadNoVNC();

    vncCounter++;
    const id = `vnc-${vncCounter}`;

    const container = document.createElement('div');
    container.className = 'vnc-window';
    container.innerHTML = `
        <div class="vnc-toolbar">
            <span>Desktop (VNC)</span>
            <div>
                <button class="btn-secondary" data-action="fullscreen">Fullscreen</button>
                <button class="btn-secondary" data-action="ctrlaltdel">Ctrl+Alt+Del</button>
            </div>
        </div>
        <div class="vnc-container" id="${id}-container"></div>
        <div class="vnc-status" id="${id}-status">Connecting...</div>
    `;

    let rfb = null;

    const win = new WinBox({
        title: 'Desktop (VNC)',
        mount: container,
        width: '1024px',
        height: '768px',
        x: 50 + (vncCounter * 30) % 150,
        y: 30 + (vncCounter * 30) % 100,
        bottom: 28,
        onclose: () => {
            if (rfb) {
                rfb.disconnect();
            }
            desktop.removeWindow(id);
            return false;
        }
    });

    function updateStatus(text, className = '') {
        const status = container.querySelector(`#${id}-status`);
        status.textContent = text;
        status.className = 'vnc-status ' + className;
    }

    function connect() {
        const vncContainer = container.querySelector(`#${id}-container`);
        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        const url = `${protocol}//${window.location.host}/ws/vnc`;

        rfb = new RFBClass(vncContainer, url, {
            scaleViewport: true,
            resizeSession: true,
            clipViewport: true
        });

        rfb.addEventListener('connect', () => {
            updateStatus('Connected', 'connected');
        });

        rfb.addEventListener('disconnect', (e) => {
            if (e.detail.clean) {
                updateStatus('Disconnected');
            } else {
                updateStatus('Connection lost', 'error');
            }
        });

        rfb.addEventListener('credentialsrequired', () => {
            updateStatus('VNC requires credentials', 'error');
        });
    }

    function toggleFullscreen() {
        const vncContainer = container.querySelector(`#${id}-container`);
        if (document.fullscreenElement) {
            document.exitFullscreen();
        } else {
            vncContainer.requestFullscreen();
        }
    }

    function sendCtrlAltDel() {
        if (rfb) {
            rfb.sendCtrlAltDel();
        }
    }

    // Event handlers
    container.addEventListener('click', (e) => {
        const action = e.target.closest('[data-action]');
        if (action) {
            if (action.dataset.action === 'fullscreen') {
                toggleFullscreen();
            } else if (action.dataset.action === 'ctrlaltdel') {
                sendCtrlAltDel();
            }
        }
    });

    // Connect after a brief delay to ensure DOM is ready
    setTimeout(connect, 100);

    return { id, win, type: 'vnc' };
}
