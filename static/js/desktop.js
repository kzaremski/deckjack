/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

import * as Terminal from './apps/terminal.js';
import * as Files from './apps/files.js';
import * as Editor from './apps/editor.js';
import * as Images from './apps/images.js';
import * as VNC from './apps/vnc.js';
import * as Settings from './apps/settings.js';
import * as Browser from './apps/browser.js';
import * as Keepass from './apps/keepass.js';

class Desktop {
    constructor() {
        this.windows = new Map();
        this.vncAvailable = false;

        this.init();
    }

    async init() {
        // Load system info (hostname)
        await this.loadSystemInfo();

        // Load wallpaper
        await this.loadWallpaper();

        // Check VNC availability
        await this.checkVNC();

        // Set up context menu
        this.setupContextMenu();

        // Start clock
        this.updateClock();
        setInterval(() => this.updateClock(), 1000);

        // Start status updates
        this.updateStatus();
        setInterval(() => this.updateStatus(), 5000);

        // Make loadWallpaper available globally for settings page
        window.loadWallpaper = () => this.loadWallpaper();
    }

    async loadWallpaper() {
        try {
            const response = await fetch('/api/settings');
            const data = await response.json();
            const desktop = document.getElementById('desktop');

            if (data.wallpaper) {
                desktop.style.backgroundImage = `url('/api/wallpaper?${Date.now()}')`;
                desktop.style.backgroundColor = '';
                desktop.classList.add('has-wallpaper');
            } else if (data.wallpaper_color) {
                desktop.style.backgroundImage = '';
                desktop.style.backgroundColor = data.wallpaper_color;
                desktop.classList.add('has-wallpaper');
            } else {
                desktop.style.backgroundImage = '';
                desktop.style.backgroundColor = '';
                desktop.classList.remove('has-wallpaper');
            }
        } catch (e) {
            console.error('Failed to load wallpaper:', e);
        }
    }

    async loadSystemInfo() {
        try {
            const response = await fetch('/api/system/info');
            const data = await response.json();
            if (data.hostname) {
                document.getElementById('status-hostname').textContent = data.hostname;
            }
        } catch (e) {
            console.error('Failed to load system info:', e);
        }
    }

    async checkVNC() {
        try {
            const response = await fetch('/api/vnc/status');
            const data = await response.json();
            this.vncAvailable = data.available;
            if (this.vncAvailable) {
                document.getElementById('vnc-menu-item').style.display = 'block';
            }
            this.updateVNCStatus(data.available);
        } catch (e) {
            this.vncAvailable = false;
        }
    }

    setupContextMenu() {
        const menu = document.getElementById('context-menu');
        const desktop = document.getElementById('desktop');
        const statusBar = document.getElementById('status-bar');

        const showMenu = (e) => {
            e.preventDefault();

            // Position menu
            let x = e.clientX;
            let y = e.clientY;

            // Ensure menu stays within viewport
            menu.classList.add('visible');
            const rect = menu.getBoundingClientRect();

            if (x + rect.width > window.innerWidth) {
                x = window.innerWidth - rect.width - 5;
            }
            if (y + rect.height > window.innerHeight - 28) {
                y = window.innerHeight - 28 - rect.height - 5;
            }

            menu.style.left = x + 'px';
            menu.style.top = y + 'px';
        };

        const hideMenu = () => {
            menu.classList.remove('visible');
        };

        // Show context menu on right-click
        desktop.addEventListener('contextmenu', showMenu);
        statusBar.addEventListener('contextmenu', showMenu);

        // Hide menu on click elsewhere
        document.addEventListener('click', (e) => {
            if (!menu.contains(e.target)) {
                hideMenu();
            }
        });

        // Hide menu on escape
        document.addEventListener('keydown', (e) => {
            if (e.key === 'Escape') {
                hideMenu();
            }
        });

        // Handle menu item clicks
        menu.addEventListener('click', (e) => {
            const item = e.target.closest('.context-item');
            if (!item) return;

            const app = item.dataset.app;
            const action = item.dataset.action;

            if (app) {
                this.launchApp(app);
            } else if (action === 'logout') {
                window.location.href = '/logout';
            }

            hideMenu();
        });
    }

    async launchApp(app, options = {}) {
        let windowInfo;
        switch (app) {
            case 'terminal':
                windowInfo = Terminal.create(this);
                break;
            case 'files':
                windowInfo = Files.create(this, options.path || '');
                break;
            case 'editor':
                windowInfo = await Editor.create(this, options.path || '');
                break;
            case 'images':
                windowInfo = Images.create(this, options.path || '');
                break;
            case 'vnc':
                if (!this.vncAvailable) {
                    alert('VNC server not available');
                    return null;
                }
                windowInfo = await VNC.create(this);
                break;
            case 'settings':
                windowInfo = Settings.create(this);
                break;
            case 'browser':
                windowInfo = Browser.create(this, options.url || '');
                break;
            case 'keepass':
                windowInfo = Keepass.create(this);
                break;
            default:
                console.error('Unknown app:', app);
                return null;
        }

        if (windowInfo) {
            this.windows.set(windowInfo.id, windowInfo);
        }
        return windowInfo;
    }

    // Helper to open editor with a file
    async openEditor(path) {
        const windowInfo = await this.launchApp('editor', { path });
        return windowInfo;
    }

    removeWindow(id) {
        this.windows.delete(id);
    }

    updateClock() {
        const now = new Date();
        const hours = now.getHours().toString().padStart(2, '0');
        const minutes = now.getMinutes().toString().padStart(2, '0');
        document.getElementById('status-clock').textContent = `${hours}:${minutes}`;
    }

    async updateStatus() {
        // Connection count comes from WebSocket connections on server
        // For now we'll show window count as a proxy
        const activeConnections = this.windows.size;
        document.getElementById('status-connections').textContent = `${activeConnections} win`;
    }

    updateVNCStatus(available) {
        const el = document.getElementById('status-vnc');
        if (available) {
            el.innerHTML = 'VNC: <span style="color:#2ecc71">●</span>';
        } else {
            el.textContent = 'VNC: -';
        }
    }
}

// Initialize desktop when DOM is ready
document.addEventListener('DOMContentLoaded', () => {
    window.desktop = new Desktop();
});
