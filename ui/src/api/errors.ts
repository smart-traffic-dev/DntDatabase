import type { AppError, StageTiming } from '../bindings';

/** An `AppError` payload of a failed command, as a throwable. */
export class AppErrorEx extends Error implements AppError {
  readonly kind: string;
  readonly detail: string | null;
  readonly stage: string | null;
  readonly timings: StageTiming[] | null;
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
    this.timings = error.timings;
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
      timings: this.timings,
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
    timings: null,
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

/** The backend asks the user for a password (kind `password_required`). */
export function isPasswordRequired(e: AppError): boolean {
  return e.kind === 'password_required';
}

/** The connection has no runtime (closed, or never opened): reconnecting fixes it. */
export function notOpenError(): AppError {
  return { ...internal('This connection is not open.'), kind: 'not_open', retryable: true };
}

/** Errors that a "Reconnect" (open the connection again, then retry) can fix. */
export function needsReconnect(e: AppError): boolean {
  return e.kind === 'not_open' || e.kind === 'connection_lost';
}

/** `cancelled` means the user (or a newer request) stopped the call: never an error to show. */
export function isCancelled(e: AppError): boolean {
  return e.kind === 'cancelled';
}

/** Labels of the connect stages (`ConnectStage`) and SSH stages (`SshStage`). */
export const STAGE_LABELS: Record<string, string> = {
  resolve_secrets: 'Secrets',
  open_tunnel: 'SSH tunnel',
  connect_db: 'Connect',
  detect_server: 'Detect server',
  execute: 'Query',
  connect: 'SSH connect',
  auth: 'SSH authentication',
  channel: 'SSH channel',
  host_key_mismatch: 'SSH host key',
};

export const stageLabel = (stage: string) => STAGE_LABELS[stage] ?? stage;

/** The SSH server presented a key that differs from the one in known_hosts. */
export function isHostKeyMismatch(e: AppError): boolean {
  return e.kind === 'ssh' && e.stage === 'host_key_mismatch';
}

/** The SSH server's host key is not trusted yet. */
export function isHostKeyUnknown(e: AppError): boolean {
  return e.kind === 'ssh_host_key_unknown';
}
