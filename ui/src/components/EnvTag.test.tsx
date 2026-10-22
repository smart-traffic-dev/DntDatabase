import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { EnvTag } from './EnvTag';

describe('EnvTag', () => {
  it('is a short PROD / STG / LCL badge by default', () => {
    render(<EnvTag env="production" />);
    const tag = screen.getByLabelText('Environment: production');
    expect(tag).toHaveTextContent(/^PROD$/);
    expect(tag).toHaveClass('env-tag', 'env-production');
    expect(tag).not.toHaveClass('chip');
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
