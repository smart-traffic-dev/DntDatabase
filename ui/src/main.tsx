import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './monaco/setup';
import App from './App';
import { installBrowserGuard } from './desktop/browserGuard';
import { ThemeProvider } from './theme/ThemeProvider';
import './theme/tokens.css';
import './theme.css';
import './desktop/desktop.css';

// A desktop app, not a web page: no reload / print / find / zoom / navigation / page menu.
installBrowserGuard(window);

const root = document.getElementById('root');
if (!root) throw new Error('missing #root');
createRoot(root).render(
  <StrictMode>
    <ThemeProvider>
      <App />
    </ThemeProvider>
  </StrictMode>,
);
