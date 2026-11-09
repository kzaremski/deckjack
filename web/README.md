# Deckjack web shell

The browser client: windows, a dock and the command palette, plus the design
system every surface in the project is styled from. It builds to a plain
static bundle in `dist/`, which `rust-embed` bakes into the `deckjack` binary.

Nothing here talks to a machine directly. Every action is dispatched as a
typed command on the one bus, and the browser is the only front end.

## Layout

```
src/tokens/           the design system — one source for every surface
  types.ts            the token vocabulary (data only; no DOM, no Vue)
  foundation.ts       type, space, radius, motion, density, breakpoints
  themes/night.ts     Night: the default dark set
  themes/circuit.ts   Circuit: the daylight set
  css.ts              emits CSS custom properties (pure string building)
  registry.ts         the theme list and the manifest generators read
  runtime.ts          applies the theme, and reads tokens back for canvas
src/shell/
  context.ts          the composition root: every store, wired once
  geometry.ts         window math: snap zones, clamping, resize
  viewport.ts         breakpoints, desktop/compact mode, the Fold's hinge
  keyboard.ts         the chord table and the Keyboard Lock request
  shell-commands.ts   the commands whose backend is the shell itself
src/stores/
  bus.ts              the command bus client
  commands.ts         the command registry and the palette's search
  windows.ts          the window manager's mirror of server state
  apps.ts             the app contract: min size, compact and wide layouts
  ui.ts               palette, switcher, notices, confirmations
src/components/       the shell: desktop, compact, window chrome, palette
src/protocol.ts       wire types — to be replaced by generated output
scripts/emit-tokens.ts  xterm, VS Code/code-server and mermaid themes
```

## Commands

```
npm run dev        Vite on :5273, proxying /api, /ws and /login to the core
npm run build      typecheck, then the static bundle in dist/
npm run typecheck  vue-tsc over the app and the scripts
npm run tokens     writes generated/ for every non-DOM target
```

`npm run tokens` needs Node 22.6 or newer and no extra dependency: it runs the
TypeScript token modules directly. It is the only sanctioned way for a
non-DOM target to learn a color.

## The rules this code follows

- **One token set.** Color, type, spacing, radius, motion and density live in
  `src/tokens`. CSS custom properties, the canvas surfaces (which read the
  resolved properties at runtime) and the generated xterm, Monaco,
  code-server and mermaid themes all come from it. A component that hard-codes
  a color is a bug.
- **Everything is a command.** A dock button dispatches `app.open.files`; it
  does not call the window store. That is what keeps the palette, the
  keyboard, the CLI and an agent on the same path.
- **Risk is declared, not inferred.** The palette shows the level the backend
  reported, and destructive commands confirm.
- **Compact by default, touch always.** Density is one token; a coarse pointer
  raises every hit area to 44px regardless of it.
- **Skeletons, never spinners,** and no layout shift when content lands.
- **Dependency injection over ambient reads.** Stores take their clock,
  storage, matchMedia and socket as arguments; `src/shell/context.ts` is the
  only file that knows it is in a browser.
