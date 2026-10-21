import { createContext, useContext, useLayoutEffect, useState, type ReactNode } from 'react';

export type Theme = 'light' | 'dark' | 'system';
/** Control and row sizes (tokens.css component layer); compact is the default. */
export type Density = 'compact' | 'comfortable';

interface ThemeState {
  theme: Theme;
  /** Resolved appearance: follows the OS when the setting is `system`. */
  dark: boolean;
  setTheme: (theme: Theme) => void;
  density: Density;
  setDensity: (density: Density) => void;
}

const ThemeContext = createContext<ThemeState>({
  theme: 'system',
  dark: false,
  setTheme: () => {},
  density: 'compact',
  setDensity: () => {},
});

function readDensity(): Density {
  try {
    return localStorage.getItem('density') === 'comfortable' ? 'comfortable' : 'compact';
  } catch {
    return 'compact';
  }
}

function readTheme(): Theme {
  try {
    const stored = localStorage.getItem('theme');
    return stored === 'light' || stored === 'dark' || stored === 'system' ? stored : 'system';
  } catch {
    return 'system';
  }
}

function systemPrefersDark(): boolean {
  return typeof window !== 'undefined' && window.matchMedia?.('(prefers-color-scheme: dark)').matches === true;
}

function resolve(theme: Theme): boolean {
  return theme === 'dark' || (theme === 'system' && systemPrefersDark());
}

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [theme, setThemeState] = useState<Theme>(readTheme);
  const [dark, setDark] = useState(() => resolve(theme));

  useLayoutEffect(() => {
    const apply = () => {
      const next = resolve(theme);
      setDark(next);
      document.documentElement.classList.toggle('dark', next);
    };
    apply();
    if (theme !== 'system') return;
    const mq = window.matchMedia?.('(prefers-color-scheme: dark)');
    if (!mq) return;
    mq.addEventListener('change', apply);
    return () => mq.removeEventListener('change', apply);
  }, [theme]);

  const [density, setDensityState] = useState<Density>(readDensity);

  useLayoutEffect(() => {
    const root = document.documentElement;
    if (density === 'compact') root.removeAttribute('data-density');
    else root.setAttribute('data-density', density);
  }, [density]);

  const setTheme = (next: Theme) => {
    try {
      localStorage.setItem('theme', next);
    } catch {
      // Storage can be unavailable in private windows; the in-memory choice still applies.
    }
    setThemeState(next);
  };

  const setDensity = (next: Density) => {
    try {
      localStorage.setItem('density', next);
    } catch {
      // As for the theme: the in-memory choice still applies.
    }
    setDensityState(next);
  };

  return (
    <ThemeContext.Provider value={{ theme, dark, setTheme, density, setDensity }}>{children}</ThemeContext.Provider>
  );
}

export function useTheme() {
  return useContext(ThemeContext);
}
