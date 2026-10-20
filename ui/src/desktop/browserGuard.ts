/**
 * Second layer (after the WebView2 settings in src-tauri/src/desktop.rs) that keeps the
 * webview from acting like a browser: reload, print, view-source, DevTools, find, zoom,
 * new window, history navigation, link opening, file / link drops and the page context menu.
 *
 * It only ever calls `preventDefault()`, never `stopPropagation()`: the app's own shortcuts
 * (queryShortcuts.ts: Ctrl+S / Ctrl+T / Ctrl+W ...), Monaco and the menus still see every event.
 */

/** Text fields keep their editing keys, native menu and text drops. */
const FIELD = 'input, textarea, select, [contenteditable]:not([contenteditable="false"])';
/** Monaco handles find, go-to-line and its other keys itself. */
const EDITOR = '.monaco-editor';
/** Places where Ctrl+A selects something useful (cells, messages, editor). */
const SELECT_ALL = `${FIELD}, ${EDITOR}, [role="grid"], [role="log"], canvas`;

/** Ctrl+<key> blocked everywhere, Monaco included (browser actions with no editor meaning). */
const CTRL_ALWAYS = new Set(['r', 'p', 's', 'o', 'n', 't', 'w', 'j', '=', '+', '-', '_', '0']);
/** Ctrl+<key> that are browser actions on the chrome but editor commands inside Monaco. */
const CTRL_EDITOR = new Set(['f', 'g', 'h', 'u', 'd', 'l', 'e', 'k']);
/** Ctrl+Shift+<key> DevTools / browser windows. */
const CTRL_SHIFT_ALWAYS = new Set(['i', 'j', 'c', 'n', 't', 'w', 'b', 'o', 'delete']);
/** Keys blocked whatever the modifiers. */
const ALWAYS = new Set([
  'F5',
  'F7', // caret browsing
  'F12',
  'BrowserBack',
  'BrowserForward',
  'BrowserRefresh',
  'BrowserHome',
  'BrowserSearch',
  'BrowserFavorites',
  'BrowserStop',
]);
const ZOOM_CODES = new Set(['NumpadAdd', 'NumpadSubtract', 'Numpad0']);

const closest = (target: EventTarget | null, selector: string) =>
  target instanceof Element ? target.closest(selector) : null;

/** The layout-independent key: `KeyR` → `r`, else `e.key` lower-cased. */
function keyOf(e: KeyboardEvent) {
  return e.code?.startsWith('Key') ? e.code.slice(3).toLowerCase() : e.key.toLowerCase();
}

export function isBlockedKey(e: KeyboardEvent): boolean {
  if (e.isComposing) return false;
  if (ALWAYS.has(e.key)) return true;
  const ctrl = e.ctrlKey || e.metaKey;
  const inEditor = closest(e.target, EDITOR) !== null;
  const inField = closest(e.target, FIELD) !== null;
  if (e.altKey && !ctrl) return e.key === 'ArrowLeft' || e.key === 'ArrowRight' || e.key === 'Home';
  if (e.key === 'F3') return !inEditor;
  if (e.key === 'Backspace') return !ctrl && !e.altKey && !inField && !inEditor;
  if (!ctrl || e.altKey) return false;
  if (ZOOM_CODES.has(e.code)) return true;
  const key = keyOf(e);
  if (e.shiftKey && CTRL_SHIFT_ALWAYS.has(key)) return true;
  if (e.key === 'F5') return true;
  if (CTRL_ALWAYS.has(key) || CTRL_ALWAYS.has(e.key)) return true;
  if (CTRL_EDITOR.has(key)) return !inEditor;
  if (key === 'a') return closest(e.target, SELECT_ALL) === null;
  return false;
}

/** F5 / Ctrl+F5 / Ctrl+R / Ctrl+Shift+R / the Refresh key. */
export function isReloadKey(e: KeyboardEvent): boolean {
  if (e.key === 'F5' || e.key === 'BrowserRefresh') return true;
  return (e.ctrlKey || e.metaKey) && !e.altKey && keyOf(e) === 'r';
}

/** Bubble phase: the app's menus (tree, tabs, Monaco) run first and may check `defaultPrevented`. */
function onContextMenu(e: MouseEvent) {
  if (closest(e.target, FIELD)) return;
  // Selected read-only text (messages, DDL, dialog text) keeps the native menu for Copy.
  const selected = (e.target instanceof Node ? e.target.ownerDocument : document)?.getSelection()?.toString() ?? '';
  if (selected.trim()) return;
  e.preventDefault();
}

/** Mouse buttons 3 / 4 navigate back / forward; button 1 starts autoscroll. */
function onMouseButton(e: MouseEvent) {
  if (e.button === 3 || e.button === 4) e.preventDefault();
  else if (e.type === 'mousedown' && e.button === 1 && !closest(e.target, EDITOR)) e.preventDefault();
}

/** Links never open a new window (Ctrl / Shift / middle click). */
function onLinkClick(e: MouseEvent) {
  if (!closest(e.target, 'a[href]')) return;
  if (e.type === 'auxclick' ? e.button === 1 : e.ctrlKey || e.shiftKey || e.metaKey) e.preventDefault();
}

function onWheel(e: WheelEvent) {
  if (e.ctrlKey) e.preventDefault();
}

/** Links and images are not dragged out as URLs (unless marked `draggable="true"`). */
function onDragStart(e: DragEvent) {
  const el = closest(e.target, 'a[href], img');
  if (el && el.getAttribute('draggable') !== 'true') e.preventDefault();
}

/** Bubble phase: a drop zone that accepted the drag already called preventDefault. */
function onDragOver(e: DragEvent) {
  if (e.defaultPrevented || closest(e.target, FIELD)) return;
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = 'none';
}

/** Capture phase: nothing dropped navigates the webview; app drop handlers still run. */
function onDrop(e: DragEvent) {
  if (!closest(e.target, FIELD)) e.preventDefault();
}

export interface BrowserGuardOptions {
  /** Let F5 / Ctrl+R reload the page (the Vite dev server: `import.meta.env.DEV`). */
  allowReload?: boolean;
}

/** Installs the guard on `win`; returns the uninstaller. */
export function installBrowserGuard(win: Window = window, { allowReload = import.meta.env.DEV }: BrowserGuardOptions = {}): () => void {
  const onKeyDown = (e: KeyboardEvent) => {
    if (allowReload && isReloadKey(e)) return;
    if (isBlockedKey(e)) e.preventDefault();
  };
  const listeners: [string, (e: never) => void, AddEventListenerOptions][] = [
    ['keydown', onKeyDown, { capture: true }],
    ['contextmenu', onContextMenu, { capture: false }],
    ['mousedown', onMouseButton, { capture: true }],
    ['mouseup', onMouseButton, { capture: true }],
    ['auxclick', onMouseButton, { capture: true }],
    ['click', onLinkClick, { capture: true }],
    ['auxclick', onLinkClick, { capture: true }],
    ['wheel', onWheel, { capture: true, passive: false }],
    ['dragstart', onDragStart, { capture: true }],
    ['dragover', onDragOver, { capture: false }],
    ['drop', onDrop, { capture: true }],
  ];
  for (const [type, fn, opts] of listeners) win.addEventListener(type, fn as EventListener, opts);
  return () => {
    for (const [type, fn, opts] of listeners) win.removeEventListener(type, fn as EventListener, opts);
  };
}
