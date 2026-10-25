import type { Dispatch } from 'react';
import type { WizardAction, WizardState } from '../importState';

export interface StepProps {
  state: WizardState;
  dispatch: Dispatch<WizardAction>;
}
