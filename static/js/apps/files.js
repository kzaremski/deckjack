/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let filesCounter = 0;

export function create(desktop, initialPath = '') {
    filesCounter++;
    const id = `files-${filesCounter}`;

    const container = document.createElement('div');
    container.className = 'files-window';
    container.innerHTML = `
        <div class="files-toolbar">
            <span>Files</span>
            <div>
                <input type="file" id="${id}-upload" hidden multiple>
                <button class="btn-secondary" data-action="upload">Upload</button>
            </div>
        </div>
        <nav class="files-breadcrumb" id="${id}-breadcrumb"></nav>
        <div class="files-content">
            <div class="files-list" id="${id}-list"></div>
            <div class="files-preview" id="${id}-preview">
                <p class="hint">Select a file to preview</p>
            </div>
        </div>
    `;

    let currentPath = initialPath;

    const win = new WinBox({
        title: 'Files',
        mount: container,
        width: '800px',
        height: '500px',
        x: 80 + (filesCounter * 30) % 200,
        y: 80 + (filesCounter * 30) % 150,
        bottom: 28,
        onclose: () => {
            desktop.removeWindow(id);
            return false;
        }
    });

    // Helper functions
    function formatSize(bytes) {
        if (bytes === 0) return '';
        const units = ['B', 'KB', 'MB', 'GB'];
        const i = Math.floor(Math.log(bytes) / Math.log(1024));
        return (bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
    }

    function renderBreadcrumb(path) {
        const parts = path.split('/').filter(p => p);
        let html = '<a href="#" data-path="/">/</a>';
        let accumulated = '';
        for (let i = 0; i < parts.length; i++) {
            const part = parts[i];
            accumulated += '/' + part;
            if (i > 0) {
                html += ' <span>/</span>';
            }
            html += ` <a href="#" data-path="${accumulated}">${part}</a>`;
        }
        container.querySelector(`#${id}-breadcrumb`).innerHTML = html;
    }

    function renderFileList(items, parent) {
        let html = '';
        if (parent) {
            html += `<div class="file-item dir" data-path="${parent}" data-type="dir">
                <span class="icon">&#128194;</span>
                <span class="name">..</span>
            </div>`;
        }
        for (const item of items) {
            const icon = item.is_dir ? '&#128193;' : '&#128196;';
            const cls = item.is_dir ? 'dir' : 'file';
            const type = item.is_dir ? 'dir' : 'file';
            const size = item.is_dir ? '' : formatSize(item.size);
            html += `<div class="file-item ${cls}" data-path="${item.path}" data-type="${type}">
                <span class="icon">${icon}</span>
                <span class="name">${item.name}</span>
                <span class="size">${size}</span>
            </div>`;
        }
        container.querySelector(`#${id}-list`).innerHTML = html || '<p class="hint">Empty directory</p>';
    }

    async function loadDirectory(path = '') {
        try {
            const response = await fetch(`/api/files/list?path=${encodeURIComponent(path)}`);
            const data = await response.json();
            if (data.error) {
                alert(data.error);
                return;
            }
            currentPath = data.path;
            win.setTitle(`Files - ${currentPath}`);
            renderBreadcrumb(data.path);
            renderFileList(data.items, data.parent);
        } catch (e) {
            console.error('Failed to load directory:', e);
        }
    }

    async function previewFile(path) {
        try {
            const response = await fetch(`/api/files/read?path=${encodeURIComponent(path)}`);
            const data = await response.json();
            const previewEl = container.querySelector(`#${id}-preview`);

            if (data.error) {
                previewEl.innerHTML = `<p class="error">${data.error}</p>`;
                return;
            }

            const filename = path.split('/').pop();
            if (data.is_image) {
                previewEl.innerHTML = `
                    <div class="preview-header">
                        <span>${filename}</span>
                        <div>
                            <a href="${data.url}" download class="btn-secondary">Download</a>
                        </div>
                    </div>
                    <img src="${data.url}" alt="Preview" class="image-preview">`;
            } else {
                const escaped = (data.content || '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
                previewEl.innerHTML = `
                    <div class="preview-header">
                        <span>${filename}</span>
                        <div>
                            <button class="btn-secondary" data-action="edit" data-path="${path}">Edit</button>
                            <a href="/api/files/download${path}" download class="btn-secondary">Download</a>
                        </div>
                    </div>
                    <pre class="code-preview">${escaped}</pre>`;
            }
        } catch (e) {
            console.error('Failed to preview file:', e);
        }
    }

    // Event handlers
    container.addEventListener('click', async (e) => {
        const target = e.target.closest('[data-path]');
        if (target) {
            e.preventDefault();
            const path = target.dataset.path;
            const type = target.dataset.type;
            if (type === 'dir' || target.tagName === 'A') {
                loadDirectory(path);
            } else if (type === 'file') {
                previewFile(path);
            }
        }

        const action = e.target.closest('[data-action]');
        if (action) {
            if (action.dataset.action === 'upload') {
                container.querySelector(`#${id}-upload`).click();
            } else if (action.dataset.action === 'edit') {
                desktop.openEditor(action.dataset.path);
            }
        }
    });

    container.querySelector(`#${id}-upload`).addEventListener('change', async (e) => {
        const files = e.target.files;
        for (const file of files) {
            const formData = new FormData();
            formData.append('file', file);
            formData.append('directory', currentPath);
            await fetch('/api/files/upload', { method: 'POST', body: formData });
        }
        loadDirectory(currentPath);
        e.target.value = '';
    });

    // Initial load
    loadDirectory(initialPath);

    return { id, win, type: 'files' };
}
