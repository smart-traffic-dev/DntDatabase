import { create } from 'zustand';

/** The command palette / quick open overlay: one input; a leading `>` means commands. */
interface PaletteState {
  open: boolean;
  /** The input's first text: `>` (commands, Ctrl+Shift+P) or empty (objects, Ctrl+P). */
  initial: string;
  /** Bumped by every `show`, so a second Ctrl+P starts over with a fresh input. */
  nonce: number;
  show(mode: 'commands' | 'objects'): void;
  hide(): void;
}

export const usePalette = create<PaletteState>()((set) => ({
  open: false,
  initial: '',
  nonce: 0,
  show: (mode) => set((s) => ({ open: true, initial: mode === 'commands' ? '>' : '', nonce: s.nonce + 1 })),
  hide: () => set({ open: false }),
}));

export const showCommandPalette = () => usePalette.getState().show('commands');
export const showQuickOpen = () => usePalette.getState().show('objects');
