/**
 * JackLink - Portable Computing Gateway
 * Copyright (c) 2025 Konstantin Zaremski
 * Licensed under the MIT License
 */

let settingsCount = 0;

export function create(desktop) {
    const id = `settings-${++settingsCount}`;

    const win = new WinBox({
        title: 'Settings',
        id: id,
        width: '500px',
        height: '400px',
        x: 'center',
        y: 'center',
        url: '/settings',
        class: ['no-full'],
        onclose: () => {
            desktop.removeWindow(id);
        }
    });

    return { id, win };
}
