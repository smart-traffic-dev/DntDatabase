import { create } from 'zustand';

export type PaletteMode = 'commands' | 'objects';

/** The command palette / quick open overlay: one input, the mode shown as a chip. */
interface PaletteState {
  open: boolean;
  /** The mode it opens in: commands (Ctrl+Shift+P) or objects (Ctrl+P). */
  mode: PaletteMode;
  /** Bumped by every `show`, so a second Ctrl+P starts over with a fresh input. */
  nonce: number;
  show(mode: PaletteMode): void;
  hide(): void;
}

export const usePalette = create<PaletteState>()((set) => ({
  open: false,
  mode: 'objects',
  nonce: 0,
  show: (mode) => set((s) => ({ open: true, mode, nonce: s.nonce + 1 })),
  hide: () => set({ open: false }),
}));

export const showCommandPalette = () => usePalette.getState().show('commands');
export const showQuickOpen = () => usePalette.getState().show('objects');
