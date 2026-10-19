// Monaco and its editor worker are bundled by Vite: nothing is fetched from a CDN (CSP allows 'self' and blob: workers).
import { loader } from '@monaco-editor/react';
import * as monaco from 'monaco-editor';
import EditorWorker from 'monaco-editor/esm/vs/editor/editor.worker?worker';

self.MonacoEnvironment = { getWorker: () => new EditorWorker() };
loader.config({ monaco });
