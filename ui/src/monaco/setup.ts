// Monaco and its editor worker are bundled by Vite: nothing is fetched from a CDN (CSP allows 'self' and blob: workers).
// Only the editor core with its contributions (suggest, find, folding…) and the three SQL
// tokenizers are bundled, not the full `monaco-editor` entry with every language and the
// TypeScript / CSS / HTML / JSON services.
import { loader } from '@monaco-editor/react';
import 'monaco-editor/esm/vs/editor/edcore.main.js';
import * as monaco from 'monaco-editor/esm/vs/editor/editor.api';
import 'monaco-editor/esm/vs/basic-languages/sql/sql.contribution';
import 'monaco-editor/esm/vs/basic-languages/mysql/mysql.contribution';
import 'monaco-editor/esm/vs/basic-languages/pgsql/pgsql.contribution';
import EditorWorker from 'monaco-editor/esm/vs/editor/editor.worker?worker';
import './monaco.css';

self.MonacoEnvironment = { getWorker: () => new EditorWorker() };

/** Monaco's `CancellationError` (name and message `Canceled`): a cancelled timeout, not a bug. */
export function isMonacoCancellation(reason: unknown): boolean {
  return reason instanceof Error && reason.name === 'Canceled' && reason.message === 'Canceled';
}

// Inline suggestions debounce with a cancellable timeout that Monaco never awaits: typing on
// rejects it with `Canceled`, which the WebView console reports as an uncaught error. Drop only
// those. (In `vite dev` its client forwards it to the terminal first; that stays, dev only.)
if (typeof window !== 'undefined') {
  window.addEventListener('unhandledrejection', (e) => {
    if (isMonacoCancellation(e.reason)) e.preventDefault();
  });
}
loader.config({ monaco });

// Monaco measures glyph widths once; a code font that finishes loading later would leave the
// cursor and selections misaligned until it measures again.
void document.fonts?.ready.then(() => monaco.editor.remeasureFonts());
