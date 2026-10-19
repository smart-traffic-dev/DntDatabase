import { Component, type ErrorInfo, type ReactNode } from 'react';

interface Props {
  /** Names the panel in the fallback ("Sidebar", "Tab content"). */
  label: string;
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/** Keeps a crash inside one panel from taking the rest of the app down. */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Only the message and component stack: no user data is rendered by the shell.
    console.error(`[${this.props.label}] render failed:`, error.message, info.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="error-fallback" role="alert">
        <strong>{this.props.label} failed to render</strong>
        <p>{error.message}</p>
        <button type="button" className="btn" onClick={() => this.setState({ error: null })}>
          Try again
        </button>
      </div>
    );
  }
}
