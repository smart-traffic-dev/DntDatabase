import type { AppError } from '../bindings';

/** An `AppError` payload of a failed command, as a throwable. */
export class AppErrorEx extends Error implements AppError {
  readonly kind: string;
  readonly detail: string | null;
  readonly stage: string | null;
  readonly server_code: string | null;
  readonly sqlstate: string | null;
  readonly position: number | null;
  readonly retryable: boolean;
  readonly hint: string | null;
  readonly fingerprint: string | null;
  readonly key_openssh: string | null;
  readonly host: string | null;
  readonly port: number | null;
  readonly field: string | null;

  constructor(error: AppError) {
    super(error.message);
    this.name = 'AppErrorEx';
    this.kind = error.kind;
    this.detail = error.detail;
    this.stage = error.stage;
    this.server_code = error.server_code;
    this.sqlstate = error.sqlstate;
    this.position = error.position;
    this.retryable = error.retryable;
    this.hint = error.hint;
    this.fingerprint = error.fingerprint;
    this.key_openssh = error.key_openssh;
    this.host = error.host;
    this.port = error.port;
    this.field = error.field;
  }

  /** The plain payload (what the stores keep). */
  toAppError(): AppError {
    return {
      kind: this.kind,
      message: this.message,
      detail: this.detail,
      stage: this.stage,
      server_code: this.server_code,
      sqlstate: this.sqlstate,
      position: this.position,
      retryable: this.retryable,
      hint: this.hint,
      fingerprint: this.fingerprint,
      key_openssh: this.key_openssh,
      host: this.host,
      port: this.port,
      field: this.field,
    };
  }
}

/** Anything thrown while calling the backend, as an `AppError` payload. */
export function toAppError(e: unknown): AppError {
  if (e instanceof AppErrorEx) return e.toAppError();
  // A real AppError payload (`kind` and `message` strings); missing optional fields become null.
  if (typeof e === 'object' && e !== null) {
    const o = e as Record<string, unknown>;
    if (typeof o.kind === 'string' && typeof o.message === 'string') {
      return { ...internal(o.message), ...(e as Partial<AppError>), kind: o.kind, message: o.message };
    }
  }
  // Tauri rejects with a plain string for deserialization or unknown-command failures.
  return internal(e instanceof Error ? e.message : String(e));
}

function internal(message: string): AppError {
  return {
    kind: 'internal',
    message,
    detail: null,
    stage: null,
    server_code: null,
    sqlstate: null,
    position: null,
    retryable: false,
    hint: null,
    fingerprint: null,
    key_openssh: null,
    host: null,
    port: null,
    field: null,
  };
}

/** The backend asks the user for a password (`secret` / `password_required`). */
export function isPasswordRequired(e: AppError): boolean {
  return e.kind === 'secret' && e.message === 'password_required';
}

/** The SSH server's host key is not trusted yet. */
export function isHostKeyUnknown(e: AppError): boolean {
  return e.kind === 'ssh_host_key_unknown';
}
