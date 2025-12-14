/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let imagesCounter = 0;

const IMAGE_EXTENSIONS = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'bmp', 'svg', 'ico'];

function isImage(filename) {
    const ext = filename.split('.').pop().toLowerCase();
    return IMAGE_EXTENSIONS.includes(ext);
}

export function create(desktop, initialPath = '') {
    imagesCounter++;
    const id = `images-${imagesCounter}`;

    const container = document.createElement('div');
    container.className = 'images-window';
    container.innerHTML = `
        <div class="images-toolbar">
            <span>Images</span>
            <div>
                <button class="btn-secondary" data-action="rotate-left" title="Rotate Left">&#8634;</button>
                <button class="btn-secondary" data-action="rotate-right" title="Rotate Right">&#8635;</button>
                <button class="btn-secondary" data-action="browse">Browse...</button>
            </div>
        </div>
        <div class="images-content">
            <div class="images-grid" id="${id}-grid"></div>
            <div class="images-viewer" id="${id}-viewer">
                <p class="hint">Select a directory to browse images</p>
            </div>
        </div>
    `;

    let currentPath = initialPath;
    let images = [];
    let selectedIndex = -1;
    let rotation = 0;

    const win = new WinBox({
        title: 'Images',
        mount: container,
        width: '900px',
        height: '600px',
        x: 120 + (imagesCounter * 30) % 200,
        y: 70 + (imagesCounter * 30) % 150,
        bottom: 28,
        onclose: () => {
            desktop.removeWindow(id);
            return false;
        }
    });

    function renderGrid() {
        const gridEl = container.querySelector(`#${id}-grid`);
        if (images.length === 0) {
            gridEl.innerHTML = '<p class="hint">No images found</p>';
            return;
        }
        let html = '';
        for (let i = 0; i < images.length; i++) {
            const img = images[i];
            const selected = i === selectedIndex ? 'selected' : '';
            html += `<img class="image-thumb ${selected}" data-index="${i}" src="/api/files/download${img.path}" alt="${img.name}">`;
        }
        gridEl.innerHTML = html;
    }

    function showImage(index) {
        if (index < 0 || index >= images.length) return;
        selectedIndex = index;
        rotation = 0;
        const img = images[index];
        const viewerEl = container.querySelector(`#${id}-viewer`);
        viewerEl.innerHTML = `<img src="/api/files/download${img.path}" alt="${img.name}" style="transform: rotate(0deg)">`;
        win.setTitle(`Images - ${img.name}`);
        renderGrid();
    }

    function rotateImage(degrees) {
        rotation = (rotation + degrees) % 360;
        const img = container.querySelector(`#${id}-viewer img`);
        if (img) {
            img.style.transform = `rotate(${rotation}deg)`;
        }
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
            images = data.items.filter(item => !item.is_dir && isImage(item.name));
            selectedIndex = -1;
            win.setTitle(`Images - ${currentPath}`);
            renderGrid();

            const viewerEl = container.querySelector(`#${id}-viewer`);
            if (images.length > 0) {
                showImage(0);
            } else {
                viewerEl.innerHTML = '<p class="hint">No images in this directory</p>';
            }
        } catch (e) {
            console.error('Failed to load directory:', e);
        }
    }

    function browse() {
        const path = prompt('Enter directory path:', currentPath || '/');
        if (path) {
            loadDirectory(path);
        }
    }

    // Event handlers
    container.addEventListener('click', (e) => {
        const thumb = e.target.closest('.image-thumb');
        if (thumb) {
            showImage(parseInt(thumb.dataset.index));
            return;
        }

        const action = e.target.closest('[data-action]');
        if (action) {
            if (action.dataset.action === 'browse') {
                browse();
            } else if (action.dataset.action === 'rotate-left') {
                rotateImage(-90);
            } else if (action.dataset.action === 'rotate-right') {
                rotateImage(90);
            }
        }
    });

    // Keyboard navigation
    container.addEventListener('keydown', (e) => {
        if (e.key === 'ArrowLeft' && selectedIndex > 0) {
            showImage(selectedIndex - 1);
        } else if (e.key === 'ArrowRight' && selectedIndex < images.length - 1) {
            showImage(selectedIndex + 1);
        }
    });

    // Initial load
    if (initialPath) {
        loadDirectory(initialPath);
    }

    return { id, win, type: 'images', loadDirectory };
}
