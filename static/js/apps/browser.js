/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let browserCount = 0;

export function create(desktop, initialUrl = '') {
    const id = `browser-${++browserCount}`;

    // History management
    const history = [];
    let historyIndex = -1;

    const container = document.createElement('div');
    container.className = 'browser-window';
    container.innerHTML = `
        <div class="browser-toolbar">
            <button class="nav-btn" id="back-${id}" title="Back" disabled>&larr;</button>
            <button class="nav-btn" id="forward-${id}" title="Forward" disabled>&rarr;</button>
            <button class="nav-btn" id="refresh-${id}" title="Refresh">&#8635;</button>
            <input type="text" class="url-bar" id="url-${id}" placeholder="Enter URL (e.g., https://example.com)">
            <button class="btn-primary" id="go-${id}">Go</button>
        </div>
        <div class="browser-content">
            <iframe id="frame-${id}" class="browser-frame" sandbox="allow-same-origin allow-scripts allow-forms allow-popups"></iframe>
            <div id="loading-${id}" class="browser-loading" style="display: none;">
                <span>Loading...</span>
            </div>
            <div id="error-${id}" class="browser-error" style="display: none;">
                <div class="error-icon">!</div>
                <div class="error-title">Unable to load page</div>
                <div class="error-message"></div>
            </div>
        </div>
        <div class="browser-status" id="status-${id}">Ready</div>
    `;

    const win = new WinBox({
        title: 'Browser',
        mount: container,
        width: '900px',
        height: '700px',
        x: 30 + (browserCount * 30) % 150,
        y: 30 + (browserCount * 30) % 100,
        bottom: 28,
        class: ['no-full'],
        onclose: () => {
            desktop.removeWindow(id);
            return false;
        },
    });

    const urlInput = container.querySelector(`#url-${id}`);
    const frame = container.querySelector(`#frame-${id}`);
    const backBtn = container.querySelector(`#back-${id}`);
    const forwardBtn = container.querySelector(`#forward-${id}`);
    const refreshBtn = container.querySelector(`#refresh-${id}`);
    const goBtn = container.querySelector(`#go-${id}`);
    const loadingEl = container.querySelector(`#loading-${id}`);
    const errorEl = container.querySelector(`#error-${id}`);
    const statusEl = container.querySelector(`#status-${id}`);

    let currentUrl = '';

    function updateHistoryButtons() {
        backBtn.disabled = historyIndex <= 0;
        forwardBtn.disabled = historyIndex >= history.length - 1;
    }

    function showLoading(show) {
        loadingEl.style.display = show ? 'flex' : 'none';
        frame.style.display = show ? 'none' : 'block';
        errorEl.style.display = 'none';
    }

    function showError(message) {
        loadingEl.style.display = 'none';
        frame.style.display = 'none';
        errorEl.style.display = 'flex';
        errorEl.querySelector('.error-message').textContent = message;
    }

    function normalizeUrl(url) {
        url = url.trim();
        if (!url) return '';

        // Add protocol if missing
        if (!url.startsWith('http://') && !url.startsWith('https://')) {
            // Check if it looks like a domain
            if (url.includes('.') && !url.includes(' ')) {
                url = 'https://' + url;
            } else {
                // Treat as search query - but we'll just show error for now
                return '';
            }
        }
        return url;
    }

    async function navigate(url, addToHistory = true) {
        url = normalizeUrl(url);
        if (!url) {
            showError('Please enter a valid URL (e.g., https://example.com)');
            return;
        }

        currentUrl = url;
        urlInput.value = url;
        win.setTitle(`Browser - ${new URL(url).hostname}`);
        statusEl.textContent = `Loading ${url}...`;
        showLoading(true);

        // Create proxy URL
        const proxyUrl = `/api/proxy?url=${encodeURIComponent(url)}`;

        try {
            // First check if the URL is accessible
            const response = await fetch(proxyUrl, { credentials: 'same-origin' });

            if (!response.ok) {
                const data = await response.json().catch(() => ({}));
                throw new Error(data.error || `HTTP ${response.status}`);
            }

            // Load in iframe
            frame.src = proxyUrl;

            // Add to history
            if (addToHistory) {
                // Remove forward history when navigating to new page
                if (historyIndex < history.length - 1) {
                    history.splice(historyIndex + 1);
                }
                history.push(url);
                historyIndex = history.length - 1;
                updateHistoryButtons();
            }

            statusEl.textContent = url;
            showLoading(false);

        } catch (e) {
            showError(`Failed to load page: ${e.message}`);
            statusEl.textContent = 'Error';
        }
    }

    function goBack() {
        if (historyIndex > 0) {
            historyIndex--;
            navigate(history[historyIndex], false);
            updateHistoryButtons();
        }
    }

    function goForward() {
        if (historyIndex < history.length - 1) {
            historyIndex++;
            navigate(history[historyIndex], false);
            updateHistoryButtons();
        }
    }

    function refresh() {
        if (currentUrl) {
            navigate(currentUrl, false);
        }
    }

    // Event listeners
    goBtn.addEventListener('click', () => navigate(urlInput.value));

    urlInput.addEventListener('keypress', (e) => {
        if (e.key === 'Enter') {
            navigate(urlInput.value);
        }
    });

    backBtn.addEventListener('click', goBack);
    forwardBtn.addEventListener('click', goForward);
    refreshBtn.addEventListener('click', refresh);

    // Listen for messages from iframe (navigation requests)
    window.addEventListener('message', (e) => {
        if (e.source === frame.contentWindow) {
            if (e.data.type === 'navigate') {
                navigate(e.data.url);
            } else if (e.data.type === 'form') {
                // Handle form submission through proxy
                handleFormSubmit(e.data);
            }
        }
    });

    async function handleFormSubmit(data) {
        const { url, method, data: formData } = data;
        statusEl.textContent = `Submitting form to ${url}...`;
        showLoading(true);

        try {
            const proxyUrl = `/api/proxy?url=${encodeURIComponent(url)}`;

            if (method === 'GET') {
                // Append form data to URL
                const params = new URLSearchParams(formData).toString();
                const fullUrl = url + (url.includes('?') ? '&' : '?') + params;
                navigate(fullUrl);
            } else {
                // POST request
                const response = await fetch(proxyUrl, {
                    method: 'POST',
                    headers: {
                        'Content-Type': 'application/x-www-form-urlencoded',
                    },
                    body: new URLSearchParams(formData).toString(),
                    credentials: 'same-origin',
                });

                if (!response.ok) {
                    throw new Error(`HTTP ${response.status}`);
                }

                // For POST, we need to display the response
                const html = await response.text();
                frame.srcdoc = html;

                currentUrl = url;
                urlInput.value = url;
                history.push(url);
                historyIndex = history.length - 1;
                updateHistoryButtons();
                statusEl.textContent = url;
                showLoading(false);
            }
        } catch (e) {
            showError(`Form submission failed: ${e.message}`);
            statusEl.textContent = 'Error';
        }
    }

    // Load initial URL if provided
    if (initialUrl) {
        navigate(initialUrl);
    }

    return { id, win, type: 'browser', navigate };
}
