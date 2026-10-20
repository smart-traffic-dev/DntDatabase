import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './monaco/setup';
import App from './App';
import { ThemeProvider } from './theme/ThemeProvider';
import './theme/tokens.css';
import './theme.css';

const root = document.getElementById('root');
if (!root) throw new Error('missing #root');
createRoot(root).render(
  <StrictMode>
    <ThemeProvider>
      <App />
    </ThemeProvider>
  </StrictMode>,
);
