import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { ContextMark, EnvTag } from './EnvTag';

describe('EnvTag', () => {
  it('is a short PROD badge by default', () => {
    render(<EnvTag env="production" />);
    const tag = screen.getByLabelText('Environment: production');
    expect(tag).toHaveTextContent(/^PROD$/);
    expect(tag).toHaveClass('env-tag', 'env-production');
    expect(tag).not.toHaveClass('chip');
  });

  it('staging is an amber dot named in its tooltip; local, the safe default, shows nothing', () => {
    const { container } = render(<EnvTag env="local" />);
    expect(container).toBeEmptyDOMElement();
    render(<EnvTag env="staging" />);
    const dot = screen.getByRole('img', { name: 'Environment: staging' });
    expect(dot).toHaveClass('env-dot', 'env-staging');
    expect(dot).toBeEmptyDOMElement();
  });

  it('the context chip head: engine logo, then PROD or the staging dot, nothing for local', () => {
    const { container, rerender } = render(<ContextMark env="local" kind="mysql" testId="env" />);
    expect(container.querySelector('.engine-logo.engine-mysql')).not.toBeNull();
    expect(screen.queryByTestId('env')).toBeNull();
    rerender(<ContextMark env="production" kind="postgres" testId="env" />);
    expect(screen.getByTestId('env')).toHaveTextContent('PROD');
    rerender(<ContextMark env="staging" kind="postgres" testId="env" />);
    expect(screen.getByTestId('env')).toHaveClass('env-dot');
  });

  // F50: the dialogs' environment chip is the same component, spelled out.
  it('chip variant names the environment with the same colours', () => {
    render(<EnvTag env="production" variant="chip" testId="env-chip" />);
    const chip = screen.getByTestId('env-chip');
    expect(chip).toHaveTextContent(/^Production$/);
    expect(chip).toHaveClass('env-tag', 'env-production', 'env-chip');
    expect(chip).toHaveAttribute('title', 'Environment: production');
  });
});
