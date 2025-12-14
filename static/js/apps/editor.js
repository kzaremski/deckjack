/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let editorCounter = 0;
let monacoLoaded = false;
let monacoLoadPromise = null;

function loadMonaco() {
    if (monacoLoaded) return Promise.resolve();
    if (monacoLoadPromise) return monacoLoadPromise;

    monacoLoadPromise = new Promise((resolve) => {
        const script = document.createElement('script');
        script.src = 'https://cdn.jsdelivr.net/npm/monaco-editor@0.44.0/min/vs/loader.js';
        script.onload = () => {
            require.config({ paths: { vs: 'https://cdn.jsdelivr.net/npm/monaco-editor@0.44.0/min/vs' } });
            require(['vs/editor/editor.main'], () => {
                monaco.editor.defineTheme('jacklink', {
                    base: 'vs-dark',
                    inherit: true,
                    rules: [],
                    colors: {
                        'editor.background': '#1a1a2e',
                        'editor.foreground': '#eaeaea',
                        'editorCursor.foreground': '#e94560',
                        'editor.lineHighlightBackground': '#16213e',
                        'editorLineNumber.foreground': '#555',
                        'editor.selectionBackground': '#e9456040',
                        'editor.inactiveSelectionBackground': '#e9456020'
                    }
                });
                monacoLoaded = true;
                resolve();
            });
        };
        document.head.appendChild(script);
    });
    return monacoLoadPromise;
}

function getLanguage(path) {
    const ext = path.split('.').pop().toLowerCase();
    const map = {
        js: 'javascript', ts: 'typescript', jsx: 'javascript', tsx: 'typescript',
        py: 'python', rb: 'ruby', go: 'go', rs: 'rust', java: 'java',
        c: 'c', cpp: 'cpp', h: 'c', hpp: 'cpp', cs: 'csharp',
        html: 'html', css: 'css', scss: 'scss', less: 'less',
        json: 'json', xml: 'xml', yaml: 'yaml', yml: 'yaml',
        md: 'markdown', sh: 'shell', bash: 'shell', zsh: 'shell',
        sql: 'sql', php: 'php', swift: 'swift', kt: 'kotlin',
        dockerfile: 'dockerfile', makefile: 'makefile'
    };
    return map[ext] || 'plaintext';
}

export async function create(desktop, initialPath = '') {
    await loadMonaco();

    editorCounter++;
    const id = `editor-${editorCounter}`;

    const container = document.createElement('div');
    container.className = 'editor-window';
    container.innerHTML = `
        <div class="editor-toolbar">
            <div style="display:flex;align-items:center;">
                <span>Editor</span>
                <span class="editor-path" id="${id}-path">No file open</span>
            </div>
            <div>
                <button class="btn-secondary" data-action="open">Open</button>
                <button class="btn-primary" data-action="save">Save</button>
            </div>
        </div>
        <div class="editor-content" id="${id}-content"></div>
        <div class="editor-status" id="${id}-status">Ready</div>
    `;

    let currentPath = initialPath;
    let isModified = false;
    let editor = null;

    const win = new WinBox({
        title: initialPath ? `Editor - ${initialPath.split('/').pop()}` : 'Editor',
        mount: container,
        width: '800px',
        height: '600px',
        x: 100 + (editorCounter * 30) % 200,
        y: 60 + (editorCounter * 30) % 150,
        bottom: 28,
        onclose: () => {
            if (editor) {
                editor.dispose();
            }
            desktop.removeWindow(id);
            return false;
        },
        onresize: () => {
            if (editor) {
                editor.layout();
            }
        }
    });

    function updateStatus(text, className = '') {
        const status = container.querySelector(`#${id}-status`);
        status.textContent = text;
        status.className = 'editor-status ' + className;
    }

    async function loadFile(path) {
        try {
            const response = await fetch(`/api/files/read?path=${encodeURIComponent(path)}`);
            const data = await response.json();
            if (data.error) {
                updateStatus('Error: ' + data.error, 'error');
                return;
            }
            currentPath = data.path;
            container.querySelector(`#${id}-path`).textContent = currentPath;
            win.setTitle(`Editor - ${currentPath.split('/').pop()}`);

            const lang = getLanguage(currentPath);
            monaco.editor.setModelLanguage(editor.getModel(), lang);
            editor.setValue(data.content || '');
            isModified = false;
            updateStatus('Loaded', 'saved');
        } catch (e) {
            updateStatus('Error loading file', 'error');
        }
    }

    async function saveFile() {
        if (!currentPath) {
            updateStatus('No file open', 'error');
            return;
        }
        try {
            const content = editor.getValue();
            const response = await fetch('/api/files/write', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ path: currentPath, content })
            });
            const data = await response.json();
            if (data.error) {
                updateStatus('Error: ' + data.error, 'error');
            } else {
                isModified = false;
                updateStatus('Saved', 'saved');
            }
        } catch (e) {
            updateStatus('Error saving file', 'error');
        }
    }

    function openFile() {
        const path = prompt('Enter file path:', currentPath || '/');
        if (path) {
            loadFile(path);
        }
    }

    // Initialize Monaco editor
    setTimeout(() => {
        const contentEl = container.querySelector(`#${id}-content`);
        editor = monaco.editor.create(contentEl, {
            value: '',
            language: 'plaintext',
            theme: 'jacklink',
            automaticLayout: true,
            minimap: { enabled: false },
            fontSize: 14,
            fontFamily: 'Menlo, Monaco, "Courier New", monospace',
            scrollBeyondLastLine: false,
            renderWhitespace: 'selection',
            tabSize: 4
        });

        editor.onDidChangeModelContent(() => {
            if (!isModified) {
                isModified = true;
                updateStatus('Modified', 'modified');
            }
        });

        editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, saveFile);

        if (initialPath) {
            loadFile(initialPath);
        }
    }, 50);

    // Event handlers
    container.addEventListener('click', (e) => {
        const action = e.target.closest('[data-action]');
        if (action) {
            if (action.dataset.action === 'open') {
                openFile();
            } else if (action.dataset.action === 'save') {
                saveFile();
            }
        }
    });

    return { id, win, type: 'editor', loadFile };
}
