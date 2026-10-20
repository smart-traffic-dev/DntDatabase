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
loader.config({ monaco });

// Monaco measures glyph widths once; a code font that finishes loading later would leave the
// cursor and selections misaligned until it measures again.
void document.fonts?.ready.then(() => monaco.editor.remeasureFonts());
