/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let keepassCounter = 0;
let argon2Initialized = false;

async function initArgon2() {
    if (argon2Initialized) return;

    // Configure kdbxweb to use argon2-browser
    if (window.kdbxweb && window.argon2) {
        kdbxweb.CryptoEngine.setArgon2Impl((password, salt, memory, iterations, length, parallelism, type, version) => {
            return argon2.hash({
                pass: new Uint8Array(password),
                salt: new Uint8Array(salt),
                time: iterations,
                mem: memory,
                hashLen: length,
                parallelism: parallelism,
                type: type,
                version: version
            }).then(result => result.hash);
        });
        argon2Initialized = true;
    }
}

// Password generator
function generatePassword(options = {}) {
    const {
        length = 20,
        uppercase = true,
        lowercase = true,
        numbers = true,
        symbols = true
    } = options;

    let chars = '';
    if (uppercase) chars += 'ABCDEFGHIJKLMNOPQRSTUVWXYZ';
    if (lowercase) chars += 'abcdefghijklmnopqrstuvwxyz';
    if (numbers) chars += '0123456789';
    if (symbols) chars += '!@#$%^&*()_+-=[]{}|;:,.<>?';

    if (!chars) chars = 'abcdefghijklmnopqrstuvwxyz';

    const array = new Uint32Array(length);
    crypto.getRandomValues(array);

    let password = '';
    for (let i = 0; i < length; i++) {
        password += chars[array[i] % chars.length];
    }
    return password;
}

// TOTP Generator (RFC 6238)
function base32Decode(encoded) {
    const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
    encoded = encoded.toUpperCase().replace(/[^A-Z2-7]/g, '');

    let bits = '';
    for (const char of encoded) {
        const val = alphabet.indexOf(char);
        if (val === -1) continue;
        bits += val.toString(2).padStart(5, '0');
    }

    const bytes = [];
    for (let i = 0; i + 8 <= bits.length; i += 8) {
        bytes.push(parseInt(bits.slice(i, i + 8), 2));
    }
    return new Uint8Array(bytes);
}

// HMAC-SHA1 implementation (pure JS fallback)
function hmacSha1Sync(key, message) {
    // SHA1 implementation
    function sha1(msg) {
        function rotl(n, s) { return (n << s) | (n >>> (32 - s)); }

        let H0 = 0x67452301, H1 = 0xEFCDAB89, H2 = 0x98BADCFE, H3 = 0x10325476, H4 = 0xC3D2E1F0;

        // Pre-processing
        const msgLen = msg.length;
        const bitLen = msgLen * 8;

        // Padding
        const padded = new Uint8Array(Math.ceil((msgLen + 9) / 64) * 64);
        padded.set(msg);
        padded[msgLen] = 0x80;

        // Length in bits as 64-bit big-endian
        const view = new DataView(padded.buffer);
        view.setUint32(padded.length - 4, bitLen, false);

        // Process each 64-byte chunk
        for (let i = 0; i < padded.length; i += 64) {
            const W = new Uint32Array(80);
            for (let t = 0; t < 16; t++) {
                W[t] = view.getUint32(i + t * 4, false);
            }
            for (let t = 16; t < 80; t++) {
                W[t] = rotl(W[t-3] ^ W[t-8] ^ W[t-14] ^ W[t-16], 1);
            }

            let a = H0, b = H1, c = H2, d = H3, e = H4;

            for (let t = 0; t < 80; t++) {
                let f, k;
                if (t < 20) { f = (b & c) | ((~b) & d); k = 0x5A827999; }
                else if (t < 40) { f = b ^ c ^ d; k = 0x6ED9EBA1; }
                else if (t < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8F1BBCDC; }
                else { f = b ^ c ^ d; k = 0xCA62C1D6; }

                const temp = (rotl(a, 5) + f + e + k + W[t]) >>> 0;
                e = d; d = c; c = rotl(b, 30); b = a; a = temp;
            }

            H0 = (H0 + a) >>> 0;
            H1 = (H1 + b) >>> 0;
            H2 = (H2 + c) >>> 0;
            H3 = (H3 + d) >>> 0;
            H4 = (H4 + e) >>> 0;
        }

        const result = new Uint8Array(20);
        const resultView = new DataView(result.buffer);
        resultView.setUint32(0, H0, false);
        resultView.setUint32(4, H1, false);
        resultView.setUint32(8, H2, false);
        resultView.setUint32(12, H3, false);
        resultView.setUint32(16, H4, false);
        return result;
    }

    const blockSize = 64;
    let keyBytes = new Uint8Array(key);

    if (keyBytes.length > blockSize) {
        keyBytes = sha1(keyBytes);
    }

    const paddedKey = new Uint8Array(blockSize);
    paddedKey.set(keyBytes);

    const ipad = new Uint8Array(blockSize + message.length);
    const opad = new Uint8Array(blockSize + 20);

    for (let i = 0; i < blockSize; i++) {
        ipad[i] = paddedKey[i] ^ 0x36;
        opad[i] = paddedKey[i] ^ 0x5c;
    }

    ipad.set(new Uint8Array(message), blockSize);
    const innerHash = sha1(ipad);
    opad.set(innerHash, blockSize);

    return sha1(opad);
}

function generateTOTP(secret, timeStep = 30, digits = 6) {
    try {
        // Clean and validate secret
        const cleanSecret = secret.toUpperCase().replace(/[^A-Z2-7]/g, '');
        if (cleanSecret.length < 8) {
            return null;
        }

        const key = base32Decode(cleanSecret);
        if (key.length === 0) {
            return null;
        }

        const time = Math.floor(Date.now() / 1000 / timeStep);

        // Convert time to 8-byte big-endian array
        const timeBytes = new Uint8Array(8);
        const timeView = new DataView(timeBytes.buffer);
        timeView.setUint32(4, time, false);

        const hmac = hmacSha1Sync(key, timeBytes);
        const offset = hmac[19] & 0x0f;
        const code = ((hmac[offset] & 0x7f) << 24 |
                     (hmac[offset + 1] & 0xff) << 16 |
                     (hmac[offset + 2] & 0xff) << 8 |
                     (hmac[offset + 3] & 0xff)) % Math.pow(10, digits);

        return code.toString().padStart(digits, '0');
    } catch (err) {
        console.error('TOTP generation error:', err);
        return null;
    }
}

function getTOTPTimeRemaining(timeStep = 30) {
    return timeStep - (Math.floor(Date.now() / 1000) % timeStep);
}

export function create(desktop, filePath = '') {
    keepassCounter++;
    const id = `keepass-${keepassCounter}`;

    const container = document.createElement('div');
    container.className = 'keepass-container';
    container.id = id;

    let db = null;
    let selectedGroup = null;
    let selectedEntry = null;
    let totpInterval = null;

    // Initial locked state HTML
    container.innerHTML = `
        <div class="keepass-locked">
            <div class="keepass-lock-icon">&#128272;</div>
            <div class="keepass-lock-text">KeePass Viewer</div>
            <div class="keepass-lock-subtext">Open a .kdbx file to view passwords</div>
            <input type="file" id="kdbx-file-${keepassCounter}" accept=".kdbx" style="display:none">
            <button class="keepass-open-btn" id="kdbx-open-${keepassCounter}">Open Database</button>
            <div class="keepass-tools-standalone">
                <button class="keepass-tool-btn" id="kdbx-pwgen-standalone-${keepassCounter}">&#127922; Password Generator</button>
                <button class="keepass-tool-btn" id="kdbx-totp-standalone-${keepassCounter}">&#128337; TOTP Generator</button>
            </div>
        </div>
        <div class="keepass-password-dialog" style="display:none">
            <div class="keepass-dialog-content">
                <div class="keepass-dialog-title">Enter Master Password</div>
                <div class="keepass-dialog-filename"></div>
                <input type="password" class="keepass-password-input" placeholder="Master Password">
                <div class="keepass-dialog-error" style="display:none"></div>
                <div class="keepass-dialog-buttons">
                    <button class="keepass-btn keepass-btn-cancel">Cancel</button>
                    <button class="keepass-btn keepass-btn-unlock">Unlock</button>
                </div>
            </div>
        </div>
        <div class="keepass-generator-dialog" style="display:none">
            <div class="keepass-dialog-content">
                <div class="keepass-dialog-title">Password Generator</div>
                <div class="keepass-gen-output">
                    <input type="text" class="keepass-gen-password" readonly>
                    <button class="keepass-gen-copy">Copy</button>
                </div>
                <div class="keepass-gen-options">
                    <label><input type="range" class="keepass-gen-length" min="8" max="64" value="20"> Length: <span class="keepass-gen-length-val">20</span></label>
                    <label><input type="checkbox" class="keepass-gen-upper" checked> Uppercase (A-Z)</label>
                    <label><input type="checkbox" class="keepass-gen-lower" checked> Lowercase (a-z)</label>
                    <label><input type="checkbox" class="keepass-gen-numbers" checked> Numbers (0-9)</label>
                    <label><input type="checkbox" class="keepass-gen-symbols" checked> Symbols (!@#$...)</label>
                </div>
                <div class="keepass-dialog-buttons">
                    <button class="keepass-btn keepass-btn-regenerate">Regenerate</button>
                    <button class="keepass-btn keepass-btn-close-gen">Close</button>
                </div>
            </div>
        </div>
        <div class="keepass-totp-dialog" style="display:none">
            <div class="keepass-dialog-content">
                <div class="keepass-dialog-title">TOTP Generator</div>
                <div class="keepass-totp-input-row">
                    <input type="text" class="keepass-totp-secret" placeholder="Enter TOTP secret (base32)">
                </div>
                <div class="keepass-totp-output" style="display:none">
                    <div class="keepass-totp-code">------</div>
                    <div class="keepass-totp-timer">
                        <div class="keepass-totp-timer-bar"></div>
                    </div>
                    <div class="keepass-totp-remaining">Expires in <span>30</span>s</div>
                    <button class="keepass-totp-copy">Copy Code</button>
                </div>
                <div class="keepass-dialog-buttons">
                    <button class="keepass-btn keepass-btn-close-totp">Close</button>
                </div>
            </div>
        </div>
        <div class="keepass-main" style="display:none">
            <div class="keepass-toolbar">
                <button class="keepass-toolbar-btn" id="kdbx-lock-${keepassCounter}" title="Lock">&#128274; Lock</button>
                <button class="keepass-toolbar-btn" id="kdbx-pwgen-${keepassCounter}" title="Password Generator">&#127922; Generate</button>
                <button class="keepass-toolbar-btn" id="kdbx-totp-${keepassCounter}" title="TOTP Generator">&#128337; TOTP</button>
                <div class="keepass-search-wrapper">
                    <input type="text" class="keepass-search" placeholder="Search entries...">
                </div>
            </div>
            <div class="keepass-content">
                <div class="keepass-sidebar">
                    <div class="keepass-groups"></div>
                </div>
                <div class="keepass-entries-panel">
                    <div class="keepass-entries"></div>
                </div>
                <div class="keepass-details">
                    <div class="keepass-details-empty">Select an entry to view details</div>
                    <div class="keepass-details-content" style="display:none"></div>
                </div>
            </div>
        </div>
    `;

    const win = new WinBox({
        title: 'KeePass',
        mount: container,
        width: '900px',
        height: '600px',
        x: 80 + (keepassCounter * 30) % 200,
        y: 60 + (keepassCounter * 30) % 150,
        bottom: 28,
        class: ['no-full'],
        onclose: () => {
            // Clear sensitive data
            if (db) {
                db.close();
                db = null;
            }
            if (totpInterval) clearInterval(totpInterval);
            desktop.removeWindow(id);
            return false;
        }
    });

    // DOM elements
    const lockedView = container.querySelector('.keepass-locked');
    const passwordDialog = container.querySelector('.keepass-password-dialog');
    const generatorDialog = container.querySelector('.keepass-generator-dialog');
    const totpDialog = container.querySelector('.keepass-totp-dialog');
    const mainView = container.querySelector('.keepass-main');
    const fileInput = container.querySelector(`#kdbx-file-${keepassCounter}`);
    const openBtn = container.querySelector(`#kdbx-open-${keepassCounter}`);
    const lockBtn = container.querySelector(`#kdbx-lock-${keepassCounter}`);
    const pwgenBtn = container.querySelector(`#kdbx-pwgen-${keepassCounter}`);
    const totpBtn = container.querySelector(`#kdbx-totp-${keepassCounter}`);
    const pwgenStandaloneBtn = container.querySelector(`#kdbx-pwgen-standalone-${keepassCounter}`);
    const totpStandaloneBtn = container.querySelector(`#kdbx-totp-standalone-${keepassCounter}`);
    const passwordInput = container.querySelector('.keepass-password-input');
    const dialogError = container.querySelector('.keepass-dialog-error');
    const dialogFilename = container.querySelector('.keepass-dialog-filename');
    const cancelBtn = container.querySelector('.keepass-btn-cancel');
    const unlockBtn = container.querySelector('.keepass-btn-unlock');
    const groupsContainer = container.querySelector('.keepass-groups');
    const entriesContainer = container.querySelector('.keepass-entries');
    const detailsEmpty = container.querySelector('.keepass-details-empty');
    const detailsContent = container.querySelector('.keepass-details-content');
    const searchInput = container.querySelector('.keepass-search');

    // Password generator elements
    const genPassword = container.querySelector('.keepass-gen-password');
    const genCopy = container.querySelector('.keepass-gen-copy');
    const genLength = container.querySelector('.keepass-gen-length');
    const genLengthVal = container.querySelector('.keepass-gen-length-val');
    const genUpper = container.querySelector('.keepass-gen-upper');
    const genLower = container.querySelector('.keepass-gen-lower');
    const genNumbers = container.querySelector('.keepass-gen-numbers');
    const genSymbols = container.querySelector('.keepass-gen-symbols');
    const regenerateBtn = container.querySelector('.keepass-btn-regenerate');
    const closeGenBtn = container.querySelector('.keepass-btn-close-gen');

    // TOTP elements
    const totpSecret = container.querySelector('.keepass-totp-secret');
    const totpOutput = container.querySelector('.keepass-totp-output');
    const totpCode = container.querySelector('.keepass-totp-code');
    const totpTimerBar = container.querySelector('.keepass-totp-timer-bar');
    const totpRemaining = container.querySelector('.keepass-totp-remaining span');
    const totpCopy = container.querySelector('.keepass-totp-copy');
    const closeTotpBtn = container.querySelector('.keepass-btn-close-totp');

    let currentFile = null;

    // Event handlers
    openBtn.addEventListener('click', () => fileInput.click());

    fileInput.addEventListener('change', async (e) => {
        const file = e.target.files[0];
        if (!file) return;

        currentFile = file;
        dialogFilename.textContent = file.name;
        passwordInput.value = '';
        dialogError.style.display = 'none';

        lockedView.style.display = 'none';
        passwordDialog.style.display = 'flex';
        passwordInput.focus();
    });

    cancelBtn.addEventListener('click', () => {
        passwordDialog.style.display = 'none';
        lockedView.style.display = 'flex';
        currentFile = null;
        fileInput.value = '';
    });

    unlockBtn.addEventListener('click', () => attemptUnlock());
    passwordInput.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') attemptUnlock();
    });

    lockBtn.addEventListener('click', () => {
        if (db) {
            db.close();
            db = null;
        }
        selectedGroup = null;
        selectedEntry = null;
        mainView.style.display = 'none';
        lockedView.style.display = 'flex';
        currentFile = null;
        fileInput.value = '';
        win.setTitle('KeePass');
    });

    searchInput.addEventListener('input', () => {
        const query = searchInput.value.toLowerCase().trim();
        if (query) {
            searchEntries(query);
        } else if (selectedGroup) {
            renderEntries(selectedGroup);
        }
    });

    // Password generator handlers
    function updateGeneratedPassword() {
        const pw = generatePassword({
            length: parseInt(genLength.value),
            uppercase: genUpper.checked,
            lowercase: genLower.checked,
            numbers: genNumbers.checked,
            symbols: genSymbols.checked
        });
        genPassword.value = pw;
    }

    function showGeneratorDialog() {
        generatorDialog.style.display = 'flex';
        updateGeneratedPassword();
    }

    pwgenBtn.addEventListener('click', showGeneratorDialog);
    pwgenStandaloneBtn.addEventListener('click', showGeneratorDialog);

    genLength.addEventListener('input', () => {
        genLengthVal.textContent = genLength.value;
        updateGeneratedPassword();
    });

    [genUpper, genLower, genNumbers, genSymbols].forEach(el => {
        el.addEventListener('change', updateGeneratedPassword);
    });

    regenerateBtn.addEventListener('click', updateGeneratedPassword);

    genCopy.addEventListener('click', () => {
        navigator.clipboard.writeText(genPassword.value).then(() => {
            genCopy.textContent = 'Copied!';
            setTimeout(() => genCopy.textContent = 'Copy', 1500);
        });
    });

    closeGenBtn.addEventListener('click', () => {
        generatorDialog.style.display = 'none';
    });

    // TOTP handlers
    let currentTotpSecret = '';

    function updateTOTP() {
        if (!currentTotpSecret) return;

        const code = generateTOTP(currentTotpSecret);
        if (code) {
            totpCode.textContent = code;
            const remaining = getTOTPTimeRemaining();
            totpRemaining.textContent = remaining;
            totpTimerBar.style.width = `${(remaining / 30) * 100}%`;
        } else {
            totpCode.textContent = 'Invalid';
        }
    }

    function showTotpDialog() {
        totpDialog.style.display = 'flex';
        totpSecret.focus();
    }

    totpBtn.addEventListener('click', showTotpDialog);
    totpStandaloneBtn.addEventListener('click', showTotpDialog);

    totpSecret.addEventListener('input', () => {
        const secret = totpSecret.value.trim().replace(/\s/g, '');
        if (secret.length >= 8) {
            currentTotpSecret = secret;
            totpOutput.style.display = 'block';

            // Clear existing interval
            if (totpInterval) clearInterval(totpInterval);

            // Update immediately and then every second
            updateTOTP();
            totpInterval = setInterval(updateTOTP, 1000);
        } else {
            currentTotpSecret = '';
            totpOutput.style.display = 'none';
            if (totpInterval) clearInterval(totpInterval);
        }
    });

    totpCopy.addEventListener('click', () => {
        navigator.clipboard.writeText(totpCode.textContent).then(() => {
            totpCopy.textContent = 'Copied!';
            setTimeout(() => totpCopy.textContent = 'Copy Code', 1500);
        });
    });

    closeTotpBtn.addEventListener('click', () => {
        totpDialog.style.display = 'none';
        if (totpInterval) clearInterval(totpInterval);
        currentTotpSecret = '';
        totpSecret.value = '';
        totpOutput.style.display = 'none';
    });

    async function attemptUnlock() {
        const password = passwordInput.value;
        if (!password) {
            dialogError.textContent = 'Please enter a password';
            dialogError.style.display = 'block';
            return;
        }

        unlockBtn.disabled = true;
        unlockBtn.textContent = 'Unlocking...';
        dialogError.style.display = 'none';

        try {
            await initArgon2();

            const arrayBuffer = await currentFile.arrayBuffer();
            const credentials = new kdbxweb.Credentials(kdbxweb.ProtectedValue.fromString(password));

            db = await kdbxweb.Kdbx.load(arrayBuffer, credentials);

            passwordDialog.style.display = 'none';
            mainView.style.display = 'flex';
            win.setTitle(`KeePass - ${currentFile.name}`);

            renderGroups();

            // Select root group by default
            if (db.groups.length > 0) {
                selectGroup(db.groups[0]);
            }
        } catch (err) {
            console.error('Failed to open database:', err);
            dialogError.textContent = 'Invalid password or corrupted database';
            dialogError.style.display = 'block';
        } finally {
            unlockBtn.disabled = false;
            unlockBtn.textContent = 'Unlock';
        }
    }

    function renderGroups() {
        groupsContainer.innerHTML = '';

        function renderGroup(group, level = 0) {
            const el = document.createElement('div');
            el.className = 'keepass-group';
            el.style.paddingLeft = `${8 + level * 16}px`;
            el.dataset.uuid = group.uuid.toString();

            const icon = group.name === 'Recycle Bin' ? '&#128465;' : '&#128193;';
            el.innerHTML = `${icon} ${escapeHtml(group.name)}`;

            el.addEventListener('click', () => selectGroup(group));
            groupsContainer.appendChild(el);

            // Render subgroups
            for (const subgroup of group.groups) {
                renderGroup(subgroup, level + 1);
            }
        }

        for (const group of db.groups) {
            renderGroup(group);
        }
    }

    function selectGroup(group) {
        selectedGroup = group;
        selectedEntry = null;
        searchInput.value = '';

        // Update group selection UI
        container.querySelectorAll('.keepass-group').forEach(el => {
            el.classList.toggle('selected', el.dataset.uuid === group.uuid.toString());
        });

        renderEntries(group);
        showEntryDetails(null);
    }

    function renderEntries(group) {
        entriesContainer.innerHTML = '';

        const entries = group.entries || [];

        if (entries.length === 0) {
            entriesContainer.innerHTML = '<div class="keepass-no-entries">No entries in this group</div>';
            return;
        }

        for (const entry of entries) {
            renderEntryItem(entry);
        }
    }

    function searchEntries(query) {
        entriesContainer.innerHTML = '';
        selectedGroup = null;

        // Clear group selection
        container.querySelectorAll('.keepass-group').forEach(el => {
            el.classList.remove('selected');
        });

        const results = [];

        function searchInGroup(group) {
            for (const entry of (group.entries || [])) {
                const title = (entry.fields.get('Title') || '').toLowerCase();
                const username = (entry.fields.get('UserName') || '').toLowerCase();
                const url = (entry.fields.get('URL') || '').toLowerCase();

                if (title.includes(query) || username.includes(query) || url.includes(query)) {
                    results.push(entry);
                }
            }
            for (const subgroup of group.groups) {
                searchInGroup(subgroup);
            }
        }

        for (const group of db.groups) {
            searchInGroup(group);
        }

        if (results.length === 0) {
            entriesContainer.innerHTML = '<div class="keepass-no-entries">No matching entries</div>';
            return;
        }

        for (const entry of results) {
            renderEntryItem(entry);
        }
    }

    function renderEntryItem(entry) {
        const el = document.createElement('div');
        el.className = 'keepass-entry';
        el.dataset.uuid = entry.uuid.toString();

        const title = entry.fields.get('Title') || 'Untitled';
        const username = entry.fields.get('UserName') || '';

        el.innerHTML = `
            <div class="keepass-entry-icon">&#128273;</div>
            <div class="keepass-entry-info">
                <div class="keepass-entry-title">${escapeHtml(title)}</div>
                <div class="keepass-entry-username">${escapeHtml(username)}</div>
            </div>
        `;

        el.addEventListener('click', () => {
            selectedEntry = entry;
            container.querySelectorAll('.keepass-entry').forEach(e => {
                e.classList.toggle('selected', e.dataset.uuid === entry.uuid.toString());
            });
            showEntryDetails(entry);
        });

        entriesContainer.appendChild(el);
    }

    function showEntryDetails(entry) {
        if (!entry) {
            detailsEmpty.style.display = 'flex';
            detailsContent.style.display = 'none';
            return;
        }

        detailsEmpty.style.display = 'none';
        detailsContent.style.display = 'block';

        const title = entry.fields.get('Title') || 'Untitled';
        const username = entry.fields.get('UserName') || '';
        const password = entry.fields.get('Password');
        const url = entry.fields.get('URL') || '';
        const notes = entry.fields.get('Notes') || '';

        // Get custom fields (excluding standard ones)
        const standardFields = ['Title', 'UserName', 'Password', 'URL', 'Notes'];
        const customFields = [];
        entry.fields.forEach((value, key) => {
            if (!standardFields.includes(key)) {
                customFields.push({ key, value });
            }
        });

        let html = `
            <div class="keepass-detail-title">${escapeHtml(title)}</div>
            <div class="keepass-detail-fields">
                ${renderDetailField('Username', username)}
                ${renderPasswordField(password)}
                ${renderUrlField(url)}
                ${notes ? renderDetailField('Notes', notes, true) : ''}
                ${customFields.map(f => renderDetailField(f.key, getFieldValue(f.value))).join('')}
            </div>
        `;

        // Attachments
        if (entry.binaries && entry.binaries.size > 0) {
            html += '<div class="keepass-detail-attachments"><div class="keepass-detail-label">Attachments</div>';
            entry.binaries.forEach((data, name) => {
                html += `<div class="keepass-attachment">&#128206; ${escapeHtml(name)}</div>`;
            });
            html += '</div>';
        }

        detailsContent.innerHTML = html;

        // Add event listeners for copy buttons
        detailsContent.querySelectorAll('.keepass-copy-btn').forEach(btn => {
            btn.addEventListener('click', () => {
                const value = btn.dataset.value;
                navigator.clipboard.writeText(value).then(() => {
                    btn.textContent = 'Copied!';
                    setTimeout(() => btn.textContent = 'Copy', 1500);
                });
            });
        });

        // Password visibility toggle
        const showBtn = detailsContent.querySelector('.keepass-show-btn');
        if (showBtn && password) {
            showBtn.addEventListener('click', () => {
                const valueEl = showBtn.parentElement.querySelector('.keepass-detail-value');
                const isHidden = valueEl.classList.contains('password-hidden');
                if (isHidden) {
                    valueEl.textContent = getFieldValue(password);
                    valueEl.classList.remove('password-hidden');
                    showBtn.textContent = 'Hide';
                } else {
                    valueEl.textContent = '••••••••••••';
                    valueEl.classList.add('password-hidden');
                    showBtn.textContent = 'Show';
                }
            });
        }

        // URL open button
        const openUrlBtn = detailsContent.querySelector('.keepass-open-url-btn');
        if (openUrlBtn) {
            openUrlBtn.addEventListener('click', () => {
                window.open(url, '_blank');
            });
        }
    }

    function renderDetailField(label, value, multiline = false) {
        if (!value) return '';
        return `
            <div class="keepass-detail-row">
                <div class="keepass-detail-label">${escapeHtml(label)}</div>
                <div class="keepass-detail-value-wrapper">
                    <div class="keepass-detail-value ${multiline ? 'multiline' : ''}">${escapeHtml(value)}</div>
                    <button class="keepass-copy-btn" data-value="${escapeHtml(value)}">Copy</button>
                </div>
            </div>
        `;
    }

    function renderPasswordField(password) {
        if (!password) return '';
        const value = getFieldValue(password);
        return `
            <div class="keepass-detail-row">
                <div class="keepass-detail-label">Password</div>
                <div class="keepass-detail-value-wrapper">
                    <div class="keepass-detail-value password-hidden">••••••••••••</div>
                    <button class="keepass-show-btn">Show</button>
                    <button class="keepass-copy-btn" data-value="${escapeHtml(value)}">Copy</button>
                </div>
            </div>
        `;
    }

    function renderUrlField(url) {
        if (!url) return '';
        return `
            <div class="keepass-detail-row">
                <div class="keepass-detail-label">URL</div>
                <div class="keepass-detail-value-wrapper">
                    <div class="keepass-detail-value url">${escapeHtml(url)}</div>
                    <button class="keepass-open-url-btn">Open</button>
                    <button class="keepass-copy-btn" data-value="${escapeHtml(url)}">Copy</button>
                </div>
            </div>
        `;
    }

    function getFieldValue(field) {
        if (field && typeof field.getText === 'function') {
            return field.getText();
        }
        return field || '';
    }

    function escapeHtml(str) {
        if (!str) return '';
        const div = document.createElement('div');
        div.textContent = str;
        return div.innerHTML;
    }

    return { id, win, type: 'keepass' };
}
