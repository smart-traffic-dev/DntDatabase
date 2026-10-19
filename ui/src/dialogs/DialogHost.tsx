import { useState } from 'react';
import { api, isHostKeyMismatch, isHostKeyUnknown, isPasswordRequired, toAppError } from '../api';
import type { ConnectionConfig, SecretsInput } from '../bindings';
import { useConnections } from '../store/connections';
import { useDialogs, type PendingPrompt } from '../store/dialogs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { ConnectionDialog } from './ConnectionDialog';
import { HostKeyDialog } from './HostKeyDialog';
import { PasswordPrompt, type SecretNeed } from './PasswordPrompt';
import { ConfirmDialog, NameDialog } from './SimpleDialogs';

/** Secrets the stored connection lacks (or, in Ask mode, always needs). */
function neededSecrets(cfg: ConnectionConfig | undefined, hasDb: boolean, hasSsh: boolean): SecretNeed[] {
  if (!cfg) return ['db_password'];
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
      if (isHostKeyUnknown(err) || isHostKeyMismatch(err)) {
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
      needs={neededSecrets(stored?.config, !!stored?.has_db_password, !!stored?.has_ssh_secret)}
      error={error}
      onSubmit={(secrets) => {
        setError(null);
        void attempt(secrets);
      }}
      onCancel={closePrompt}
    />
  );
}

/** Mounts the dialogs the connection tree and toolbar ask for (`useDialogs`). */
export function DialogHost() {
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
      {prompt && <PromptHost key={`${prompt.kind}:${prompt.connectionId}:${prompt.error.fingerprint ?? ''}:${prompt.error.stage ?? ''}`} prompt={prompt} />}
    </>
  );
}
