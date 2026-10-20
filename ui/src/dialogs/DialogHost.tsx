import { useEffect, useState } from 'react';
import { api, isCancelled, isHostKeyMismatch, isHostKeyUnknown, isPasswordRequired, toAppError } from '../api';
import type { AppError, ConnectionConfig, SecretsInput } from '../bindings';
import { useConnections } from '../store/connections';
import { useDialogs, type PendingPrompt, type Question } from '../store/dialogs';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { ConnectionDialog } from './ConnectionDialog';
import { ExportDialog } from './ExportDialog';
import { HostKeyDialog } from './HostKeyDialog';
import { ImportDialog } from './ImportDialog';
import { PasswordPrompt, type SecretNeed } from './PasswordPrompt';
import { SettingsDialog } from './SettingsDialog';
import { ConfirmDialog, NameDialog } from './SimpleDialogs';
import { AppCloseDialog } from './AppCloseDialog';
import { TxCloseDialog } from './TxCloseDialog';

/** Secrets the stored connection lacks (or, in Ask mode, always needs). */
function neededSecrets(cfg: ConnectionConfig | undefined, hasDb: boolean, hasSsh: boolean, error: AppError): SecretNeed[] {
  if (!cfg) return ['db_password'];
  // Asked while opening the tunnel: the SSH key is encrypted and its passphrase is not stored.
  if (error.stage === 'open_tunnel' && cfg.ssh?.auth === 'key' && cfg.secret_mode === 'saved') {
    return ['ssh_key_passphrase'];
  }
  const ask = cfg.secret_mode === 'ask';
  const needs: SecretNeed[] = [];
  if (ask || !hasDb) needs.push('db_password');
  if (cfg.ssh && (ask || !hasSsh)) needs.push(cfg.ssh.auth === 'password' ? 'ssh_password' : 'ssh_key_passphrase');
  return needs.length ? needs : ['db_password'];
}

/** A connection whose open attempt succeeded: remember its info and expand it. */
async function finishOpen(id: string, info: Awaited<ReturnType<typeof api.openConnection>>['info']) {
  useConnections.getState().setInfo(id, info);
  // Rebuild the node (engine chip, version) from the now-open connection, then list its databases.
  await useTree.getState().reloadStored();
  useTree.getState().expand(connNodeId(id));
}

function PromptHost({ prompt }: { prompt: PendingPrompt }) {
  const { showPrompt, closePrompt } = useDialogs.getState();
  const stored = useConnections((s) => s.summaries.get(prompt.connectionId));
  const [error, setError] = useState<string | null>(null);
  const id = prompt.connectionId;

  /** Opens the connection; routes a failure to the right prompt again. */
  const attempt = async (secrets: SecretsInput | null) => {
    try {
      const opened = await api.openConnection(id, secrets);
      closePrompt();
      await finishOpen(id, opened.info);
    } catch (e) {
      const err = toAppError(e);
      if (isCancelled(err)) {
        // Superseded by a close or another open: nothing to report.
        closePrompt();
      } else if (isHostKeyUnknown(err) || isHostKeyMismatch(err)) {
        showPrompt({ kind: 'hostKey', connectionId: id, error: err });
      } else if (isPasswordRequired(err) && prompt.kind === 'hostKey') {
        showPrompt({ kind: 'password', connectionId: id, error: err });
      } else if (prompt.kind === 'password') {
        setError(err.message);
      } else {
        closePrompt();
        useTree.setState({ error: err });
      }
    }
  };

  if (prompt.kind === 'hostKey') {
    const e = prompt.error;
    const ssh = stored?.config.ssh;
    return (
      <HostKeyDialog
        host={e.host ?? ssh?.host ?? ''}
        port={e.port ?? ssh?.port ?? 22}
        fingerprint={e.fingerprint ?? ''}
        keyOpenssh={e.key_openssh ?? ''}
        mismatchMessage={isHostKeyMismatch(e) ? e.message : undefined}
        onDone={(trusted) => {
          if (trusted) void attempt(null);
          else closePrompt();
        }}
      />
    );
  }

  return (
    <PasswordPrompt
      connectionName={stored?.config.name ?? 'connection'}
      needs={neededSecrets(stored?.config, !!stored?.has_db_password, !!stored?.has_ssh_secret, prompt.error)}
      error={error}
      onSubmit={(secrets) => {
        setError(null);
        void attempt(secrets);
      }}
      onCancel={closePrompt}
    />
  );
}

/** Shows the status message set by a dialog (import summary) for a few seconds. */
function Toast() {
  const notice = useDialogs((s) => s.notice);
  useEffect(() => {
    if (!notice) return;
    const t = setTimeout(() => useDialogs.getState().setNotice(null), 6000);
    return () => clearTimeout(t);
  }, [notice]);
  if (!notice) return null;
  return (
    <div className="toast" role="status">
      {notice}
    </div>
  );
}

/** Spec §7: an internal error is a toast that points at the log folder. */
function InternalErrorToast() {
  const error = useDialogs((s) => s.internalError);
  const [openFailed, setOpenFailed] = useState(false);
  if (!error) return null;
  const openLogs = async () => {
    try {
      await api.openLogFolder();
      setOpenFailed(false);
    } catch {
      setOpenFailed(true);
    }
  };
  return (
    <div className="toast error" role="alert" aria-label="Internal error">
      <span>{error.message}</span>
      {openFailed && <span className="hint">The log folder could not be opened.</span>}
      <button type="button" className="btn small" onClick={() => void openLogs()}>
        Open log folder
      </button>
      <button
        type="button"
        className="btn small"
        onClick={() => {
          setOpenFailed(false);
          useDialogs.getState().dismissInternal();
        }}
      >
        Dismiss
      </button>
    </div>
  );
}

/** A question a store awaits (closing a query tab, quitting the app). */
function QuestionHost({ question }: { question: Question }) {
  if (question.kind === 'txClose') {
    return <TxCloseDialog title={question.title} onSettle={question.settle} onDone={question.done} />;
  }
  if (question.kind === 'discard') {
    return (
      <ConfirmDialog
        title="Unsaved changes"
        message={`Discard unsaved changes to ${question.file}?`}
        confirmLabel="Discard"
        onClose={() => question.done(false)}
        onConfirm={async () => question.done(true)}
      />
    );
  }
  if (question.kind === 'saveFailed') {
    return (
      <ConfirmDialog
        title="Unsaved tabs"
        message={`Could not save ${question.count} tab(s).`}
        confirmLabel="Quit anyway"
        onClose={() => question.done(false)}
        onConfirm={async () => question.done(true)}
      />
    );
  }
  return <AppCloseDialog tabs={question.tabs} onQuit={question.quit} onCancel={question.done} />;
}

/** Mounts the dialogs the connection tree and toolbar ask for (`useDialogs`). */
export function DialogHost() {
  const question = useDialogs((s) => s.question);
  const request = useDialogs((s) => s.request);
  const prompt = useDialogs((s) => s.prompt);
  const closeRequest = useDialogs((s) => s.closeRequest);
  const summaries = useConnections((s) => s.summaries);
  const groups = useConnections((s) => s.groups);

  const reload = () => useTree.getState().reloadStored();

  let dialog = null;
  if (request?.kind === 'newConnection') {
    dialog = (
      <ConnectionDialog
        mode="create"
        onClose={(saved) => {
          closeRequest();
          if (saved) void reload();
        }}
      />
    );
  } else if (request?.kind === 'editConnection') {
    const summary = summaries.get(request.id);
    if (summary) {
      const id = request.id;
      dialog = (
        <ConnectionDialog
          key={id}
          mode="edit"
          initial={summary.config}
          hasDbPassword={summary.has_db_password}
          hasSshSecret={summary.has_ssh_secret}
          onClose={(saved) => {
            closeRequest();
            if (!saved) return;
            // The backend closed the runtime on update: show the connection as closed.
            void (async () => {
              try {
                if (useConnections.getState().info.has(id)) await useTree.getState().closeConnection(id);
                await reload();
              } catch (e) {
                useTree.setState({ error: toAppError(e) });
              }
            })();
          }}
        />
      );
    }
  } else if (request?.kind === 'newGroup') {
    const parentId = request.parentId;
    dialog = (
      <NameDialog
        title="New group"
        label="Group name"
        submitLabel="Create"
        onClose={closeRequest}
        onSubmit={async (name) => {
          await api.createGroup(name, parentId);
          closeRequest();
          await reload();
        }}
      />
    );
  } else if (request?.kind === 'renameGroup') {
    const id = request.id;
    dialog = (
      <NameDialog
        title="Rename group"
        label="Group name"
        submitLabel="Rename"
        initial={groups.find((g) => g.id === id)?.name}
        onClose={closeRequest}
        onSubmit={async (name) => {
          await api.renameGroup(id, name);
          closeRequest();
          await reload();
        }}
      />
    );
  } else if (request?.kind === 'export') {
    dialog = <ExportDialog preselected={request.preselected} onClose={closeRequest} />;
  } else if (request?.kind === 'import') {
    dialog = <ImportDialog onClose={closeRequest} />;
  } else if (request?.kind === 'settings') {
    dialog = <SettingsDialog onClose={closeRequest} />;
  } else if (request?.kind === 'deleteNode') {
    const { node, label } = request;
    const isGroup = node.type === 'group';
    dialog = (
      <ConfirmDialog
        title={isGroup ? 'Delete group' : 'Delete connection'}
        message={
          isGroup
            ? `Delete the group "${label}"? Its subgroups are deleted; their connections are kept and move to the top level.`
            : `Delete the connection "${label}"? Its saved passwords are removed too.`
        }
        confirmLabel="Delete"
        onClose={closeRequest}
        onConfirm={async () => {
          if (node.type === 'group') {
            await api.deleteGroup(node.id);
          } else {
            await api.deleteConnection(node.id);
            useConnections.getState().clearInfo(node.id);
            // The objects of a deleted connection cannot be shown any more.
            useTabs.getState().closeConnection(node.id);
          }
          closeRequest();
          await reload();
        }}
      />
    );
  }

  return (
    <>
      {dialog}
      <Toast />
      <InternalErrorToast />
      {prompt && <PromptHost key={`${prompt.kind}:${prompt.connectionId}:${prompt.error.fingerprint ?? ''}:${prompt.error.stage ?? ''}`} prompt={prompt} />}
      {question && <QuestionHost question={question} />}
    </>
  );
}
