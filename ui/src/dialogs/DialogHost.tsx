import { useState } from 'react';
import { api, isCancelled, isHostKeyMismatch, isHostKeyUnknown, isPasswordRequired, toAppError } from '../api';
import type { AppError, ConnectionConfig, SecretsInput } from '../bindings';
import { confirmDiscardConnection } from '../components/tabActions';
import { plural } from '../lib/plural';
import { useConnections } from '../store/connections';
import { useDialogs, type PendingPrompt, type Question } from '../store/dialogs';
import { useRunPrompt } from '../store/runPrompt';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId, groupNodeId } from '../store/treeModel';
import { ObjectActionDialog, stagedOnConnection } from './ObjectActionDialog';
import { TableDesignDialog } from './TableDesignDialog';
import { DdlEditDialog } from './DdlEditDialog';
import { AboutDialog } from './AboutDialog';
import { UpdateDialog } from './UpdateDialog';
import { ConnectionDialog } from './ConnectionDialog';
import { ExportDialog } from './ExportDialog';
import { ResultExportDialog } from './ResultExportDialog';
import { HostKeyDialog } from './HostKeyDialog';
import { ImportDialog } from './ImportDialog';
import { PasswordPrompt, type SecretNeed } from './PasswordPrompt';
import { SettingsDialog } from './SettingsDialog';
import { RunBlockedDialog } from './RunBlockedDialog';
import { RunConfirmDialog } from './RunConfirmDialog';
import { ConfirmDialog, NameDialog } from './SimpleDialogs';
import { AppCloseDialog } from './AppCloseDialog';
import { TxCloseDialog } from './TxCloseDialog';
import { AddFavoriteDialog } from './AddFavoriteDialog';
import { SnippetsDialog } from './SnippetsDialog';
import { ToastHost } from './ToastHost';
import { ImportWizard } from '../transfer/import/ImportWizard';
// Registers "Run again" of import jobs and dry runs (R5).
import '../transfer/import/rerunImport';
import { SeedDialog } from '../transfer/seed/SeedDialog';
import '../transfer/seed/rerunSeed';
import { DumpDialog } from '../transfer/backup/DumpDialog';
import { RestoreDialog } from '../transfer/backup/RestoreDialog';
// Registers "Run again" of dumps, tool dumps and restores (R5).
import '../transfer/backup/rerunBackup';

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
  const [error, setError] = useState<{ message: string; hint: string | null } | null>(null);
  const [busy, setBusy] = useState(false);
  const id = prompt.connectionId;

  /** Opens the connection; routes a failure to the right prompt again. */
  const attempt = async (secrets: SecretsInput | null) => {
    setBusy(true);
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
        setError({ message: err.message, hint: err.hint ?? null });
      } else {
        closePrompt();
        useTree.setState({ error: err });
      }
    } finally {
      setBusy(false);
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

  const cfg = stored?.config;
  const target = cfg ? `${cfg.username ? `${cfg.username}@` : ''}${cfg.host}:${cfg.port}${cfg.ssh ? ` · via ${cfg.ssh.host}` : ''}` : null;
  return (
    <PasswordPrompt
      connectionName={stored?.config.name ?? 'connection'}
      target={target}
      busy={busy}
      needs={neededSecrets(stored?.config, !!stored?.has_db_password, !!stored?.has_ssh_secret, prompt.error)}
      error={error?.message ?? null}
      errorHint={error?.hint ?? null}
      onSubmit={(secrets) => {
        setError(null);
        void attempt(secrets);
      }}
      onCancel={closePrompt}
    />
  );
}

/** A question a store awaits (closing a query tab, quitting the app). */
function QuestionHost({ question }: { question: Question }) {
  if (question.kind === 'txClose') {
    return (
      <TxCloseDialog
        title={question.title}
        connectionName={question.connectionName}
        production={question.envTag === 'production'}
        onSettle={question.settle}
        onDone={question.done}
      />
    );
  }
  if (question.kind === 'discard') {
    return (
      <ConfirmDialog
        title={`Discard changes to ${question.file}?`}
        message="Your changes are lost if you discard them."
        confirmLabel="Discard"
        onClose={() => question.done(false)}
        onConfirm={async () => question.done(true)}
      />
    );
  }
  if (question.kind === 'discardEdits') {
    return (
      <ConfirmDialog
        title={`Discard ${plural(question.count, 'unsaved change')}?`}
        message="The staged edits in the grid are dropped; nothing was sent to the server."
        confirmLabel="Discard"
        onClose={() => question.done(false)}
        onConfirm={async () => question.done(true)}
      />
    );
  }
  if (question.kind === 'discardForImport') {
    return (
      <ConfirmDialog
        title="Discard unsaved changes and import?"
        message={`The import replaces connections that have ${plural(question.count, 'unsaved change')} in open tabs.`}
        confirmLabel="Discard and import"
        cancelLabel="Cancel import"
        onClose={() => question.done(false)}
        onConfirm={async () => question.done(true)}
      />
    );
  }
  if (question.kind === 'stopRun') {
    return (
      <ConfirmDialog
        title="Stop the running query?"
        message={`${question.title} is still running. Closing the tab stops the query.`}
        confirmLabel="Stop and close"
        cancelLabel="Keep running"
        onClose={() => question.done(false)}
        onConfirm={async () => question.done(true)}
      />
    );
  }
  if (question.kind === 'saveFailed') {
    return (
      <ConfirmDialog
        title="Quit without saving?"
        message={`Could not save ${plural(question.count, 'tab')}. Their changes are lost if you quit.`}
        confirmLabel="Quit anyway"
        onClose={() => question.done(false)}
        onConfirm={async () => question.done(true)}
      />
    );
  }
  if (question.kind === 'jobsRunning') {
    const one = question.count === 1;
    return (
      <ConfirmDialog
        title="Stop background jobs and quit?"
        message={`${plural(question.count, 'background job')} ${one ? 'is' : 'are'} running. Quitting stops ${one ? 'it' : 'them'}.`}
        confirmLabel="Stop jobs and quit"
        busyLabel="Stopping…"
        onClose={question.done}
        onConfirm={question.quit}
      />
    );
  }
  return <AppCloseDialog tabs={question.tabs} onQuit={question.quit} onCancel={question.done} />;
}

/** The Production confirmation / read-only notice a run waits for (`useRunPrompt`). */
function RunPromptHost() {
  const pending = useRunPrompt((s) => s.pending);
  const settle = useRunPrompt((s) => s.settle);
  if (!pending) return null;
  return pending.kind === 'confirm' ? (
    <RunConfirmDialog key={pending.plan.plan_id} plan={pending.plan} onDone={settle} />
  ) : (
    <RunBlockedDialog plan={pending.plan} onClose={() => settle(false)} />
  );
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
  if (request?.kind === 'objectAction') {
    dialog = <ObjectActionDialog request={request} onClose={closeRequest} />;
  } else if (request?.kind === 'tableDesign') {
    dialog = <TableDesignDialog key={request.tabId} tabId={request.tabId} onClose={closeRequest} />;
  } else if (request?.kind === 'ddlEdit') {
    dialog = <DdlEditDialog key={request.tabId} tabId={request.tabId} onClose={closeRequest} />;
  } else if (request?.kind === 'newConnection') {
    dialog = (
      <ConnectionDialog
        mode="create"
        onClose={(saved) => {
          closeRequest();
          if (!saved) return;
          // Reveal the new row: open its group and select it.
          void (async () => {
            await reload();
            const tree = useTree.getState();
            // Open every ancestor group so the row is visible.
            const parents = new Map(useConnections.getState().groups.map((g) => [g.id, g.parent_id]));
            const seen = new Set<string>();
            for (let g = saved.group_id; g && !seen.has(g); g = parents.get(g) ?? null) {
              seen.add(g);
              tree.expand(groupNodeId(g));
            }
            tree.select(connNodeId(saved.id));
          })();
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
  } else if (request?.kind === 'exportResult') {
    dialog = <ResultExportDialog request={request.request} onClose={closeRequest} />;
  } else if (request?.kind === 'import') {
    dialog = <ImportDialog onClose={closeRequest} />;
  } else if (request?.kind === 'settings') {
    // Keyed by tab: F1 while Settings is open switches to the Shortcuts tab.
    dialog = <SettingsDialog key={request.tab ?? 'appearance'} initialTab={request.tab} onClose={closeRequest} />;
  } else if (request?.kind === 'addFavorite') {
    dialog = (
      <AddFavoriteDialog
        sql={request.sql}
        name={request.name}
        connectionId={request.connectionId}
        database={request.database}
        fromSelection={request.fromSelection}
        onClose={(saved) => {
          closeRequest();
          if (saved) useDialogs.getState().setNotice(`Added "${saved.name}" to favorites.`, 'success');
        }}
      />
    );
  } else if (request?.kind === 'snippets') {
    dialog = <SnippetsDialog onClose={closeRequest} />;
  } else if (request?.kind === 'about') {
    dialog = <AboutDialog onClose={closeRequest} />;
  } else if (request?.kind === 'update') {
    dialog = <UpdateDialog found={request.found} onClose={closeRequest} />;
  } else if (request?.kind === 'importData') {
    dialog = (
      <ImportWizard
        key={`${request.connectionId}:${request.table?.database ?? ''}.${request.table?.schema ?? ''}.${request.table?.name ?? ''}:${request.params ? 'rerun' : ''}`}
        connectionId={request.connectionId}
        table={request.table}
        params={request.params}
        onClose={closeRequest}
      />
    );
  } else if (request?.kind === 'seedData') {
    dialog = (
      <SeedDialog
        key={`${request.connectionId}:${request.table.database}.${request.table.schema ?? ''}.${request.table.name}:${request.rerun ?? ''}`}
        connectionId={request.connectionId}
        table={request.table}
        params={request.params}
        onClose={closeRequest}
      />
    );
  } else if (request?.kind === 'dump') {
    dialog = (
      <DumpDialog
        key={`${request.connectionId}:${request.database}.${request.schema ?? ''}.${request.table ?? ''}:${request.rerun ?? ''}`}
        connectionId={request.connectionId}
        database={request.database}
        schema={request.schema}
        table={request.table}
        params={request.params}
        onClose={closeRequest}
      />
    );
  } else if (request?.kind === 'restore') {
    dialog = (
      <RestoreDialog
        key={`${request.connectionId}:${request.database ?? ''}:${request.rerun ?? ''}`}
        connectionId={request.connectionId}
        database={request.database}
        params={request.params}
        onClose={closeRequest}
      />
    );
  } else if (request?.kind === 'deleteNode') {
    const { node, label } = request;
    const isGroup = node.type === 'group';
    const openTabs = isGroup ? 0 : useTabs.getState().tabs.filter((t) => t.kind === 'object' && t.connectionId === node.id).length;
    dialog = (
      <ConfirmDialog
        title={isGroup ? `Delete group ${label}?` : `Delete connection ${label}?`}
        details={
          <ul className="confirm-details">
            {isGroup ? (
              <>
                <li>Its subgroups are deleted too</li>
                <li>Their connections are kept and move to the top level</li>
              </>
            ) : (
              <>
                <li>Its saved passwords are removed from the keychain</li>
                {openTabs > 0 && <li>{plural(openTabs, 'open object tab')} will close</li>}
                {stagedOnConnection(node.id) && <li className="bad">Unsaved data edits in its tabs are lost</li>}
              </>
            )}
          </ul>
        }
        confirmLabel={isGroup ? 'Delete group' : 'Delete connection'}
        busyLabel="Deleting…"
        onClose={closeRequest}
        onConfirm={async () => {
          if (node.type === 'group') {
            await api.deleteGroup(node.id);
          } else {
            // Staged data edits of its tabs go with it: ask before anything is deleted.
            if (!(await confirmDiscardConnection(node.id))) return closeRequest();
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
      <ToastHost />
      {prompt && <PromptHost key={`${prompt.kind}:${prompt.connectionId}:${prompt.error.fingerprint ?? ''}:${prompt.error.stage ?? ''}`} prompt={prompt} />}
      {question && <QuestionHost question={question} />}
      <RunPromptHost />
    </>
  );
}
